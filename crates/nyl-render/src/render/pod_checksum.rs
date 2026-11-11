//! Pod template checksums derived from the ConfigMaps and Secrets a Release
//! renders.
//!
//! A workload rolls out again only when its Pod template changes. A Pod
//! template that opts in with `k8s.nyl/checksum` gets
//! `k8s.nyl/checksum-sha256`, the SHA-256 of the data of the ConfigMaps and
//! Secrets it consumes, so a change to that data changes the template. The
//! step runs once per Release in [`super::session::RenderSession::render`], on
//! the final documents after expansion, Kyverno, and deduplication, so every
//! command that renders a Release, from `nyl render` to `nyl publish-tree`,
//! writes the same value. It is a pure function of those documents and the
//! Release namespace: it reads no cluster and no file, so the render cache
//! needs no input of its own for it.
//!
//! # Opt-in
//!
//! `k8s.nyl/checksum` is read from the annotations of the workload's Pod
//! template: `spec.template` of any document, or
//! `spec.jobTemplate.spec.template` of a `CronJob`. Its value is either
//!
//! - `auto`: every ConfigMap and Secret the Pod spec references that the
//!   Release renders (see [`discover`] for the exhaustive list of reference
//!   forms). A reference to something the Release does not render, or to a key
//!   the rendered object lacks, is skipped; or
//! - a comma-separated list of `Kind/name` entries, each with an optional
//!   `#key`. Every entry must resolve, or the render fails.
//!
//! References resolve in the workload's namespace, as Kubernetes resolves
//! them: its `metadata.namespace`, else the Release namespace. A resource
//! without a namespace is in the Release namespace too, so the result does not
//! depend on document order or on which documents spell the namespace out.
//!
//! # What is hashed
//!
//! Only the bytes a Pod can read: a ConfigMap's `data` (UTF-8 strings) and
//! `binaryData` (base64, decoded), a Secret's `data` (base64, decoded) and
//! `stringData`, which wins over `data` for the same key as it does when
//! Kubernetes stores the Secret. Metadata, labels, annotations, `type`,
//! `immutable`, and the order or formatting of keys never matter. A value is
//! its exact bytes, so a trailing newline counts.
//!
//! The selected values form a set of `(kind, name, key, value)` items. The
//! digest input is [`FORMAT_TAG`] followed by the items in ascending byte
//! order of `(kind, name, key)`, each field written as its length in bytes
//! (eight bytes, big endian) and then the bytes. Because every field carries
//! its length, no two different sets produce the same input. Referencing an
//! object twice, or by whole object and by key, adds each key once, and the
//! order of references (or of the annotation's entries) never matters.
//!
//! The namespace is not hashed: it is metadata, and every item shares the
//! workload's namespace.

use std::collections::{BTreeMap, HashMap};

use base64::Engine as _;
use nyl_core::constants::{ANNOTATION_CHECKSUM, ANNOTATION_CHECKSUM_SHA256};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::NylError;

/// Prefix of the digest input. It names the serialization, so a future format
/// cannot produce a value that this one also produces.
pub const FORMAT_TAG: &[u8] = b"nyl.k8s/checksum/v1\0";

/// `k8s.nyl/checksum` is neither `auto` nor a list of `Kind/name[#key]`.
pub const MALFORMED: &str = "NYL-KUBERNETES-CHECKSUM-MALFORMED";
/// An explicit entry names a ConfigMap, Secret, or key the Release does not render.
pub const NOT_FOUND: &str = "NYL-KUBERNETES-CHECKSUM-NOT-FOUND";
/// A reference matches more than one rendered object, or a key has two values.
pub const AMBIGUOUS: &str = "NYL-KUBERNETES-CHECKSUM-AMBIGUOUS";
/// `k8s.nyl/checksum` is on a document that has no Pod template, or not on its Pod template.
pub const NO_POD_TEMPLATE: &str = "NYL-KUBERNETES-CHECKSUM-NO-POD-TEMPLATE";
/// A document sets `k8s.nyl/checksum-sha256`, which Nyl owns.
pub const RESERVED: &str = "NYL-KUBERNETES-CHECKSUM-RESERVED";
/// A referenced ConfigMap or Secret holds data that is not valid.
pub const INVALID_DATA: &str = "NYL-KUBERNETES-CHECKSUM-INVALID-DATA";

/// A failure of the step, with the document it concerns.
#[derive(Debug)]
pub(crate) struct ChecksumError {
    /// Index of the workload document in the manifests the step ran on.
    pub document: usize,
    /// The workload's kind, for the error's heading.
    kind: String,
    code: &'static str,
    message: String,
    fix: String,
}

