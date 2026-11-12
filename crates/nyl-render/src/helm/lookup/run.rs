//! Running Helm against the lookup API.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};

use regex::Regex;

use super::api::{discovery_fingerprint, ApiCapabilities, ApiState};
use super::builtin::builtin_kinds_of;
use super::capabilities::{
    chart_crd_kinds, declares_api_version, helm_default_api_versions, parse_declared_api_version,
};
use super::model::{split_api_version, KindInfo};
use super::{
    declare_api_version_fix, discovery_dependency_name, lookup_error, request_dependency_name, LookupHandle,
    ServerReport, API_VERSION_UNDECLARED_CODE, HELM_UNSUPPORTED_CODE, KIND_UNDECLARED_CODE, SERVER_FAILED_CODE,
};
use crate::render::cache::LookupFingerprint;
use crate::resources::ResourceScope;
use crate::{NylError, Result};

/// Everything Helm needs to render against the lookup API besides its command.
pub struct LookupRender<'a> {
    /// The Release's lookups, and where they are answered from.
    pub handle: &'a LookupHandle,
    pub release_name: &'a str,
    /// The chart directory, whose `crds/` declare API versions.
    pub chart: &'a Path,
    /// The declared Kubernetes version, which discovery reports.
    pub kube_version: &'a str,
    /// The declared API versions, which discovery lists.
    pub api_versions: &'a [String],
}

/// Run the `helm template` that `build` makes with Helm's cluster replaced by
/// the lookup API, and return what the render looked up: every object request
/// Helm made and the discovery it saw, by dependency name.
///
/// `--dry-run=server` is what makes Helm's `lookup` work, and Helm then ignores
/// `--kube-version` and `--api-versions`, which stay on the command for the
/// cache key. `--take-ownership` keeps Helm from refusing an object it finds
/// without its own ownership metadata, which the objects a snapshot holds
/// lack; in a dry run it changes no output. Helm gets its kubeconfig only from
/// the temporary file, never from the environment.
pub fn render_with_lookup(
    build: &dyn Fn() -> Command,
    run: &LookupRender<'_>,
) -> Result<(Output, BTreeMap<String, LookupFingerprint>)> {
    let scope = run.handle.scope()?;
    let store = run.handle.config().store(&scope);

    // What discovery lists is fixed by the declaration and the chart, never by what the
    // lookup source holds, so `.Capabilities` is the same on every render of a Release.
    let mut group_versions = (*helm_default_api_versions()?).clone();
    let mut declared = Vec::new();
    for entry in run.api_versions {
        let (api_version, kind) = parse_declared_api_version(entry);
        group_versions.insert(api_version.clone());
        declared.extend(kind.map(|kind| declared_kind(&api_version, &kind, None)));
    }
    let crd_kinds = chart_crd_kinds(run.chart)?;
    group_versions.extend(crd_kinds.iter().map(KindInfo::api_version));
    // Helm maps every object it renders, and every kind a chart looks up, through discovery:
    // the built-in kinds of the declared API versions, the kinds declared as
    // `group/version/Kind`, and the kinds the chart's own CRDs define. Where they list one
    // kind twice, the CRD's scope wins over the built-in table's, which wins over a declared
    // entry's guess.
    let kinds = declared
        .into_iter()
        .chain(builtin_kinds_of(&group_versions))
        .chain(crd_kinds)
        .collect::<Vec<_>>();
    let capabilities = ApiCapabilities::new(run.kube_version, group_versions.clone(), kinds)
        .map_err(|error| NylError::config(format!("Cannot declare the Kubernetes version to Helm: {error}")))?;
    attempt(build(), run, &capabilities, &group_versions, &scope, &store)
}

