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
    refresh: bool,
    pub schema_digests: BTreeMap<String, String>,
    pub observed_sources: BTreeSet<PathBuf>,
    pub observed_origins: BTreeMap<String, super::report::SchemaOrigin>,
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
            refresh: false,
            observed_builtins: BTreeMap::new(),
            observed_origins: BTreeMap::new(),
            observed_sources: BTreeSet::new(),
            schema_digests: BTreeMap::new(),
            cache: project.join(".nyl/cache/validation-schemas"),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(settings.timeout_seconds))
                .build()
                .map_err(|error| NylError::validation(format!("Cannot initialize schema downloader: {error}")))?,
        })
    }

    /// Refresh population fetches each immutable source once per operation, bypassing persisted inputs.
    pub fn with_refresh(mut self, refresh: bool) -> Self {
        self.refresh = refresh && self.populate && !self.check;
        self
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

    /// Resolve native resources, retaining references for recursive CRD schemas.
    pub async fn builtin_resource(&mut self, gvk: &str, version: &str) -> Result<Option<SchemaDocument>> {
        let url = self.builtin_url(gvk, version)?;
        let (api, kind) = resource_parts(gvk)?;
        if let Some(api_version) = api.strip_prefix("apiextensions.k8s.io/") {
            if matches!(kind, "CustomResourceDefinition" | "CustomResourceDefinitionList") {
                // Recursive CRD schemas exist in shared definitions, not standalone files.
                let definition = format!("io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.{api_version}.{kind}");
                return Ok(Some(SchemaDocument {
                    value: serde_json::json!({"$ref": format!("_definitions.json#/definitions/{definition}")}),
                    origin: Origin::Builtin(url),
                }));
            }
        }
        self.builtin(&url).await
    }

    pub async fn builtin(&mut self, url: &str) -> Result<Option<SchemaDocument>> {
        let document = self.load_builtin(url).await?;
        if let Some(document) = &document {
            self.schema_digests
                .insert(url.to_owned(), store::digest(&store::json_bytes(&document.value)?));
        }
        Ok(document)
    }

    async fn load_builtin(&mut self, url: &str) -> Result<Option<SchemaDocument>> {
        if let Some(hash) = self.observed_builtins.get(url) {
            return Ok(Some(SchemaDocument {
                value: serde_json::from_slice(&store::read_blob(&self.vendor, hash)?)?,
                origin: Origin::Builtin(url.to_owned()),
            }));
        }
        if !self.refresh {
            let existing = store::read_builtins(&self.vendor)?;
            if let Some(hash) = existing.schemas.get(url) {
                match store::read_blob(&self.vendor, hash) {
                    Ok(bytes) => {
                        self.observed_builtins.insert(url.to_owned(), hash.clone());
                        return Ok(Some(SchemaDocument {
                            value: serde_json::from_slice(&bytes)?,
                            origin: Origin::Builtin(url.to_owned()),
                        }));
                    }
                    Err(_) if self.populate && !self.check => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if self.check || (self.settings.vendor_builtin_schemas && !self.populate) {
            return Err(NylError::validation(format!(
                "Missing vendored built-in schema {url}; run nyl vendor"
            )));
        }
        let cache_path = self.cache.join(format!("{}.json", store::digest(url.as_bytes())));
        let cached = (!self.refresh)
            .then(|| std::fs::read(&cache_path).ok())
            .flatten()
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

    pub fn local(&mut self, gvk: &str, version: &str) -> Result<Option<SchemaDocument>> {
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
            self.observed_sources.insert(path.clone());
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
        let mut queue = vec![(document, destination, BTreeSet::from([String::new()]))];
        let mut seen = BTreeMap::<PathBuf, BTreeSet<String>>::new();
        while let Some((mut document, destination, fragments)) = queue.pop() {
            let visited = seen.entry(destination.clone()).or_default();
            if fragments.is_subset(visited) {
                continue;
            }
            visited.extend(fragments);
            let fragments = visited.clone();
            let mut references = BTreeSet::new();
            visit_refs(&mut document.value, &fragments, &mut |reference| {
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
                        self.observed_sources.insert(path.clone());
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
                queue.push((dependency, path, BTreeSet::from([fragment.to_owned()])));
                replacements.insert(reference, replacement);
            }
            visit_refs(&mut document.value, &fragments, &mut |reference| {
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

/// Traverse schema positions and every fragment target reachable from them.
fn visit_refs(value: &mut Value, fragments: &BTreeSet<String>, visitor: &mut impl FnMut(&mut String)) -> Result<()> {
    let mut pending = fragments
        .iter()
        .map(|fragment| (String::new(), fragment.clone()))
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some((scope, fragment)) = pending.pop() {
        let scope_value = value.pointer(&scope).expect("reference scope exists");
        let pointer = format!("{scope}{}", fragment_pointer(scope_value, &fragment)?);
        if !visited.insert(pointer.clone()) {
            continue;
        }
        let scope = resource_scope(value, &pointer);
        let target = value
            .pointer_mut(&pointer)
            .ok_or_else(|| NylError::validation(format!("Schema fragment #{fragment} does not exist")))?;
        visit_schema_refs(target, &pointer, &scope, &mut |reference, scope| {
            if let Some(fragment) = reference.strip_prefix('#') {
                pending.push((scope.to_owned(), fragment.to_owned()));
            }
            visitor(reference);
        })?;
    }
    Ok(())
}

fn fragment_pointer(value: &Value, fragment: &str) -> Result<String> {
    let mut decoded = Vec::new();
    let mut bytes = fragment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| char::from(byte).to_digit(16));
            let low = bytes.next().and_then(|byte| char::from(byte).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(NylError::validation("Invalid percent escape in schema fragment"));
            };
            decoded.push(u8::try_from(high * 16 + low).expect("two hexadecimal digits fit in one byte"));
        } else {
            decoded.push(byte);
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| NylError::validation("Invalid UTF-8 schema fragment"))?;
    if decoded.is_empty() || decoded.starts_with('/') {
        return Ok(decoded);
    }
    let mut matches = Vec::new();
    find_anchor(value, "", &decoded, &mut matches);
    match matches.as_slice() {
        [pointer] => Ok(pointer.clone()),
        _ => Err(NylError::validation(format!(
            "Schema anchor #{decoded} must resolve unambiguously"
        ))),
    }
}

fn has_resource_id(fields: &serde_json::Map<String, Value>) -> bool {
    fields
        .get("$id")
        .or_else(|| fields.get("id"))
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty() && !id.starts_with('#'))
}

fn resource_scope(value: &Value, pointer: &str) -> String {
    let mut scope = String::new();
    let mut current = String::new();
    for segment in pointer.split('/').skip(1) {
        current.push('/');
        current.push_str(segment);
        if value
            .pointer(&current)
            .and_then(Value::as_object)
            .is_some_and(has_resource_id)
        {
            scope.clone_from(&current);
        }
    }
    scope
}

fn child_pointer(pointer: &str, key: &str) -> String {
    format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn find_anchor(value: &Value, pointer: &str, anchor: &str, matches: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if !pointer.is_empty() && has_resource_id(fields) {
                return;
            }
            if fields.get("$anchor").and_then(Value::as_str) == Some(anchor)
                || ["$id", "id"].iter().any(|key| {
                    fields
                        .get(*key)
                        .and_then(Value::as_str)
                        .and_then(|id| id.strip_prefix('#'))
                        == Some(anchor)
                })
            {
                matches.push(pointer.to_owned());
            }
            for (key, child) in fields {
                find_anchor(child, &child_pointer(pointer, key), anchor, matches);
            }
        }
        Value::Array(children) => {
            for (index, child) in children.iter().enumerate() {
                find_anchor(child, &format!("{pointer}/{index}"), anchor, matches);
            }
        }
        _ => {}
    }
}

fn visit_schema_refs(
    value: &mut Value,
    pointer: &str,
    scope: &str,
    visitor: &mut impl FnMut(&mut String, &str),
) -> Result<()> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let scope = if has_resource_id(object) { pointer } else { scope };
    if let Some(reference) = object.get_mut("$ref") {
        let Value::String(reference) = reference else {
            return Err(NylError::validation("Schema $ref must be a string"));
        };
        visitor(reference, scope);
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
            for (name, child) in children {
                visit_schema_refs(
                    child,
                    &child_pointer(&child_pointer(pointer, key), name),
                    scope,
                    visitor,
                )?;
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
                for (index, child) in children.iter_mut().enumerate() {
                    visit_schema_refs(
                        child,
                        &child_pointer(&child_pointer(pointer, key), &index.to_string()),
                        scope,
                        visitor,
                    )?;
                }
            } else {
                visit_schema_refs(child, &child_pointer(pointer, key), scope, visitor)?;
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for (index, child) in children.iter_mut().enumerate() {
                visit_schema_refs(
                    child,
                    &child_pointer(&child_pointer(pointer, key), &index.to_string()),
                    scope,
                    visitor,
                )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn seed_builtin(resolver: &SchemaResolver<'_>, url: &str, value: &Value) -> String {
        let bytes = store::json_bytes(value).unwrap();
        let hash = store::write_blob(&resolver.vendor, &bytes).unwrap();
        let index = store::BuiltinIndex {
            version: 1,
            schemas: BTreeMap::from([(url.to_owned(), hash.clone())]),
        };
        store::atomic_write(
            &resolver.vendor.join("schemas/builtins.json"),
            &store::json_bytes(&index).unwrap(),
        )
        .unwrap();
        let cache = CachedSchema {
            digest: hash.clone(),
            value: value.clone(),
        };
        store::atomic_write(
            &resolver.cache.join(format!("{}.json", store::digest(url.as_bytes()))),
            &store::json_bytes(&cache).unwrap(),
        )
        .unwrap();
        hash
    }

    #[tokio::test]
    async fn test_population_repairs_blobs_but_validation_and_check_fail_closed() {
        let directory = TempDir::new().unwrap();
        let vendor = directory.path().join("vendor");
        let settings = KubeconformSettings {
            vendor_builtin_schemas: true,
            ..KubeconformSettings::default()
        };
        for corrupt in [false, true] {
            let mut populate = SchemaResolver::new(directory.path(), vendor.clone(), &settings, true, false).unwrap();
            let url = populate.builtin_url("v1/ConfigMap", "1.31.4").unwrap();
            let value = json!({"type":"object"});
            let hash = seed_builtin(&populate, &url, &value);
            let blob = vendor.join(format!("schemas/blobs/{hash}.json"));
            if corrupt {
                std::fs::write(&blob, b"{}").unwrap();
            } else {
                std::fs::remove_file(&blob).unwrap();
            }
            for check in [false, true] {
                let mut reader =
                    SchemaResolver::new(directory.path(), vendor.clone(), &settings, false, check).unwrap();
                assert!(reader.builtin(&url).await.is_err());
            }
            assert_eq!(populate.builtin(&url).await.unwrap().unwrap().value, value);
            assert_eq!(
                store::read_blob(&vendor, &hash).unwrap(),
                store::json_bytes(&value).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn test_refresh_fetches_once_and_bypasses_vendor_and_disposable_cache() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/schema.json", listener.local_addr().unwrap());
        let fresh = json!({"type":"object","required":["metadata"]});
        let body = fresh.to_string();
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let length = socket.read(&mut buffer).await.unwrap();
                    assert!(length > 0);
                    request.extend_from_slice(&buffer[..length]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") { break; }
                }
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }).await.unwrap();
        });
        let directory = TempDir::new().unwrap();
        let settings = KubeconformSettings::default();
        let mut resolver = SchemaResolver::new(
            directory.path(),
            directory.path().join("vendor"),
            &settings,
            true,
            false,
        )
        .unwrap()
        .with_refresh(true);
        seed_builtin(&resolver, &url, &json!({"type":"object"}));
        assert_eq!(resolver.builtin(&url).await.unwrap().unwrap().value, fresh);
        server.await.unwrap();
        // The listener is closed; a second lookup must reuse the refreshed blob.
        assert_eq!(resolver.builtin(&url).await.unwrap().unwrap().value, fresh);
        let hash = &resolver.observed_builtins[&url];
        assert_eq!(
            store::read_blob(&resolver.vendor, hash).unwrap(),
            store::json_bytes(&fresh).unwrap()
        );
    }
}
