//! Resolve schemas before invoking a validator, retaining immutable source identities.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct CachedSchema {
    digest: String,
    value: Value,
}

use super::{store, KubeconformSettings};
use crate::{NylError, Result};

/// Updated deliberately alongside compatibility fixtures and the tool pin.
pub const BUILTIN_REVISION: &str = "07b64c5376535fbbd6fb9910621e1a41f7613c14";

#[derive(Clone)]
pub enum Origin {
    Local(PathBuf),
    Builtin(String),
    Captured,
}

pub struct SchemaDocument {
    pub value: Value,
    pub origin: Origin,
}

pub struct SchemaResolver<'a> {
    pub project: &'a Path,
    pub vendor: PathBuf,
    pub settings: &'a KubeconformSettings,
    pub populate: bool,
    pub check: bool,
    pub observed_builtins: BTreeMap<String, String>,
    cache: PathBuf,
    client: reqwest::Client,
}

impl<'a> SchemaResolver<'a> {
    pub fn new(
        project: &'a Path,
        vendor: PathBuf,
        settings: &'a KubeconformSettings,
        populate: bool,
        check: bool,
    ) -> Result<Self> {
        if settings.timeout_seconds == 0 {
            return Err(NylError::config(
                "validation.kubeconform.timeout_seconds must be positive",
            ));
        }
        let revision = settings.builtin_schema_revision.as_deref().unwrap_or(BUILTIN_REVISION);
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(NylError::config(
                "builtin_schema_revision must be a full immutable Git commit SHA",
            ));
        }
        for gvk in &settings.skip {
            resource_parts(gvk)?;
        }
        Ok(Self {
            project,
            vendor,
            settings,
            populate,
            check,
            observed_builtins: BTreeMap::new(),
            cache: project.join(".nyl/cache/validation-schemas"),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(settings.timeout_seconds))
                .build()
                .map_err(|error| NylError::validation(format!("Cannot initialize schema downloader: {error}")))?,
        })
    }

    pub fn builtin_url(&self, gvk: &str, version: &str) -> Result<String> {
        let (api_version, kind) = resource_parts(gvk)?;
        let (_, _, suffix) = lookup_parts(api_version);
        let revision = self
            .settings
            .builtin_schema_revision
            .as_deref()
            .unwrap_or(BUILTIN_REVISION);
        let strict = if self.settings.strict { "-strict" } else { "" };
        Ok(format!("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{revision}/v{version}-standalone{strict}/{}{suffix}.json", kind.to_lowercase()))
    }

    pub async fn builtin(&mut self, url: &str) -> Result<Option<SchemaDocument>> {
        let existing = store::read_builtins(&self.vendor)?;
        if let Some(hash) = existing.schemas.get(url) {
            self.observed_builtins.insert(url.to_owned(), hash.clone());
            let bytes = store::read_blob(&self.vendor, hash)?;
            return Ok(Some(SchemaDocument {
                value: serde_json::from_slice(&bytes)?,
                origin: Origin::Builtin(url.to_owned()),
            }));
        }
        if self.check || (self.settings.vendor_builtin_schemas && !self.populate) {
            return Err(NylError::validation(format!(
                "Missing vendored built-in schema {url}; run nyl vendor"
            )));
        }
        let cache_path = self.cache.join(format!("{}.json", store::digest(url.as_bytes())));
        let cached = std::fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedSchema>(&bytes).ok())
            .filter(|record| store::json_bytes(&record.value).is_ok_and(|bytes| store::digest(&bytes) == record.digest))
            .map(|record| record.value);
        let value = if let Some(value) = cached {
            value
        } else {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|error| NylError::validation(format!("Schema download failed for {url}: {error}")))?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let response = response
                .error_for_status()
                .map_err(|error| NylError::validation(format!("Schema download failed for {url}: {error}")))?;
            let bytes = response
                .bytes()
                .await
                .map_err(|error| NylError::validation(format!("Cannot read schema {url}: {error}")))?;
            let value: Value = serde_json::from_slice(&bytes)?;
            // A disposable cache must not make a valid download fail.
            if let Ok(bytes) = store::json_bytes(&value) {
                let record = CachedSchema {
                    digest: store::digest(&bytes),
                    value: value.clone(),
                };
                if let Ok(bytes) = store::json_bytes(&record) {
                    let _ = store::atomic_write(&cache_path, &bytes);
                }
            }
            value
        };
        if self.populate {
            let bytes = store::json_bytes(&value)?;
            let hash = store::write_blob(&self.vendor, &bytes)?;
            self.observed_builtins.insert(url.to_owned(), hash);
        }
        Ok(Some(SchemaDocument {
            value,
            origin: Origin::Builtin(url.to_owned()),
        }))
    }

    pub fn local(&self, gvk: &str, version: &str) -> Result<Option<SchemaDocument>> {
        let (api_version, kind) = resource_parts(gvk)?;
        let (group, api, suffix) = lookup_parts(api_version);
        let fields = BTreeMap::from([
            ("Group", group.to_owned()),
            ("ResourceAPIVersion", api.to_owned()),
            ("ResourceKind", kind.to_lowercase()),
            ("KindSuffix", suffix),
            (
                "StrictSuffix",
                if self.settings.strict { "-strict" } else { "" }.to_owned(),
            ),
            ("NormalizedKubernetesVersion", format!("v{version}")),
        ]);
        let expression = regex::Regex::new(r"\{\{\s*\.([A-Za-z]+)\s*\}\}").expect("constant regex");
        for location in &self.settings.schema_locations {
            let template = if Path::new(location)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                location.clone()
            } else {
                format!("{location}/{{{{.NormalizedKubernetesVersion}}}}-standalone{{{{.StrictSuffix}}}}/{{{{.ResourceKind}}}}{{{{.KindSuffix}}}}.json")
            };
            let mut invalid = false;
            let path = expression.replace_all(&template, |captures: &regex::Captures<'_>| {
                fields.get(&captures[1]).cloned().unwrap_or_else(|| {
                    invalid = true;
                    String::new()
                })
            });
            if invalid || path.contains("{{") || path.contains("://") {
                return Err(NylError::config(format!(
                    "Unsupported local schema lookup template: {location}"
                )));
            }
            let path = store::safe_path(self.project, Path::new(path.as_ref()))?;
            match std::fs::read(&path) {
                Ok(bytes) => {
                    return Ok(Some(SchemaDocument {
                        value: serde_json::from_slice(&bytes)?,
                        origin: Origin::Local(path),
                    }))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }

    /// Copy the complete reference graph into private staging; kubeconform needs no network.
    pub async fn materialize(&mut self, document: SchemaDocument, destination: PathBuf, stage: &Path) -> Result<()> {
        let mut queue = vec![(document, destination)];
        let mut seen = BTreeSet::new();
        while let Some((mut document, destination)) = queue.pop() {
            if !seen.insert(destination.clone()) {
                continue;
            }
            let mut references = BTreeSet::new();
            visit_refs(&mut document.value, &mut |reference| {
                references.insert(reference.clone());
            })?;
            let mut replacements = BTreeMap::new();
            for reference in references {
                if reference.starts_with('#') {
                    continue;
                }
                let (file, fragment) = reference.split_once('#').unwrap_or((&reference, ""));
                let dependency = match &document.origin {
                    Origin::Local(source) => {
                        if file.contains(':') || file.starts_with("//") {
                            return Err(NylError::validation(
                                "Local schema references must stay within the project",
                            ));
                        }
                        let parent = source.parent().expect("local schema has parent");
                        let path = normalize_local(self.project, &parent.join(file))?;
                        let value = serde_json::from_slice(&std::fs::read(&path)?)?;
                        SchemaDocument {
                            value,
                            origin: Origin::Local(path),
                        }
                    }
                    Origin::Builtin(source) => {
                        let base =
                            reqwest::Url::parse(source).map_err(|error| NylError::validation(error.to_string()))?;
                        let url = base
                            .join(file)
                            .map_err(|error| NylError::validation(error.to_string()))?;
                        let revision = self
                            .settings
                            .builtin_schema_revision
                            .as_deref()
                            .unwrap_or(BUILTIN_REVISION);
                        let prefix =
                            format!("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{revision}/");
                        if !url.as_str().starts_with(&prefix) {
                            return Err(NylError::validation(
                                "Built-in schema reference escapes the pinned registry",
                            ));
                        }
                        self.builtin(url.as_str())
                            .await?
                            .ok_or_else(|| NylError::validation(format!("Missing schema dependency {url}")))?
                    }
                    Origin::Captured => {
                        return Err(NylError::validation("Captured schema contains an external reference"))
                    }
                };
                let identity = match &dependency.origin {
                    Origin::Local(path) => path.to_string_lossy().into_owned(),
                    Origin::Builtin(url) => url.clone(),
                    Origin::Captured => unreachable!(),
                };
                let path = stage
                    .join("dependencies")
                    .join(format!("{}.json", store::digest(identity.as_bytes())));
                let url = reqwest::Url::from_file_path(&path)
                    .map_err(|()| NylError::validation("Cannot construct staged schema reference"))?;
                let replacement = format!("{url}#{fragment}");
                replacements.insert(reference, replacement);
                queue.push((dependency, path));
            }
            visit_refs(&mut document.value, &mut |reference| {
                if let Some(replacement) = replacements.get(reference) {
                    *reference = replacement.clone();
                }
            })?;
            store::atomic_write(&destination, &store::json_bytes(&document.value)?)?;
        }
        Ok(())
    }
}