impl ChecksumError {
    #[cfg(test)]
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    /// The error as Nyl reports it: stable code and message, then the fix.
    pub(crate) fn into_nyl(self) -> NylError {
        NylError::ResourceParse {
            kind: self.kind,
            message: format!("{}: {}", self.code, self.message),
            hint: self.fix,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SourceKind {
    ConfigMap,
    Secret,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ConfigMap => "ConfigMap",
            Self::Secret => "Secret",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "ConfigMap" => Some(Self::ConfigMap),
            "Secret" => Some(Self::Secret),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Select {
    All,
    Key(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Reference {
    kind: SourceKind,
    name: String,
    select: Select,
}

impl Reference {
    fn display(&self) -> String {
        match &self.select {
            Select::All => format!("{}/{}", self.kind.as_str(), self.name),
            Select::Key(key) => format!("{}/{}#{key}", self.kind.as_str(), self.name),
        }
    }
}

enum Mode {
    Auto,
    Explicit(Vec<Reference>),
}

/// The workload's Pod template and what its annotation asks for.
struct Workload {
    index: usize,
    namespace: String,
    template_path: &'static str,
    mode: Mode,
}

/// `(kind, name, key)` to value; its iteration order is the digest order.
type Selected = BTreeMap<(SourceKind, String, String), Vec<u8>>;

type SourceIndex = HashMap<(SourceKind, String, String), Vec<usize>>;

/// Write `k8s.nyl/checksum-sha256` on every Pod template that declares
/// `k8s.nyl/checksum`.
///
/// `default_namespace` is the Release namespace, which documents without
/// `metadata.namespace` take.
pub(crate) fn apply(manifests: &mut [Value], default_namespace: Option<&str>) -> Result<(), ChecksumError> {
    let default_namespace = default_namespace.unwrap_or_default();
    let workloads = collect_workloads(manifests, default_namespace)?;
    if workloads.is_empty() {
        return Ok(());
    }
    let index = index_sources(manifests, default_namespace);
    let mut data = HashMap::new();
    let mut checksums = Vec::with_capacity(workloads.len());
    for workload in &workloads {
        let selected = select(manifests, &index, &mut data, workload)?;
        checksums.push((workload, checksum(&selected)));
    }
    for (workload, digest) in checksums {
        let annotations = manifests[workload.index]
            .pointer_mut(&format!("{}/metadata/annotations", workload.template_path))
            .and_then(Value::as_object_mut)
            .expect("the workload's Pod template annotations were read from this document");
        annotations.insert(ANNOTATION_CHECKSUM_SHA256.to_owned(), Value::String(digest));
    }
    Ok(())
}

/// The digest of a selection, in lowercase hex. See the module documentation
/// for the serialization.
fn checksum(selected: &Selected) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FORMAT_TAG);
    for ((kind, name, key), value) in selected {
        for field in [kind.as_str().as_bytes(), name.as_bytes(), key.as_bytes(), value] {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
    }
    hex::encode(hasher.finalize())
}

fn template_path(kind: &str) -> &'static str {
    if kind == "CronJob" {
        "/spec/jobTemplate/spec/template"
    } else {
        "/spec/template"
    }
}

fn kind_of(document: &Value) -> &str {
    document.get("kind").and_then(Value::as_str).unwrap_or_default()
}

fn namespace_of<'a>(document: &'a Value, default_namespace: &'a str) -> &'a str {
    document
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|namespace| !namespace.trim().is_empty())
        .unwrap_or(default_namespace)
}

fn identity(document: &Value, default_namespace: &str) -> String {
    let name = document
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>");
    match namespace_of(document, default_namespace) {
        "" => format!("{} {name}", kind_of(document)),
        namespace => format!("{} {namespace}/{name}", kind_of(document)),
    }
}

fn namespace_label(namespace: &str) -> String {
    if namespace.is_empty() {
        "no namespace".to_owned()
    } else {
        format!("namespace {namespace:?}")
    }
}

fn error(
    document: &Value,
    index: usize,
    default_namespace: &str,
    code: &'static str,
    message: impl std::fmt::Display,
    fix: impl Into<String>,
) -> ChecksumError {
    ChecksumError {
        document: index,
        kind: kind_of(document).to_owned(),
        code,
        message: format!("{}: {message}", identity(document, default_namespace)),
        fix: fix.into(),
    }
}

fn annotations_at<'a>(document: &'a Value, pointer: &str) -> Option<&'a serde_json::Map<String, Value>> {
    document.pointer(pointer)?.as_object()
}

/// Find the Pod templates that opt in, and reject misplaced or reserved annotations.
fn collect_workloads(manifests: &[Value], default_namespace: &str) -> Result<Vec<Workload>, ChecksumError> {
    let mut workloads = Vec::new();
    for (index, document) in manifests.iter().enumerate() {
        let path = template_path(kind_of(document));
        let template = document.pointer(path).filter(|template| template.is_object());
        let own = annotations_at(document, "/metadata/annotations");
        let in_template = template.and_then(|_| annotations_at(document, &format!("{path}/metadata/annotations")));

        for (location, annotations) in [
            ("metadata.annotations".to_owned(), own),
            (
                format!(
                    "{}.metadata.annotations",
                    path.trim_start_matches('/').replace('/', ".")
                ),
                in_template,
            ),
        ] {
            if annotations.is_some_and(|annotations| annotations.contains_key(ANNOTATION_CHECKSUM_SHA256)) {
                return Err(error(
                    document,
                    index,
                    default_namespace,
                    RESERVED,
                    format!("{location} sets {ANNOTATION_CHECKSUM_SHA256}, which Nyl owns"),
                    format!(
                        "Remove {ANNOTATION_CHECKSUM_SHA256}. Nyl writes it on the Pod template that sets {ANNOTATION_CHECKSUM}; \
                         set {ANNOTATION_CHECKSUM}: auto there, or list the ConfigMaps and Secrets it covers."
                    ),
                ));
            }
        }

        if own.is_some_and(|annotations| annotations.contains_key(ANNOTATION_CHECKSUM)) {
            let template_location = path.trim_start_matches('/').replace('/', ".");
            return Err(if template.is_some() {
                error(
                    document,
                    index,
                    default_namespace,
                    NO_POD_TEMPLATE,
                    format!("{ANNOTATION_CHECKSUM} is set on the document's own metadata.annotations; Nyl reads it from the Pod template"),
                    format!(
                        "Move {ANNOTATION_CHECKSUM} to {template_location}.metadata.annotations, \
                         so the checksum changes the Pod template and rolls the Pods."
                    ),
                )
            } else {
                error(
                    document,
                    index,
                    default_namespace,
                    NO_POD_TEMPLATE,
                    format!(
                        "{ANNOTATION_CHECKSUM} is set on a {} that has no Pod template ({template_location})",
                        kind_of(document)
                    ),
                    format!(
                        "Remove the annotation from this document. Set it on the Pod template of the workload that consumes the \
                         ConfigMap or Secret: {template_location}.metadata.annotations of a Deployment, StatefulSet, DaemonSet, \
                         ReplicaSet, or Job, or spec.jobTemplate.spec.template.metadata.annotations of a CronJob."
                    ),
                )
            });
        }

        let Some(value) = in_template.and_then(|annotations| annotations.get(ANNOTATION_CHECKSUM)) else {
            continue;
        };
        let mode = value
            .as_str()
            .ok_or_else(|| "its value is not a string".to_owned())
            .and_then(parse_annotation)
            .map_err(|reason| {
                error(
                    document,
                    index,
                    default_namespace,
                    MALFORMED,
                    format!("{ANNOTATION_CHECKSUM} is {value}, but {reason}"),
                    format!(
                        "Set {ANNOTATION_CHECKSUM} to `auto`, or to a comma-separated list of `ConfigMap/<name>` and \
                         `Secret/<name>` entries, each with an optional `#<key>`, for example \
                         \"ConfigMap/app-config,Secret/app-env#password\"."
                    ),
                )
            })?;
        workloads.push(Workload {
            index,
            namespace: namespace_of(document, default_namespace).to_owned(),
            template_path: path,
            mode,
        });
    }
    Ok(workloads)
}

fn parse_annotation(value: &str) -> Result<Mode, String> {
    let value = value.trim();
    if value == "auto" {
        return Ok(Mode::Auto);
    }
    let mut references = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(if value.is_empty() {
                "it is empty".to_owned()
            } else {
                "it has an empty entry".to_owned()
            });
        }
        let (kind, rest) = entry
            .split_once('/')
            .ok_or_else(|| format!("entry `{entry}` is not `Kind/name`"))?;
        let kind = SourceKind::parse(kind).ok_or_else(|| {
            format!("entry `{entry}` names kind `{kind}`, but only ConfigMap and Secret can be hashed")
        })?;
        let (name, select) = match rest.split_once('#') {
            None => (rest, Select::All),
            Some((name, key)) if !key.is_empty() && key.chars().all(is_key_char) => (name, Select::Key(key.to_owned())),
            Some(_) => {
                return Err(format!(
                    "entry `{entry}` has an empty or invalid key after `#`; keys use letters, digits, `-`, `_`, and `.`"
                ))
            }
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
            return Err(format!("entry `{entry}` has an empty or invalid name"));
        }
        references.push(Reference {
            kind,
            name: name.to_owned(),
            select,
        });
    }
    Ok(Mode::Explicit(references))
}