fn attempt(
    mut command: Command,
    run: &LookupRender<'_>,
    capabilities: &ApiCapabilities,
    declared: &BTreeSet<String>,
    scope: &super::ReleaseScope,
    store: &Arc<dyn super::ObjectStore>,
) -> Result<(Output, BTreeMap<String, LookupFingerprint>)> {
    let token = random_token()?;
    let state = Arc::new(ApiState::new(capabilities.clone(), store.clone(), token.clone()));
    let server = super::ApiServer::start(state, token)?;
    let mut kubeconfig = tempfile::Builder::new()
        .prefix("nyl-lookup-")
        .suffix(".kubeconfig")
        .tempfile()?;
    kubeconfig.write_all(server.kubeconfig().as_bytes())?;
    kubeconfig.flush()?;

    command
        .arg("--dry-run=server")
        .arg("--disable-openapi-validation")
        .arg("--take-ownership")
        .arg("--kubeconfig")
        .arg(kubeconfig.path());
    isolate_kube_environment(&mut command, kubeconfig.path());

    let output = command
        .output()
        .map_err(|error| NylError::Process(format!("Failed to execute helm: {error}")));
    let report = server.finish();
    let output = output?;

    if !output.status.success() || !report.failures.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(failure(run, &report, &stderr, declared));
    }
    let mut observed = report
        .observations
        .into_iter()
        .map(|observation| {
            (
                request_dependency_name(scope, &observation.request),
                observation.fingerprint,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let discovery = discovery_fingerprint(store.as_ref()).map_err(|error| lookup_error(SERVER_FAILED_CODE, error))?;
    observed.insert(discovery_dependency_name(scope), LookupFingerprint::Plain(discovery));
    Ok((output, observed))
}

/// A kind Helm must map: the scope of a built-in kind is fixed, and any other
/// kind (a custom resource) is namespaced unless `namespaced` says otherwise.
fn declared_kind(api_version: &str, kind: &str, namespaced: Option<bool>) -> KindInfo {
    let (group, _) = split_api_version(api_version);
    let namespaced = namespaced.unwrap_or_else(|| {
        crate::kubernetes::builtin_scope(group, kind).is_none_or(|scope| scope == ResourceScope::Namespaced)
    });
    KindInfo::new(api_version, kind, namespaced)
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| NylError::process(format!("Failed to generate a lookup token: {error}")))?;
    Ok(hex::encode(bytes))
}

/// Keep ambient credentials, contexts, and exec plugins out of Helm: it reads
/// only the kubeconfig Nyl wrote.
fn isolate_kube_environment(command: &mut Command, kubeconfig: &Path) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().to_ascii_uppercase().starts_with("HELM_KUBE") {
            command.env_remove(name);
        }
    }
    command.env("KUBECONFIG", kubeconfig);
    // Loopback needs no proxy, whatever the environment says.
    command.env("NO_PROXY", "127.0.0.1,localhost");
    command.env("no_proxy", "127.0.0.1,localhost");
}

fn failure(run: &LookupRender<'_>, report: &ServerReport, stderr: &str, declared: &BTreeSet<String>) -> NylError {
    if let Some(first) = report.failures.first() {
        let code = first.code.unwrap_or(SERVER_FAILED_CODE);
        let message = match first.code {
            Some(_) => first.message.clone(),
            None => format!(
                "the lookup API for the Helm release {:?} could not answer a request: {}",
                run.release_name, first.message
            ),
        };
        return lookup_error(code, message);
    }
    if stderr.contains("unknown flag: --take-ownership") {
        return lookup_error(
            HELM_UNSUPPORTED_CODE,
            format!(
                "the Helm release {:?} declares lookup, which renders with `helm template --dry-run=server \
                 --take-ownership` and needs Helm 3.17 or later; install the Helm version the project pins \
                 (mise install helm). Helm said: {}",
                run.release_name,
                stderr.trim()
            ),
        );
    }
    match unmapped_kind(stderr) {
        Some((api_version, kind)) if declares_api_version(declared, &api_version) => lookup_error(
            KIND_UNDECLARED_CODE,
            format!(
                "the Helm release {:?} renders or looks up {kind} of API version {api_version}, which is declared but \
                 whose kind a render with lookup cannot map; declare {api_version}/{kind} in the Cluster's \
                 spec.kubernetes.apiVersions (or pass it to --kube-api-versions for a render without a target), or \
                 ship the CustomResourceDefinition in the chart's crds/. Helm said: {}",
                run.release_name,
                stderr.trim()
            ),
        ),
        Some((api_version, kind)) => lookup_error(
            API_VERSION_UNDECLARED_CODE,
            format!(
                "the Helm release {:?} renders {kind} of API version {api_version}, which is not declared, so a render \
                 with lookup cannot map it; {}",
                run.release_name,
                declare_api_version_fix(&api_version)
            ),
        ),
        None => NylError::HelmChart(format!("helm template failed: {stderr}")),
    }
}