fn normalize_local(project: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix(project)
        .map_err(|_| NylError::validation("Schema reference escapes the project"))?;
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if normalized.pop() => {}
            _ => return Err(NylError::validation("Schema reference escapes the project")),
        }
    }
    store::safe_path(project, &normalized)
}

fn visit_refs(value: &mut Value, visitor: &mut impl FnMut(&mut String)) -> Result<()> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    if let Some(reference) = object.get_mut("$ref") {
        let Value::String(reference) = reference else {
            return Err(NylError::validation("Schema $ref must be a string"));
        };
        visitor(reference);
    }
    for key in [
        "properties",
        "patternProperties",
        "definitions",
        "$defs",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                visit_refs(child, visitor)?;
            }
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "additionalItems",
        "not",
        "if",
        "then",
        "else",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
    ] {
        if let Some(child) = object.get_mut(key) {
            if let Some(children) = child.as_array_mut() {
                for child in children {
                    visit_refs(child, visitor)?;
                }
            } else {
                visit_refs(child, visitor)?;
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                visit_refs(child, visitor)?;
            }
        }
    }
    Ok(())
}

pub fn resource_parts(gvk: &str) -> Result<(&str, &str)> {
    let (api_version, kind) = gvk
        .rsplit_once('/')
        .ok_or_else(|| NylError::config(format!("Expected apiVersion/kind: {gvk}")))?;
    if kind.is_empty()
        || api_version.is_empty()
        || gvk.split('/').count() > 3
        || !gvk
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'/' | b'_'))
    {
        return Err(NylError::validation(format!("Invalid resource identity {gvk}")));
    }
    Ok((api_version, kind))
}

