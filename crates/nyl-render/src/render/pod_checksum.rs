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
//! destination namespace, which the render request carries into the render
//! cache's key: it reads no cluster and no file.
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
//! them: its `metadata.namespace`, else the destination namespace. That is the
//! namespace the Release is deployed to, which a document without one takes: the
//! `destinationNamespace` of the ApplicationGroup that renders the Release, else
//! the Release's own namespace (for a file without a Release, the namespace the
//! command was given). A resource without a namespace is in it too, so the
//! result does not depend on document order or on which documents spell the
//! namespace out.
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
const FORMAT_TAG: &[u8] = b"k8s.nyl/checksum/v1\0";

/// `k8s.nyl/checksum` is neither `auto` nor a list of `Kind/name[#key]`.
const MALFORMED: &str = "NYL-KUBERNETES-CHECKSUM-MALFORMED";
/// An explicit entry names a ConfigMap, Secret, or key the Release does not render.
const NOT_FOUND: &str = "NYL-KUBERNETES-CHECKSUM-NOT-FOUND";
/// A reference matches more than one rendered object, or a key has two values.
const AMBIGUOUS: &str = "NYL-KUBERNETES-CHECKSUM-AMBIGUOUS";
/// `k8s.nyl/checksum` is on a document that has no Pod template, or not on its Pod template.
const NO_POD_TEMPLATE: &str = "NYL-KUBERNETES-CHECKSUM-NO-POD-TEMPLATE";
/// A document sets `k8s.nyl/checksum-sha256` without opting in, which Nyl owns.
const RESERVED: &str = "NYL-KUBERNETES-CHECKSUM-RESERVED";
/// A referenced ConfigMap or Secret holds data that is not valid.
const INVALID_DATA: &str = "NYL-KUBERNETES-CHECKSUM-INVALID-DATA";
/// A partial render (`--only-source-kind`) holds a Pod template that sets `k8s.nyl/checksum`.
const PARTIAL: &str = "NYL-KUBERNETES-CHECKSUM-PARTIAL";
/// `auto` found no ConfigMap or Secret the Release renders, so the digest covers nothing.
const EMPTY: &str = "NYL-KUBERNETES-CHECKSUM-EMPTY";

/// What the step needs besides the documents.
pub(crate) struct Options<'a> {
    /// The namespace documents without `metadata.namespace` are in.
    pub default_namespace: Option<&'a str>,
    /// The documents are a part of the Release, selected by `--only-source-kind`.
    pub partial: bool,
}