fn is_key_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

fn index_sources(manifests: &[Value], default_namespace: &str) -> SourceIndex {
    let mut index = SourceIndex::new();
    for (position, document) in manifests.iter().enumerate() {
        if document.get("apiVersion").and_then(Value::as_str) != Some("v1") {
            continue;
        }
        let Some(kind) = SourceKind::parse(kind_of(document)) else {
            continue;
        };
        let Some(name) = document.pointer("/metadata/name").and_then(Value::as_str) else {
            continue;
        };
        index
            .entry((
                kind,
                namespace_of(document, default_namespace).to_owned(),
                name.to_owned(),
            ))
            .or_default()
            .push(position);
    }
    index
}

/// What a workload's checksum covers.
fn select(
    manifests: &[Value],
    index: &SourceIndex,
    data: &mut HashMap<usize, BTreeMap<String, Vec<u8>>>,
    workload: &Workload,
) -> Result<Selected, ChecksumError> {
    let document = &manifests[workload.index];
    let default_namespace = workload.namespace.as_str();
    let fail =
        |code, message: String, fix: String| error(document, workload.index, default_namespace, code, message, fix);
    let (references, strict) = match &workload.mode {
        Mode::Explicit(references) => (references.clone(), true),
        Mode::Auto => {
            let spec = document
                .pointer(&format!("{}/spec", workload.template_path))
                .unwrap_or(&Value::Null);
            (discover(spec), false)
        }
    };

    let mut selected = Selected::new();
    for reference in &references {
        let key = (reference.kind, workload.namespace.clone(), reference.name.clone());
        let position =
            match index.get(&key).map(Vec::as_slice) {
                None | Some([]) => {
                    if strict {
                        return Err(fail(
                        NOT_FOUND,
                        format!(
                            "{ANNOTATION_CHECKSUM} lists {}, but the Release renders no such resource in {}",
                            reference.display(),
                            namespace_label(&workload.namespace)
                        ),
                        "Render the resource in the same Release, in the workload's namespace, or correct its name. \
                         Nyl hashes only rendered documents and never reads the cluster; use `auto` to skip references \
                         the Release does not render."
                            .to_owned(),
                    ));
                    }
                    continue;
                }
                Some([position]) => *position,
                Some(positions) => {
                    return Err(fail(
                    AMBIGUOUS,
                    format!(
                        "{} matches {} rendered documents in {} (documents {}), so its data is not defined",
                        reference.display(),
                        positions.len(),
                        namespace_label(&workload.namespace),
                        positions.iter().map(|p| (p + 1).to_string()).collect::<Vec<_>>().join(", ")
                    ),
                    "Render the resource once: remove the duplicate, or give them different names. A resource with an \
                     explicit namespace and one without, in the Release namespace, are the same object."
                        .to_owned(),
                ));
                }
            };
        let source = &manifests[position];
        if let std::collections::hash_map::Entry::Vacant(slot) = data.entry(position) {
            let values = source_data(source, reference.kind).map_err(|problem| match problem {
                DataProblem::Ambiguous(message) => fail(
                    AMBIGUOUS,
                    format!("{} is ambiguous: {message}", reference.display()),
                    "Keep each key in one of data and binaryData.".to_owned(),
                ),
                DataProblem::Invalid(message) => fail(
                    INVALID_DATA,
                    format!("{} cannot be hashed: {message}", reference.display()),
                    "Correct the value: data and stringData hold strings, and binaryData, and a Secret's data, hold \
                     base64 text."
                        .to_owned(),
                ),
            })?;
            slot.insert(values);
        }
        let values = &data[&position];
        match &reference.select {
            Select::All => {
                for (key, value) in values {
                    selected.insert((reference.kind, reference.name.clone(), key.clone()), value.clone());
                }
            }
            Select::Key(key) => match values.get(key) {
                Some(value) => {
                    selected.insert((reference.kind, reference.name.clone(), key.clone()), value.clone());
                }
                None if strict => {
                    return Err(fail(
                        NOT_FOUND,
                        format!(
                            "{ANNOTATION_CHECKSUM} lists {}, but that resource has no key {key:?}",
                            reference.display()
                        ),
                        "Add the key to the resource's data, binaryData, or stringData, or correct the key.".to_owned(),
                    ));
                }
                None => {}
            },
        }
    }
    Ok(selected)
}

