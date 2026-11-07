//! The project the scenario runs: the end-to-end example of the Orchestration
//! guide (an image build, an OpenTofu configuration, and a publication that
//! consumes both), rendered from one template per variant.
//!
//! The files under `fixture/project` hold `%PLACEHOLDERS%` for what the
//! harness decides (Git URLs, the kubeconfig context, the registry) and for
//! what the variant decides (the reconciler's declaration and what the
//! publication unit admits). Everything else is the project as a user would
//! write it, including the `{{ templates }}` Nyl renders itself.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};

/// The environment, target, and units the fixture declares.
pub const ENVIRONMENT: &str = "dev";
pub const UNIT_DATABASE: &str = "database";
pub const UNIT_IMAGE: &str = "web-image";
pub const UNIT_PUBLICATION: &str = "kubernetes";
/// The Namespace and the workload objects the Release renders.
pub const NAMESPACE: &str = "web";
pub const DEPLOYMENT: &str = "web";
pub const SERVICE: &str = "web";
/// The branch the publication unit pushes the rendered tree to, and the prefix
/// of the target on it.
pub const PUBLICATION_BRANCH: &str = "deploy";
pub const PUBLICATION_PREFIX: &str = "dev";
/// The Argo CD namespace and the Applications the target generates.
pub const ARGOCD_NAMESPACE: &str = "argocd";
pub const CATALOG_APPLICATION: &str = "dev-catalog";
pub const WORKLOAD_APPLICATION: &str = "dev-web";
/// Where the catalog Application lives in the published tree.
pub const CATALOG_MANIFEST: &str = "dev/_nyl/catalog/application.argoproj.io/argocd/dev-catalog.yaml";

/// The files of the project: path in the repository, and the template.
const FILES: &[(&str, &str)] = &[
    (".gitignore", include_str!("../fixture/project/.gitignore")),
    ("nyl/nyl.toml", include_str!("../fixture/project/nyl/nyl.toml")),
    (
        "nyl/platform.yaml",
        include_str!("../fixture/project/nyl/platform.yaml"),
    ),
    ("nyl/units.yaml", include_str!("../fixture/project/nyl/units.yaml")),
    ("nyl/web.yaml", include_str!("../fixture/project/nyl/web.yaml")),
    (
        "applications/web/release.yaml",
        include_str!("../fixture/project/applications/web/release.yaml"),
    ),
    (
        "services/web/Dockerfile",
        include_str!("../fixture/project/services/web/Dockerfile"),
    ),
    (
        "services/web/index.html",
        include_str!("../fixture/project/services/web/index.html"),
    ),
    (
        "infra/database/main.tf",
        include_str!("../fixture/project/infra/database/main.tf"),
    ),
    (
        "infra/database/.terraform.lock.hcl",
        include_str!("../fixture/project/infra/database/.terraform.lock.hcl"),
    ),
];

/// What a reconciler contributes to the project: its control resources, the
/// lines of the DeploymentTarget that choose it, and what the publication unit
/// admits to read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declarations {
    /// Documents appended to `platform.yaml`, each starting with `---`; empty
    /// when the reconciler needs no resource of its own.
    pub instance_documents: String,
    /// The Cluster the DeploymentTarget deploys to. Argo CD's targets use the
    /// fixture's `dev` Cluster, whose destination is the in-cluster alias;
    /// another reconciler may declare a Cluster of its own in
    /// `instance_documents` that names the same kind cluster by a server URL
    /// Nyl can verify against the kubeconfig context.
    pub target_cluster: String,
    /// Lines inside `DeploymentTarget.spec` that select the reconciler.
    pub target_fields: String,
    /// The environment variables the publication unit admits, in order: the
    /// credentials for reading the reconciler and the cluster.
    pub publication_env: Vec<String>,
}

/// What the harness decides about one run.
#[derive(Clone, Debug)]
pub struct Params {
    /// The registry as the host's Docker and the kind nodes name it,
    /// `localhost:<port>`.
    pub registry_host: String,
    /// What Argo CD reads: the Git server's address on the kind network.
    pub repo_url: String,
    /// What Nyl pushes to: the Git server's published loopback port.
    pub publish_url: String,
    /// An absolute directory outside the checkout for OpenTofu's local state,
    /// with forward slashes.
    pub state_dir: String,
    /// The kubeconfig context of the cluster, `Cluster.spec.live.context`.
    pub kube_context: String,
    pub kube_version: String,
    /// Which revision of the project to render: it changes the page the image
    /// serves and the host OpenTofu outputs, so a new revision changes both
    /// inputs of the publication.
    pub revision: u32,
    /// The environment variables the image build admits for its Docker client.
    pub docker_env: Vec<String>,
}