fn lookup_parts(api_version: &str) -> (&str, &str, String) {
    match api_version.split_once('/') {
        Some((group, version)) => (
            group,
            version,
            format!("-{}-{version}", group.split('.').next().unwrap_or(group).to_lowercase()),
        ),
        None => (api_version, api_version, format!("-{}", api_version.to_lowercase())),
    }
}

pub fn schema_relative_path(gvk: &str) -> Result<PathBuf> {
    let (api, kind) = resource_parts(gvk)?;
    let (group, version, _) = lookup_parts(api);
    Ok(PathBuf::from("schemas")
        .join(group)
        .join(format!("{}_{version}.json", kind.to_lowercase())))
}

pub fn normalize_version(version: &str) -> Result<String> {
    let version = version.trim_start_matches('v').split(['-', '+']).next().unwrap_or("");
    let parts = version.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(NylError::config(format!(
            "A concrete Kubernetes major.minor[.patch] version is required, got {version:?}"
        )));
    }
    Ok(if parts.len() == 2 {
        format!("{version}.0")
    } else {
        version.to_owned()
    })
}

/// Native API groups for which the pinned Kubernetes registry is authoritative.
/// Other groups require explicit or captured schemas, even when their kind names collide.
pub fn is_builtin_group(group: &str) -> bool {
    matches!(
        group,
        "" | "apps"
            | "autoscaling"
            | "batch"
            | "policy"
            | "extensions"
            | "admissionregistration.k8s.io"
            | "apiextensions.k8s.io"
            | "apiregistration.k8s.io"
            | "authentication.k8s.io"
            | "authorization.k8s.io"
            | "certificates.k8s.io"
            | "coordination.k8s.io"
            | "discovery.k8s.io"
            | "events.k8s.io"
            | "flowcontrol.apiserver.k8s.io"
            | "internal.apiserver.k8s.io"
            | "networking.k8s.io"
            | "node.k8s.io"
            | "rbac.authorization.k8s.io"
            | "resource.k8s.io"
            | "scheduling.k8s.io"
            | "storage.k8s.io"
            | "storagemigration.k8s.io"
    )
}