enum DataProblem {
    Ambiguous(String),
    Invalid(String),
}

/// The bytes a Pod reads from a ConfigMap or Secret document, by key.
fn source_data(document: &Value, kind: SourceKind) -> Result<BTreeMap<String, Vec<u8>>, DataProblem> {
    let strings = |field: &str| -> Result<Vec<(&str, &str)>, DataProblem> {
        match document.get(field) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Object(entries)) => entries
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|text| (key.as_str(), text))
                        .ok_or_else(|| DataProblem::Invalid(format!("{field}[{key:?}] is not a string")))
                })
                .collect(),
            Some(_) => Err(DataProblem::Invalid(format!("{field} is not a map of strings"))),
        }
    };
    let decode = |field: &str, entries: Vec<(&str, &str)>| -> Result<Vec<(String, Vec<u8>)>, DataProblem> {
        entries
            .into_iter()
            .map(|(key, text)| {
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map(|bytes| (key.to_owned(), bytes))
                    .map_err(|error| DataProblem::Invalid(format!("{field}[{key:?}] is not valid base64: {error}")))
            })
            .collect()
    };
    let mut values = BTreeMap::new();
    match kind {
        SourceKind::ConfigMap => {
            for (key, text) in strings("data")? {
                values.insert(key.to_owned(), text.as_bytes().to_vec());
            }
            for (key, bytes) in decode("binaryData", strings("binaryData")?)? {
                if values.contains_key(&key) {
                    return Err(DataProblem::Ambiguous(format!(
                        "key {key:?} is in both data and binaryData"
                    )));
                }
                values.insert(key, bytes);
            }
        }
        SourceKind::Secret => {
            for (key, bytes) in decode("data", strings("data")?)? {
                values.insert(key, bytes);
            }
            // Kubernetes merges stringData into data when it stores the Secret, and it wins.
            for (key, text) in strings("stringData")? {
                values.insert(key.to_owned(), text.as_bytes().to_vec());
            }
        }
    }
    Ok(values)
}