/// The kind Helm could not map, as its error names it.
fn unmapped_kind(stderr: &str) -> Option<(String, String)> {
    static NO_MATCH: OnceLock<Regex> = OnceLock::new();
    let pattern = NO_MATCH.get_or_init(|| {
        Regex::new(r#"no matches for kind "(?P<kind>[^"]+)" in version "(?P<version>[^"]+)""#)
            .expect("the pattern is valid")
    });
    let captures = pattern.captures(stderr)?;
    Some((captures["version"].to_owned(), captures["kind"].to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helm::lookup::{LookupConfig, LookupHandle};

    fn run(handle: &LookupHandle) -> LookupRender<'_> {
        LookupRender {
            handle,
            release_name: "cilium",
            chart: Path::new("."),
            kube_version: "1.30.0",
            api_versions: &[],
        }
    }

    const NO_MATCH: &str = "Error: unable to build kubernetes objects from release manifest: resource mapping not found for name: \"x\" namespace: \"\" from \"\": no matches for kind \"Widget\" in version \"foo.example.com/v1\"\nensure CRDs are installed first";

    #[test]
    fn test_an_undeclared_api_version_is_reported_with_its_fix() {
        let handle = LookupHandle::new(LookupConfig::default(), None, None);
        let error = failure(
            &run(&handle),
            &ServerReport::default(),
            NO_MATCH,
            &BTreeSet::from(["v1".to_owned()]),
        )
        .to_string();
        assert!(error.starts_with("NYL-HELM-LOOKUP-API-VERSION-UNDECLARED:"), "{error}");
        assert!(
            error.contains("spec.kubernetes.apiVersions") && error.contains("foo.example.com/v1"),
            "{error}"
        );
    }

    #[test]
    fn test_an_unlisted_kind_of_a_declared_api_version_is_reported_with_its_kind_stderr_and_fix() {
        let handle = LookupHandle::new(LookupConfig::default(), None, None);
        let error = failure(
            &run(&handle),
            &ServerReport::default(),
            NO_MATCH,
            &BTreeSet::from(["foo.example.com/v1".to_owned()]),
        )
        .to_string();
        assert!(error.starts_with("NYL-HELM-LOOKUP-KIND-UNDECLARED:"), "{error}");
        assert!(
            error.contains("declare foo.example.com/v1/Widget") && error.contains("no matches for kind"),
            "the fix names the kind and Helm's own words follow: {error}"
        );
    }

    #[test]
    fn test_a_helm_that_does_not_take_the_flags_is_reported_with_its_fix() {
        let handle = LookupHandle::new(LookupConfig::default(), None, None);
        let error = failure(
            &run(&handle),
            &ServerReport::default(),
            "Error: unknown flag: --take-ownership",
            &BTreeSet::new(),
        )
        .to_string();
        assert!(error.starts_with("NYL-HELM-LOOKUP-HELM-UNSUPPORTED:"), "{error}");
        assert!(error.contains("Helm 3.17"), "{error}");
    }

    #[test]
    fn test_a_failed_read_is_reported_under_its_own_code() {
        let handle = LookupHandle::new(LookupConfig::default(), None, None);
        let report = ServerReport {
            observations: Vec::new(),
            failures: vec![crate::helm::lookup::snapshot_unavailable(
                "the publication cannot be read",
            )],
        };
        let error = failure(&run(&handle), &report, "Error: lookup failed", &BTreeSet::new());
        assert!(
            error.to_string().starts_with("NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE:"),
            "{error}"
        );
    }
}