/// The host OpenTofu outputs for `revision` in the fixture's environment.
pub fn expected_database_host(revision: u32) -> String {
    format!("{ENVIRONMENT}.r{revision}.db.internal")
}

/// One file of the rendered project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedFile {
    /// Relative to the repository root, with `/` separators.
    pub path: &'static str,
    pub content: String,
}

/// Render the project for `params` and the reconciler's `declarations`.
pub fn render(params: &Params, declarations: &Declarations) -> Result<Vec<RenderedFile>> {
    let values = values(params, declarations)?;
    let mut used = BTreeSet::new();
    let mut files = Vec::new();
    for (path, template) in FILES {
        // A checkout with CRLF endings (Windows without the repository's
        // .gitattributes) must render the same project.
        let content = substitute(&normalize_newlines(template), &values, &mut used)
            .map_err(|error| error.context(format!("render fixture file {path}")))?;
        files.push(RenderedFile { path, content });
    }
    let unused = values.keys().filter(|key| !used.contains(*key)).collect::<Vec<_>>();
    if !unused.is_empty() {
        return Err(Error::new(format!(
            "no fixture file uses the placeholder(s) {unused:?}; a typo in a placeholder name?"
        )));
    }
    Ok(files)
}

/// The `kubernetes:` capabilities of every Cluster the fixture declares: what
/// the Release renders against, since rendering never asks the cluster.
pub fn cluster_capabilities(kube_version: &str) -> String {
    format!(
        "kubernetes:\n  kubeVersion: {kube_version}\n  apiVersions: [v1, apps/v1, v1/Namespace, v1/Service, apps/v1/Deployment]\n  clusterScopedKinds: [core/Namespace]\n"
    )
}

/// A Cluster document, starting with `---`, for a destination reached through
/// the kubeconfig context `context`.
pub fn cluster_document(name: &str, server: &str, context: &str, kube_version: &str) -> String {
    let capabilities = cluster_capabilities(kube_version)
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect::<String>();
    format!(
        "---\napiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  destination:\n    server: {server}\n  live: {{context: {context}}}\n{capabilities}"
    )
}

/// The template with LF line endings.
fn normalize_newlines(template: &str) -> String {
    template.replace("\r\n", "\n")
}

fn values(params: &Params, declarations: &Declarations) -> Result<BTreeMap<&'static str, String>> {
    let json = |list: &[String]| serde_json::to_string(list).map_err(|error| Error::new(error.to_string()));
    Ok(BTreeMap::from([
        ("REPO_URL", params.repo_url.clone()),
        ("PUBLISH_URL", params.publish_url.clone()),
        ("KUBE_CONTEXT", params.kube_context.clone()),
        ("CLUSTER_CAPABILITIES", cluster_capabilities(&params.kube_version)),
        ("REGISTRY_HOST", params.registry_host.clone()),
        ("STATE_DIR", params.state_dir.clone()),
        ("REVISION", params.revision.to_string()),
        ("DOCKER_PASSTHROUGH", json(&params.docker_env)?),
        ("PUBLICATION_ENV", json(&declarations.publication_env)?),
        ("TARGET_CLUSTER", declarations.target_cluster.clone()),
        ("INSTANCE_DOCUMENTS", declarations.instance_documents.clone()),
        ("TARGET_RECONCILER_FIELDS", declarations.target_fields.clone()),
    ]))
}

/// Replace `%KEY%` in `template`.
///
/// A line that holds only a placeholder is replaced by the value's lines,
/// each indented like the placeholder, and by nothing when the value is empty;
/// that is how a variant adds whole documents or fields. Any other occurrence
/// is replaced in place. A placeholder with no value is an error, so a typo
/// never reaches the cluster as literal `%TEXT%`.
pub fn substitute(
    template: &str,
    values: &BTreeMap<&'static str, String>,
    used: &mut BTreeSet<&'static str>,
) -> Result<String> {
    let mut output = String::with_capacity(template.len());
    for line in template.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim();
        let whole_line = values.iter().find(|(key, _)| trimmed == format!("%{key}%"));
        if let Some((key, value)) = whole_line {
            used.insert(*key);
            let indent = &content[..content.len() - content.trim_start().len()];
            for value_line in value.lines() {
                if value_line.is_empty() {
                    output.push('\n');
                } else {
                    output.push_str(indent);
                    output.push_str(value_line);
                    output.push('\n');
                }
            }
            continue;
        }
        let mut replaced = line.to_owned();
        for (key, value) in values {
            let marker = format!("%{key}%");
            if replaced.contains(&marker) {
                used.insert(*key);
                replaced = replaced.replace(&marker, value);
            }
        }
        output.push_str(&replaced);
    }
    let unresolved = unresolved_placeholders(&output);
    if !unresolved.is_empty() {
        return Err(Error::new(format!("unresolved placeholder(s) {unresolved:?}")));
    }
    Ok(output)
}