/// The ConfigMaps and Secrets a Pod spec references, in discovery order.
///
/// These are all the forms `auto` reads, for the containers, init containers,
/// and ephemeral containers alike:
///
/// - `envFrom[].configMapRef` and `envFrom[].secretRef`: the whole object;
/// - `env[].valueFrom.configMapKeyRef` and `secretKeyRef`: that key;
/// - `volumes[].configMap` and `volumes[].secret`: the keys in `items`, else
///   the whole object;
/// - `volumes[].projected.sources[].configMap` and `.secret`: as above.
///
/// Image pull secrets, service accounts, and volume types that reference a
/// Secret for access (CSI, RBD) are not read: they carry no content the
/// workload's configuration depends on.
fn discover(spec: &Value) -> Vec<Reference> {
    fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
        value.get(field).and_then(Value::as_array).map_or(&[], Vec::as_slice)
    }
    fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }
    fn whole_or_items(references: &mut Vec<Reference>, kind: SourceKind, name: &str, items: &[Value]) {
        let keys = items.iter().filter_map(|item| text(item, "/key")).collect::<Vec<_>>();
        if items.is_empty() || keys.is_empty() {
            references.push(Reference {
                kind,
                name: name.to_owned(),
                select: Select::All,
            });
        }
        for key in keys {
            references.push(Reference {
                kind,
                name: name.to_owned(),
                select: Select::Key(key.to_owned()),
            });
        }
    }

    let mut references = Vec::new();
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for container in array(spec, field) {
            for source in array(container, "envFrom") {
                if let Some(name) = text(source, "/configMapRef/name") {
                    references.push(Reference {
                        kind: SourceKind::ConfigMap,
                        name: name.to_owned(),
                        select: Select::All,
                    });
                }
                if let Some(name) = text(source, "/secretRef/name") {
                    references.push(Reference {
                        kind: SourceKind::Secret,
                        name: name.to_owned(),
                        select: Select::All,
                    });
                }
            }
            for variable in array(container, "env") {
                for (kind, pointer) in [
                    (SourceKind::ConfigMap, "/valueFrom/configMapKeyRef"),
                    (SourceKind::Secret, "/valueFrom/secretKeyRef"),
                ] {
                    if let (Some(name), Some(key)) = (
                        text(variable, &format!("{pointer}/name")),
                        text(variable, &format!("{pointer}/key")),
                    ) {
                        references.push(Reference {
                            kind,
                            name: name.to_owned(),
                            select: Select::Key(key.to_owned()),
                        });
                    }
                }
            }
        }
    }
    for volume in array(spec, "volumes") {
        if let Some(name) = text(volume, "/configMap/name") {
            let items = volume.pointer("/configMap/items").and_then(Value::as_array);
            whole_or_items(
                &mut references,
                SourceKind::ConfigMap,
                name,
                items.map_or(&[], Vec::as_slice),
            );
        }
        // The volume's Secret is named `secretName`, unlike every other reference.
        if let Some(name) = text(volume, "/secret/secretName") {
            let items = volume.pointer("/secret/items").and_then(Value::as_array);
            whole_or_items(
                &mut references,
                SourceKind::Secret,
                name,
                items.map_or(&[], Vec::as_slice),
            );
        }
        for source in volume
            .pointer("/projected/sources")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            for (kind, field) in [(SourceKind::ConfigMap, "configMap"), (SourceKind::Secret, "secret")] {
                if let Some(name) = text(source, &format!("/{field}/name")) {
                    let items = source.pointer(&format!("/{field}/items")).and_then(Value::as_array);
                    whole_or_items(&mut references, kind, name, items.map_or(&[], Vec::as_slice));
                }
            }
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_map(name: &str, data: Value) -> Value {
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "app"}, "data": data})
    }

    fn secret(name: &str, fields: Value) -> Value {
        let mut secret = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name, "namespace": "app"}});
        for (field, value) in fields.as_object().unwrap() {
            secret[field] = value.clone();
        }
        secret
    }

    fn deployment(annotation: &str, spec: Value) -> Value {
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "app"},
            "spec": {"template": {"metadata": {"annotations": {ANNOTATION_CHECKSUM: annotation}}, "spec": spec}},
        })
    }

    fn explicit(annotation: &str) -> Value {
        deployment(annotation, json!({"containers": []}))
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// The checksum written on the Deployment of `manifests`.
    fn run(mut manifests: Vec<Value>) -> Result<String, ChecksumError> {
        apply(&mut manifests, Some("app"))?;
        let workload = manifests.iter().find(|doc| doc["kind"] == "Deployment").unwrap();
        Ok(workload
            .pointer(&format!(
                "/spec/template/metadata/annotations/{}",
                ANNOTATION_CHECKSUM_SHA256.replace('/', "~1")
            ))
            .and_then(Value::as_str)
            .unwrap()
            .to_owned())
    }

    fn checksum_of(annotation: &str, sources: Vec<Value>) -> String {
        let mut manifests = vec![explicit(annotation)];
        manifests.extend(sources);
        run(manifests).unwrap()
    }

    fn failure(manifests: Vec<Value>) -> ChecksumError {
        run(manifests).unwrap_err()
    }

    #[test]
    fn test_serialization_is_the_documented_framing() {
        // The golden digest pins the format: a change to it re-rolls every
        // workload that uses the annotation, so it must be deliberate.
        let digest = checksum_of(
            "ConfigMap/cfg",
            vec![config_map("cfg", json!({"b": "two", "a": "one\n"}))],
        );

        let mut input = FORMAT_TAG.to_vec();
        for (key, value) in [("a", "one\n"), ("b", "two")] {
            for field in ["ConfigMap", "cfg", key, value] {
                input.extend((field.len() as u64).to_be_bytes());
                input.extend(field.as_bytes());
            }
        }
        assert_eq!(digest, hex::encode(Sha256::digest(&input)));
        assert_eq!(
            digest, "83a62a2013fa0f563e1f1ac76fd8d78e7c528426fe05a764cf9989eae4937f89",
            "serialization golden"
        );
    }

    #[test]
    fn test_checksum_ignores_everything_but_data() {
        let base = checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"a": "1", "b": "2"}))]);

        let mut decorated = config_map("cfg", json!({"b": "2", "a": "1"}));
        decorated["metadata"]["labels"] = json!({"team": "x"});
        decorated["metadata"]["annotations"] = json!({"note": "y"});
        decorated["immutable"] = json!(true);
        assert_eq!(checksum_of("ConfigMap/cfg", vec![decorated]), base);

        let changed = checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"a": "1", "b": "3"}))]);
        assert_ne!(changed, base);
    }

    #[test]
    fn test_trailing_newline_is_significant() {
        let without = checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"a": "text"}))]);
        let with = checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"a": "text\n"}))]);
        assert_ne!(without, with);
    }

    #[test]
    fn test_selecting_one_key_ignores_the_others() {
        let selected =
            |a: &str, b: &str| checksum_of("ConfigMap/cfg#a", vec![config_map("cfg", json!({"a": a, "b": b}))]);
        assert_eq!(selected("1", "x"), selected("1", "y"));
        assert_ne!(selected("1", "x"), selected("2", "x"));
    }

    #[test]
    fn test_explicit_entries_are_a_set() {
        let sources = || {
            vec![
                config_map("one", json!({"k": "1"})),
                config_map("two", json!({"k": "2"})),
                secret("s", json!({"stringData": {"p": "pw"}})),
            ]
        };
        let ordered = checksum_of("ConfigMap/one,ConfigMap/two,Secret/s#p", sources());
        let reordered = checksum_of(" Secret/s#p , ConfigMap/two,ConfigMap/one,ConfigMap/one#k", sources());
        assert_eq!(ordered, reordered);
    }

    #[test]
    fn test_binary_data_and_string_data_hash_the_bytes_a_pod_reads() {
        let from_data = checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"f": "abc"}))]);
        let mut binary = config_map("cfg", json!({}));
        binary["binaryData"] = json!({"f": b64(b"abc")});
        assert_eq!(checksum_of("ConfigMap/cfg", vec![binary]), from_data);

        let mut bytes = config_map("cfg", json!({}));
        bytes["binaryData"] = json!({"f": b64(&[0, 255, 7])});
        let mut other = config_map("cfg", json!({}));
        other["binaryData"] = json!({"f": b64(&[0, 255, 8])});
        assert_ne!(
            checksum_of("ConfigMap/cfg", vec![bytes]),
            checksum_of("ConfigMap/cfg", vec![other])
        );

        // A Secret's stringData is its data, and wins for the same key.
        let encoded = secret("s", json!({"data": {"p": b64(b"pw"), "q": b64(b"q")}}));
        let plain = secret("s", json!({"stringData": {"p": "pw"}, "data": {"q": b64(b"q")}}));
        let overridden = secret(
            "s",
            json!({"data": {"p": b64(b"old"), "q": b64(b"q")}, "stringData": {"p": "pw"}}),
        );
        let expected = checksum_of("Secret/s", vec![encoded]);
        assert_eq!(checksum_of("Secret/s", vec![plain]), expected);
        assert_eq!(checksum_of("Secret/s", vec![overridden]), expected);
    }

    #[test]
    fn test_framing_separates_names_keys_and_values() {
        let value_split = |first: &str, second: &str| {
            checksum_of(
                "ConfigMap/cfg",
                vec![config_map("cfg", json!({"a": first, "b": second}))],
            )
        };
        assert_ne!(value_split("x", "yz"), value_split("xy", "z"));
        let key_vs_name_one = checksum_of("ConfigMap/a", vec![config_map("a", json!({"bc": "v"}))]);
        let key_vs_name_two = checksum_of("ConfigMap/ab", vec![config_map("ab", json!({"c": "v"}))]);
        assert_ne!(key_vs_name_one, key_vs_name_two);
    }

    fn full_spec() -> Value {
        json!({
            "containers": [{
                "name": "app",
                "envFrom": [{"configMapRef": {"name": "from-cm"}}, {"secretRef": {"name": "from-secret"}}],
                "env": [
                    {"name": "A", "valueFrom": {"configMapKeyRef": {"name": "env-cm", "key": "k"}}},
                    {"name": "B", "valueFrom": {"secretKeyRef": {"name": "env-secret", "key": "k"}}},
                    {"name": "C", "value": "literal"},
                ],
            }],
            "initContainers": [{"name": "init", "envFrom": [{"configMapRef": {"name": "init-cm"}}]}],
            "ephemeralContainers": [{"name": "debug", "envFrom": [{"secretRef": {"name": "debug-secret"}}]}],
            "volumes": [
                {"name": "v1", "configMap": {"name": "vol-cm"}},
                {"name": "v2", "configMap": {"name": "items-cm", "items": [{"key": "one", "path": "x"}]}},
                {"name": "v3", "secret": {"secretName": "vol-secret"}},
                {"name": "v4", "secret": {"secretName": "items-secret", "items": [{"key": "one", "path": "y"}]}},
                {"name": "v5", "projected": {"sources": [
                    {"configMap": {"name": "proj-cm"}},
                    {"secret": {"name": "proj-secret", "items": [{"key": "one", "path": "z"}]}},
                ]}},
                {"name": "v6", "emptyDir": {}},
            ],
        })
    }

    fn full_sources() -> Vec<Value> {
        let mut sources = Vec::new();
        for name in ["from-cm", "env-cm", "init-cm", "vol-cm", "items-cm", "proj-cm"] {
            sources.push(config_map(name, json!({"k": "v", "one": "1", "two": "2"})));
        }
        let data = json!({"data": {"k": b64(b"v"), "one": b64(b"1"), "two": b64(b"2")}});
        for name in [
            "from-secret",
            "env-secret",
            "debug-secret",
            "vol-secret",
            "items-secret",
            "proj-secret",
        ] {
            sources.push(secret(name, data.clone()));
        }
        sources
    }

    #[test]
    fn test_auto_covers_every_reference_form() {
        let mut manifests = vec![deployment("auto", full_spec())];
        manifests.extend(full_sources());
        let auto = run(manifests).unwrap();

        // The same selection, spelled out: whole objects for envFrom and
        // whole-volume references, single keys for key references and items.
        let expected = checksum_of(
            "ConfigMap/from-cm,Secret/from-secret,ConfigMap/env-cm#k,Secret/env-secret#k,ConfigMap/init-cm,\
             Secret/debug-secret,ConfigMap/vol-cm,ConfigMap/items-cm#one,Secret/vol-secret,Secret/items-secret#one,\
             ConfigMap/proj-cm,Secret/proj-secret#one",
            full_sources(),
        );
        assert_eq!(auto, expected);

        // Each reference form is read: changing what only it covers changes the checksum.
        for (kind, name, key) in [
            ("ConfigMap", "from-cm", "two"),
            ("ConfigMap", "env-cm", "k"),
            ("ConfigMap", "init-cm", "two"),
            ("ConfigMap", "vol-cm", "two"),
            ("ConfigMap", "items-cm", "one"),
            ("ConfigMap", "proj-cm", "two"),
            ("Secret", "from-secret", "two"),
            ("Secret", "env-secret", "k"),
            ("Secret", "debug-secret", "two"),
            ("Secret", "vol-secret", "two"),
            ("Secret", "items-secret", "one"),
            ("Secret", "proj-secret", "one"),
        ] {
            let mut manifests = vec![deployment("auto", full_spec())];
            for mut source in full_sources() {
                if source["kind"] == kind && source["metadata"]["name"] == name {
                    if kind == "Secret" {
                        source["data"][key] = json!(b64(b"changed"));
                    } else {
                        source["data"][key] = json!("changed");
                    }
                }
                manifests.push(source);
            }
            assert_ne!(run(manifests).unwrap(), auto, "{kind}/{name}#{key} is not hashed");
        }

        // What no reference selects is not hashed: other keys of a key
        // reference or of `items`, and objects the Pod does not reference.
        for (kind, name, key) in [
            ("ConfigMap", "env-cm", "one"),
            ("ConfigMap", "items-cm", "two"),
            ("Secret", "env-secret", "one"),
            ("Secret", "items-secret", "two"),
            ("Secret", "proj-secret", "two"),
        ] {
            let mut manifests = vec![deployment("auto", full_spec())];
            for mut source in full_sources() {
                if source["kind"] == kind && source["metadata"]["name"] == name {
                    source["data"][key] = if kind == "Secret" {
                        json!(b64(b"changed"))
                    } else {
                        json!("changed")
                    };
                }
                manifests.push(source);
            }
            assert_eq!(run(manifests).unwrap(), auto, "{kind}/{name}#{key} is hashed");
        }
        let mut manifests = vec![deployment("auto", full_spec())];
        manifests.extend(full_sources());
        manifests.push(config_map("unrelated", json!({"k": "changed"})));
        assert_eq!(run(manifests).unwrap(), auto);
    }

    #[test]
    fn test_auto_skips_what_the_release_does_not_render_and_ignores_document_order() {
        let spec = json!({"containers": [{"name": "app", "envFrom": [
            {"configMapRef": {"name": "one"}},
            {"configMapRef": {"name": "elsewhere"}},
            {"secretRef": {"name": "two"}},
        ], "env": [{"name": "M", "valueFrom": {"configMapKeyRef": {"name": "one", "key": "absent"}}}]}]});
        let sources = || {
            vec![
                config_map("one", json!({"k": "1"})),
                secret("two", json!({"stringData": {"k": "2"}})),
            ]
        };

        let mut forward = vec![deployment("auto", spec.clone())];
        forward.extend(sources());
        let mut backward = sources();
        backward.reverse();
        backward.insert(1, deployment("auto", spec));
        let digest = run(forward).unwrap();
        assert_eq!(run(backward).unwrap(), digest);
        assert_eq!(digest, checksum_of("ConfigMap/one,Secret/two", sources()));
    }

    #[test]
    fn test_references_resolve_in_the_workloads_namespace() {
        let workload = |namespace: Option<&str>| {
            let mut document = explicit("ConfigMap/cfg");
            match namespace {
                Some(namespace) => document["metadata"]["namespace"] = json!(namespace),
                None => {
                    document["metadata"].as_object_mut().unwrap().remove("namespace");
                }
            }
            document
        };
        let source = |namespace: Option<&str>| {
            let mut document = config_map("cfg", json!({"k": "v"}));
            match namespace {
                Some(namespace) => document["metadata"]["namespace"] = json!(namespace),
                None => {
                    document["metadata"].as_object_mut().unwrap().remove("namespace");
                }
            }
            document
        };
        // Spelling the Release namespace out, or leaving it off, is the same.
        let digests = [
            run(vec![workload(None), source(None)]),
            run(vec![workload(Some("app")), source(None)]),
            run(vec![workload(None), source(Some("app"))]),
        ]
        .map(Result::unwrap);
        assert!(digests.iter().all(|digest| digest == &digests[0]));

        // Another namespace is another object.
        let error = failure(vec![workload(None), source(Some("other"))]);
        assert_eq!(error.code(), NOT_FOUND);

        // Without a Release namespace, documents with none match each other.
        let mut manifests = vec![workload(None), source(None)];
        apply(&mut manifests, None).unwrap();
        assert!(manifests[0]
            .pointer("/spec/template/metadata/annotations")
            .and_then(|annotations| annotations.get(ANNOTATION_CHECKSUM_SHA256))
            .is_some());
    }

    #[test]
    fn test_cron_job_template_is_nested_under_the_job_template() {
        let mut manifests = vec![
            json!({
                "apiVersion": "batch/v1", "kind": "CronJob",
                "metadata": {"name": "nightly", "namespace": "app"},
                "spec": {"jobTemplate": {"spec": {"template": {
                    "metadata": {"annotations": {ANNOTATION_CHECKSUM: "ConfigMap/cfg"}},
                    "spec": {"containers": []},
                }}}},
            }),
            config_map("cfg", json!({"k": "v"})),
        ];
        apply(&mut manifests, Some("app")).unwrap();
        let written = manifests[0]
            .pointer("/spec/jobTemplate/spec/template/metadata/annotations")
            .unwrap();
        assert_eq!(written[ANNOTATION_CHECKSUM], "ConfigMap/cfg");
        assert_eq!(
            written[ANNOTATION_CHECKSUM_SHA256],
            checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"k": "v"}))])
        );
    }

    #[test]
    fn test_documents_without_the_annotation_are_untouched() {
        let original = vec![
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "w"},
                   "spec": {"template": {"spec": {"containers": []}}}}),
            config_map("cfg", json!({"k": "v"})),
        ];
        let mut manifests = original.clone();
        apply(&mut manifests, Some("app")).unwrap();
        assert_eq!(manifests, original);
    }

    #[test]
    fn test_errors_carry_stable_codes_and_fixes() {
        let cfg = || config_map("cfg", json!({"k": "v"}));
        let cases: Vec<(Vec<Value>, &str, &str)> = vec![
            (
                vec![explicit("ConfigMap/missing"), cfg()],
                NOT_FOUND,
                "ConfigMap/missing",
            ),
            (vec![explicit("Secret/cfg"), cfg()], NOT_FOUND, "Secret/cfg"),
            (
                vec![explicit("ConfigMap/cfg#absent"), cfg()],
                NOT_FOUND,
                "no key \"absent\"",
            ),
            (
                vec![explicit("ConfigMap/cfg"), cfg(), {
                    let mut twin = cfg();
                    twin["metadata"].as_object_mut().unwrap().remove("namespace");
                    twin
                }],
                AMBIGUOUS,
                "documents 2, 3",
            ),
            (
                vec![explicit("ConfigMap/cfg"), {
                    let mut both = cfg();
                    both["binaryData"] = json!({"k": b64(b"v")});
                    both
                }],
                AMBIGUOUS,
                "both data and binaryData",
            ),
            (
                vec![explicit("Secret/s"), secret("s", json!({"data": {"k": "***"}}))],
                INVALID_DATA,
                "not valid base64",
            ),
            (vec![explicit("")], MALFORMED, "empty"),
            (vec![explicit("ConfigMap/a,")], MALFORMED, "empty entry"),
            (vec![explicit("ConfigMap")], MALFORMED, "not `Kind/name`"),
            (vec![explicit("Deployment/web")], MALFORMED, "only ConfigMap and Secret"),
            (vec![explicit("ConfigMap/cfg#")], MALFORMED, "invalid key"),
            (vec![explicit("auto,ConfigMap/cfg")], MALFORMED, "not `Kind/name`"),
            (vec![explicit("AUTO")], MALFORMED, "not `Kind/name`"),
            (
                vec![{
                    let mut document = explicit("auto");
                    document["spec"]["template"]["metadata"]["annotations"][ANNOTATION_CHECKSUM_SHA256] = json!("abc");
                    document
                }],
                RESERVED,
                "which Nyl owns",
            ),
            (
                vec![{
                    let mut document = explicit("auto");
                    document["metadata"]["annotations"] = json!({ANNOTATION_CHECKSUM_SHA256: "abc"});
                    document
                }],
                RESERVED,
                "metadata.annotations",
            ),
            (
                vec![{
                    let mut document = cfg();
                    document["metadata"]["annotations"] = json!({ANNOTATION_CHECKSUM: "auto"});
                    document
                }],
                NO_POD_TEMPLATE,
                "has no Pod template",
            ),
            (
                vec![{
                    let mut document = json!({"apiVersion": "apps/v1", "kind": "Deployment",
                        "metadata": {"name": "web", "annotations": {ANNOTATION_CHECKSUM: "auto"}},
                        "spec": {"template": {"spec": {"containers": []}}}});
                    document["metadata"]["namespace"] = json!("app");
                    document
                }],
                NO_POD_TEMPLATE,
                "Move k8s.nyl/checksum to spec.template.metadata.annotations",
            ),
        ];
        for (manifests, code, expected) in cases {
            let error = failure(manifests);
            assert_eq!(error.code(), code, "{}", error.message);
            let rendered = error.into_nyl().to_string();
            assert!(rendered.contains(code) && rendered.contains(expected), "{rendered}");
            assert!(rendered.contains("Hint:"), "{rendered}");
        }

        let non_string = {
            let mut document = explicit("auto");
            document["spec"]["template"]["metadata"]["annotations"][ANNOTATION_CHECKSUM] = json!(true);
            document
        };
        assert_eq!(failure(vec![non_string]).code(), MALFORMED);
    }

    #[test]
    fn test_error_names_the_workload() {
        let error = failure(vec![explicit("ConfigMap/missing")]);
        assert_eq!(error.document, 0);
        let rendered = error.into_nyl().to_string();
        assert!(rendered.starts_with("Invalid Deployment: NYL-KUBERNETES-CHECKSUM-NOT-FOUND: Deployment app/web"));
    }
}