/// A failure of the step, with the document it concerns.
#[derive(Debug)]
pub(crate) struct ChecksumError {
    /// Index of the workload document in the manifests the step ran on.
    pub document: usize,
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
        NylError::Coded {
            code: self.code,
            message: self.message,
            fix: self.fix,
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
/// A Pod template that already holds `k8s.nyl/checksum-sha256`, written by an
/// earlier render of the same document, gets the value recomputed.
///
/// Returns the warnings, each with its stable code.
pub(crate) fn apply(manifests: &mut [Value], options: &Options<'_>) -> Result<Vec<String>, ChecksumError> {
    let default_namespace = options.default_namespace.unwrap_or_default();
    let workloads = collect_workloads(manifests, default_namespace)?;
    let mut warnings = Vec::new();
    if workloads.is_empty() {
        return Ok(warnings);
    }
    if options.partial {
        let workload = &workloads[0];
        return Err(error(
            &manifests[workload.index],
            workload.index,
            default_namespace,
            PARTIAL,
            format!("{ANNOTATION_CHECKSUM} is set, but this render holds only the documents `--only-source-kind` selected, which may lack the ConfigMaps and Secrets the checksum reads"),
            "Drop `--only-source-kind` so the whole Release renders. A checksum of a part of the Release would be a different value than every other command writes.",
        ));
    }
    let index = index_sources(manifests, default_namespace);
    let mut data = HashMap::new();
    let mut checksums = Vec::with_capacity(workloads.len());
    for workload in &workloads {
        let selected = select(manifests, &index, &mut data, workload)?;
        if selected.is_empty() && matches!(workload.mode, Mode::Auto) {
            warnings.push(format!(
                "{EMPTY}: {}: {ANNOTATION_CHECKSUM} is auto, but its Pod spec references no ConfigMap or Secret that the \
                 Release renders, so the digest covers nothing. Reference the ConfigMap or Secret from the Pod spec in \
                 {}, or list what to hash.",
                identity(&manifests[workload.index], default_namespace),
                namespace_label(&workload.namespace),
            ));
        }
        checksums.push((workload, checksum(&selected)));
    }
    for (workload, digest) in checksums {
        let annotations = manifests[workload.index]
            .pointer_mut(&format!("{}/metadata/annotations", workload.template_path))
            .and_then(Value::as_object_mut)
            .expect("the workload's Pod template annotations were read from this document");
        annotations.insert(ANNOTATION_CHECKSUM_SHA256.to_owned(), Value::String(digest));
    }
    Ok(warnings)
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
        let in_template = template.and_then(|_| annotations_at(document, &format!("{path}/metadata/annotations")));
        let template_location = path.trim_start_matches('/').replace('/', ".");

        reject_misplaced(document, index, default_namespace, template.is_some())?;

        // An earlier render of the same document wrote the digest next to the
        // opt-in, and it is recomputed. Without the opt-in, Nyl does not own it.
        if in_template.is_some_and(|annotations| {
            annotations.contains_key(ANNOTATION_CHECKSUM_SHA256) && !annotations.contains_key(ANNOTATION_CHECKSUM)
        }) {
            return Err(error(
                document,
                index,
                default_namespace,
                RESERVED,
                format!("{template_location}.metadata.annotations sets {ANNOTATION_CHECKSUM_SHA256} without {ANNOTATION_CHECKSUM}; Nyl owns that key"),
                format!(
                    "Remove {ANNOTATION_CHECKSUM_SHA256}, or set {ANNOTATION_CHECKSUM}: auto next to it so Nyl computes it."
                ),
            ));
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

/// Reject the annotations on the workload's own metadata, and on a CronJob's
/// job template metadata. They are not the Pod template, which Nyl reads and
/// writes, and a change to them rolls no Pods.
fn reject_misplaced(
    document: &Value,
    index: usize,
    default_namespace: &str,
    has_template: bool,
) -> Result<(), ChecksumError> {
    let template_location = template_path(kind_of(document))
        .trim_start_matches('/')
        .replace('/', ".");
    let mut elsewhere = vec![(
        "metadata.annotations",
        annotations_at(document, "/metadata/annotations"),
    )];
    if kind_of(document) == "CronJob" {
        elsewhere.push((
            "spec.jobTemplate.metadata.annotations",
            annotations_at(document, "/spec/jobTemplate/metadata/annotations"),
        ));
    }
    for (location, annotations) in &elsewhere {
        let Some(annotations) = annotations else { continue };
        if annotations.contains_key(ANNOTATION_CHECKSUM_SHA256) {
            return Err(error(
                document,
                index,
                default_namespace,
                RESERVED,
                format!("{location} sets {ANNOTATION_CHECKSUM_SHA256}, which Nyl owns"),
                format!(
                    "Remove {ANNOTATION_CHECKSUM_SHA256}. Nyl writes it on the Pod template that sets {ANNOTATION_CHECKSUM}."
                ),
            ));
        }
        if annotations.contains_key(ANNOTATION_CHECKSUM) {
            return Err(if has_template {
                error(
                    document,
                    index,
                    default_namespace,
                    NO_POD_TEMPLATE,
                    format!("{ANNOTATION_CHECKSUM} is set on {location}; Nyl reads it from the Pod template"),
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
                    "Remove the annotation from this document. Set it on the Pod template of the workload that \
                     consumes the ConfigMap or Secret: spec.template.metadata.annotations of a Deployment, \
                     StatefulSet, DaemonSet, ReplicaSet, or Job, or \
                     spec.jobTemplate.spec.template.metadata.annotations of a CronJob."
                        .to_owned(),
                )
            });
        }
    }
    Ok(())
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
                // Kubernetes decodes base64 with Go, which ignores line breaks.
                let text: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
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

    fn apply_in(manifests: &mut [Value], namespace: Option<&str>) -> Result<Vec<String>, ChecksumError> {
        apply(
            manifests,
            &Options {
                default_namespace: namespace,
                partial: false,
            },
        )
    }

    /// The checksum written on the Deployment of `manifests`.
    fn run(mut manifests: Vec<Value>) -> Result<String, ChecksumError> {
        apply_in(&mut manifests, Some("app"))?;
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
            digest, "044cd7701a07230a904677b8aa126e847d2f16daa32abbc6cdcef8041fba1aba",
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
        apply_in(&mut manifests, None).unwrap();
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
        apply_in(&mut manifests, Some("app")).unwrap();
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
        apply_in(&mut manifests, Some("app")).unwrap();
        assert_eq!(manifests, original);
    }

    #[test]
    fn test_errors_for_references_values_and_the_annotation_itself() {
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
        ];
        assert_failures(cases);

        let non_string = {
            let mut document = explicit("auto");
            document["spec"]["template"]["metadata"]["annotations"][ANNOTATION_CHECKSUM] = json!(true);
            document
        };
        assert_eq!(failure(vec![non_string]).code(), MALFORMED);
    }

    /// Each case fails with its code, the expected text, and a fix.
    fn assert_failures(cases: Vec<(Vec<Value>, &str, &str)>) {
        for (manifests, code, expected) in cases {
            let error = failure(manifests);
            assert_eq!(error.code(), code, "{}", error.message);
            let rendered = error.into_nyl().to_string();
            assert!(rendered.contains(code) && rendered.contains(expected), "{rendered}");
            assert!(rendered.contains("Hint:"), "{rendered}");
        }
    }

    #[test]
    fn test_errors_for_reserved_and_misplaced_annotations() {
        let cfg = || config_map("cfg", json!({"k": "v"}));
        let cases: Vec<(Vec<Value>, &str, &str)> = vec![
            (
                vec![{
                    let mut document = explicit("auto");
                    let annotations = document["spec"]["template"]["metadata"]["annotations"]
                        .as_object_mut()
                        .unwrap();
                    annotations.remove(ANNOTATION_CHECKSUM);
                    annotations.insert(ANNOTATION_CHECKSUM_SHA256.to_owned(), json!("abc"));
                    document
                }],
                RESERVED,
                "without k8s.nyl/checksum",
            ),
            (
                vec![{
                    let mut document = cfg();
                    document["kind"] = json!("Pod");
                    document["metadata"]["annotations"] = json!({ANNOTATION_CHECKSUM: "auto"});
                    document
                }],
                NO_POD_TEMPLATE,
                "has no Pod template",
            ),
            (
                vec![json!({
                    "apiVersion": "batch/v1", "kind": "CronJob",
                    "metadata": {"name": "nightly", "namespace": "app"},
                    "spec": {"jobTemplate": {
                        "metadata": {"annotations": {ANNOTATION_CHECKSUM: "auto"}},
                        "spec": {"template": {"spec": {"containers": []}}},
                    }},
                })],
                NO_POD_TEMPLATE,
                "Move k8s.nyl/checksum to spec.jobTemplate.spec.template.metadata.annotations",
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
        assert_failures(cases);
    }

    #[test]
    fn test_error_names_the_workload() {
        let error = failure(vec![explicit("ConfigMap/missing")]);
        assert_eq!(error.document, 0);
        let rendered = error.into_nyl().to_string();
        assert!(rendered.starts_with("NYL-KUBERNETES-CHECKSUM-NOT-FOUND: Deployment app/web"));
    }

    #[test]
    fn test_the_destination_namespace_is_the_default_and_may_differ_from_the_releases() {
        // The group deploys the Release to `prod`, whatever the Release's own
        // namespace is: the ConfigMap names `prod`, the Deployment names none.
        let in_prod = |auto: &str| {
            let mut workload = explicit(auto);
            workload["metadata"].as_object_mut().unwrap().remove("namespace");
            let mut source = config_map("cfg", json!({"k": "v"}));
            source["metadata"]["namespace"] = json!("prod");
            vec![workload, source]
        };
        for annotation in ["auto", "ConfigMap/cfg"] {
            let mut manifests = in_prod(annotation);
            manifests[0]["spec"]["template"]["spec"] =
                json!({"containers": [{"name": "c", "envFrom": [{"configMapRef": {"name": "cfg"}}]}]});
            let warnings = apply_in(&mut manifests, Some("prod")).unwrap();
            assert!(warnings.is_empty(), "{annotation}: {warnings:?}");
            let written = manifests[0].pointer("/spec/template/metadata/annotations").unwrap();
            assert_eq!(
                written[ANNOTATION_CHECKSUM_SHA256],
                checksum_of("ConfigMap/cfg", vec![config_map("cfg", json!({"k": "v"}))]),
                "{annotation}"
            );
            // In the Release's namespace instead, the reference does not resolve.
            let mut manifests = in_prod(annotation);
            let outcome = apply_in(&mut manifests, Some("glance"));
            assert_eq!(outcome.is_err(), annotation != "auto", "{annotation}");
        }
    }

    #[test]
    fn test_a_digest_written_by_an_earlier_render_is_recomputed() {
        let sources = || vec![config_map("cfg", json!({"k": "v"}))];
        let mut once = vec![explicit("ConfigMap/cfg")];
        once.extend(sources());
        apply_in(&mut once, Some("app")).unwrap();
        let mut again = once.clone();
        apply_in(&mut again, Some("app")).unwrap();
        assert_eq!(again, once);

        // A stale digest is overwritten, so rendering Nyl's own output after an edit is correct.
        let mut stale = once.clone();
        stale[1]["data"]["k"] = json!("edited");
        apply_in(&mut stale, Some("app")).unwrap();
        assert_ne!(stale[0], once[0]);
    }

    #[test]
    fn test_a_partial_render_refuses_a_checksum_it_cannot_compute() {
        let mut manifests = vec![explicit("auto")];
        let error = apply(
            &mut manifests,
            &Options {
                default_namespace: Some("app"),
                partial: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), PARTIAL);
        assert!(error.into_nyl().to_string().contains("Drop `--only-source-kind`"));

        // Without an opt-in there is nothing to compute, and a partial render is fine.
        let mut plain = vec![config_map("cfg", json!({"k": "v"}))];
        assert!(apply(
            &mut plain,
            &Options {
                default_namespace: None,
                partial: true
            }
        )
        .is_ok());
    }

    #[test]
    fn test_base64_may_contain_line_breaks_as_kubernetes_allows() {
        let plain = secret("s", json!({"data": {"p": b64(b"password")}}));
        for wrapped in ["cGFzc3dvcmQ=\n", "cGFz\r\nc3dvcmQ=", "cGFzc3dv\ncmQ=\n"] {
            let digest = checksum_of("Secret/s", vec![secret("s", json!({"data": {"p": wrapped}}))]);
            assert_eq!(digest, checksum_of("Secret/s", vec![plain.clone()]), "{wrapped:?}");
        }
    }

    #[test]
    fn test_empty_items_select_every_key_like_kubernetes() {
        let spec = |items: Value| json!({"containers": [], "volumes": [{"name": "v", "configMap": {"name": "cfg", "items": items}}]});
        let sources = || vec![config_map("cfg", json!({"a": "1", "b": "2"}))];
        let digest = |items: Value| {
            let mut manifests = vec![deployment("auto", spec(items))];
            manifests.extend(sources());
            run(manifests).unwrap()
        };
        assert_eq!(digest(json!([])), checksum_of("ConfigMap/cfg", sources()));
        assert_eq!(digest(json!([{"path": "x"}])), checksum_of("ConfigMap/cfg", sources()));
        assert_eq!(
            digest(json!([{"key": "a", "path": "x"}])),
            checksum_of("ConfigMap/cfg#a", sources())
        );
    }

    #[test]
    fn test_auto_that_selects_nothing_warns_with_its_code() {
        let mut manifests = vec![explicit("auto"), config_map("unreferenced", json!({"k": "v"}))];
        let warnings = apply_in(&mut manifests, Some("app")).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("NYL-KUBERNETES-CHECKSUM-EMPTY: Deployment app/web"),
            "{warnings:?}"
        );
        // An explicit list that selects nothing is the author's own statement.
        let mut manifests = vec![explicit("ConfigMap/empty"), config_map("empty", json!({}))];
        assert!(apply_in(&mut manifests, Some("app")).unwrap().is_empty());
    }
}