/// The `%UPPER_CASE%` markers left in `text`.
fn unresolved_placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                let is_placeholder = name.starts_with(|character: char| character.is_ascii_uppercase())
                    && name.chars().all(|character| {
                        character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
                    });
                if is_placeholder {
                    found.push(format!("%{name}%"));
                    rest = &after[end + 1..];
                } else {
                    // Not a placeholder: the closing '%' may open the next one.
                    rest = after;
                }
            }
            None => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            registry_host: "localhost:5001".to_owned(),
            repo_url: "http://172.18.0.4:8080/git/shop.git".to_owned(),
            publish_url: "http://127.0.0.1:40123/git/shop.git".to_owned(),
            state_dir: "/work/tofu".to_owned(),
            kube_context: "e2e-admin".to_owned(),
            kube_version: "1.36.4".to_owned(),
            revision: 1,
            docker_env: vec!["DOCKER_HOST".to_owned()],
        }
    }

    fn file<'a>(files: &'a [RenderedFile], path: &str) -> &'a str {
        &files.iter().find(|file| file.path == path).unwrap().content
    }

    #[test]
    fn test_a_placeholder_line_takes_the_values_lines_at_its_own_indent() {
        let values = BTreeMap::from([("FIELDS", "reconciler: {argocd: {}}\nother: 1\n".to_owned())]);
        let rendered = substitute(
            "spec:\n  clusterRef: x\n  %FIELDS%\n  next: y\n",
            &values,
            &mut BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "spec:\n  clusterRef: x\n  reconciler: {argocd: {}}\n  other: 1\n  next: y\n"
        );
    }

    #[test]
    fn test_an_empty_value_removes_its_placeholder_line() {
        let values = BTreeMap::from([("DOCS", String::new())]);
        let rendered = substitute("a: 1\n%DOCS%\n---\nb: 2\n", &values, &mut BTreeSet::new()).unwrap();
        assert_eq!(rendered, "a: 1\n---\nb: 2\n");
    }

    #[test]
    fn test_inline_placeholders_leave_nyls_own_templates_alone() {
        let values = BTreeMap::from([("DIR", "/work/tofu".to_owned())]);
        let rendered = substitute(
            "backend: {path: '%DIR%/{{ environment.name }}.tfstate'}\nname: '${ target.metadata.name }'\n",
            &values,
            &mut BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "backend: {path: '/work/tofu/{{ environment.name }}.tfstate'}\nname: '${ target.metadata.name }'\n"
        );
    }

    #[test]
    fn test_a_placeholder_without_a_value_is_an_error_naming_it() {
        let error = substitute("image: %REGISTRY%/web\n", &BTreeMap::new(), &mut BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("%REGISTRY%"), "{error}");
    }

    #[test]
    fn test_percent_signs_that_are_not_placeholders_are_left_alone() {
        let rendered = substitute(
            "rate: 50% of 100%\nx: '%s %d'\n",
            &BTreeMap::new(),
            &mut BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(rendered, "rate: 50% of 100%\nx: '%s %d'\n");
    }

    #[test]
    fn test_a_value_that_no_file_uses_is_an_error() {
        // A typo in a placeholder name must not silently drop a declaration.
        let values = values(&params(), &Declarations::default()).unwrap();
        let mut used = BTreeSet::new();
        let _ = substitute("only %REVISION%\n", &values, &mut used).unwrap();
        assert!(values.keys().any(|key| !used.contains(key)));
    }

    #[test]
    fn test_every_file_under_the_fixture_directory_is_rendered() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture/project");
        let mut on_disk = Vec::new();
        let mut pending = vec![root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(&root).unwrap();
                    on_disk.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut embedded = FILES.iter().map(|(path, _)| (*path).to_owned()).collect::<Vec<_>>();
        embedded.sort();
        assert_eq!(on_disk, embedded);
    }

    #[test]
    fn test_render_puts_the_harness_coordinates_into_the_project() {
        let declarations = Declarations {
            instance_documents: "---\nkind: ArgoCDInstance\n".to_owned(),
            target_cluster: "dev".to_owned(),
            target_fields: "reconciler: {argocd: {instanceRef: {name: dev}}}".to_owned(),
            publication_env: vec!["KUBECONFIG".to_owned()],
        };
        let files = render(&params(), &declarations).unwrap();
        let platform = file(&files, "nyl/platform.yaml");
        assert!(
            platform.contains("repoURL: 'http://172.18.0.4:8080/git/shop.git'"),
            "{platform}"
        );
        assert!(
            platform.contains("publishURL: 'http://127.0.0.1:40123/git/shop.git'"),
            "{platform}"
        );
        assert!(platform.contains("live: {context: e2e-admin}"), "{platform}");
        assert!(platform.contains("kubeVersion: 1.36.4"), "{platform}");
        assert!(
            platform.contains("\n  clusterRef: {name: dev}\n  reconciler: {argocd: {instanceRef: {name: dev}}}\n  applicationGroupSelector:"),
            "{platform}"
        );
        assert!(platform.contains("kind: ArgoCDInstance"), "{platform}");
        let units = file(&files, "nyl/units.yaml");
        assert!(units.contains("backend: {path: '/work/tofu/{{ environment.name }}-database.tfstate'}"));
        assert!(units.contains("repository: 'localhost:5001/web'"), "{units}");
        assert!(units.contains("env: {passthrough: [\"DOCKER_HOST\"]}"), "{units}");
        assert!(units.contains("env: {passthrough: [\"KUBECONFIG\"]}"), "{units}");
    }

    #[test]
    fn test_the_revision_changes_both_inputs_of_the_publication() {
        let render_revision = |revision| render(&Params { revision, ..params() }, &Declarations::default()).unwrap();
        let (first, second) = (render_revision(1), render_revision(2));
        for path in ["services/web/index.html", "infra/database/main.tf"] {
            assert_ne!(file(&first, path), file(&second, path), "{path}");
        }
        assert!(file(&second, "infra/database/main.tf").contains("\"${var.name}.r2.db.internal\""));
        assert_eq!(expected_database_host(2), "dev.r2.db.internal");
        // Nothing else changes, so a revision is exactly an input change.
        for path in ["nyl/platform.yaml", "nyl/units.yaml", "applications/web/release.yaml"] {
            assert_eq!(file(&first, path), file(&second, path), "{path}");
        }
    }

    /// The scenario reads the attestations a reconcile recorded, which only
    /// `waitHealthy` makes it record.
    #[test]
    fn test_the_publication_waits_for_health_in_every_variant() {
        let files = render(&params(), &Declarations::default()).unwrap();
        assert!(file(&files, "nyl/units.yaml").contains("  observe: {waitHealthy: {timeout: 5m}}\n"));
    }

    #[test]
    fn test_a_cluster_document_declares_the_destination_context_and_capabilities() {
        let document = cluster_document("kind-dev", "https://127.0.0.1:6443", "e2e-admin", "1.36.4");
        assert!(
            document.starts_with("---\napiVersion: k8s.nyl/v1\nkind: Cluster\n"),
            "{document}"
        );
        assert!(document.contains("  destination:\n    server: https://127.0.0.1:6443\n  live: {context: e2e-admin}\n"));
        // The same capabilities the template's Cluster gets.
        for line in cluster_capabilities("1.36.4").lines() {
            assert!(document.contains(&format!("  {line}\n")), "{line}");
        }
    }

    #[test]
    fn test_crlf_templates_render_like_lf_ones() {
        let values = BTreeMap::from([("DOCS", "a: 1\n".to_owned())]);
        let render = |template: &str| substitute(&normalize_newlines(template), &values, &mut BTreeSet::new()).unwrap();
        assert_eq!(render("x: 1\r\n  %DOCS%\r\ny: 2\r\n"), render("x: 1\n  %DOCS%\ny: 2\n"));
    }

    /// Argo CD waits for confirmation to delete a resource with either
    /// `Prune=confirm` or `Delete=confirm` when its Application is deleted, so
    /// a teardown that nobody confirms needs both policies to be Automatic.
    #[test]
    fn test_the_workload_namespace_never_asks_for_confirmation() {
        let group = FILES.iter().find(|(path, _)| *path == "nyl/web.yaml").unwrap().1;
        assert!(group.contains("namespace: {prunePolicy: Automatic, deletePolicy: Automatic}"));
    }

    #[test]
    fn test_a_reconciler_can_deploy_to_a_cluster_resource_of_its_own() {
        let declarations = Declarations {
            instance_documents: "---\nkind: Cluster\nmetadata:\n  name: dev-direct\n".to_owned(),
            target_cluster: "dev-direct".to_owned(),
            ..Declarations::default()
        };
        let files = render(&params(), &declarations).unwrap();
        let platform = file(&files, "nyl/platform.yaml");
        assert!(platform.contains("kind: DeploymentTarget"), "{platform}");
        assert!(
            platform.contains("\nspec:\n  clusterRef: {name: dev-direct}\n"),
            "{platform}"
        );
        assert!(platform.contains("  name: dev-direct\n---\n"), "{platform}");
    }

    #[test]
    fn test_a_reconciler_without_documents_leaves_no_empty_yaml_document() {
        let files = render(&params(), &Declarations::default()).unwrap();
        let platform = file(&files, "nyl/platform.yaml");
        assert!(!platform.contains("---\n---"), "{platform}");
        assert!(!platform.contains("---\n\n---"), "{platform}");
    }
}
