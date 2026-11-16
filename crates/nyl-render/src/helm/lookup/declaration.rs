//! Where Helm `lookup` is turned on, and the warning for a chart that calls it anyway.
//!
//! A HelmChart declares `spec.lookup: true` (on) or `spec.lookup: false` (off). A
//! Component, whose `spec` is passed verbatim as chart values, has two layers:
//! the `k8s.nyl/helm-lookup` annotation (`"true"` / `"false"`) on the invocation,
//! which wins, and the `lookup` field of the component's `nyl-component.yaml`.
//! The effective setting is a [`Option<bool>`]: `Some(true)` renders against the
//! lookup API, `Some(false)` renders without lookup and says so deliberately, and
//! `None` is undeclared, which warns with [`UNDECLARED_CODE`] when a template of
//! the chart (or of a subchart or library chart) calls `lookup`.
//!
//! Detecting a call is a scan of template sources, a heuristic. An action such as
//! `{{ lookup ... }}` is a call when the identifier `lookup` is used as a function
//! outside comments, strings, and field accesses such as `.Values.lookup`. A call
//! outside any `{{ define }}` always counts. A call inside `{{ define "X" }}` counts
//! only when "X" is reachable: referenced by `include "X"`, `template "X"` or
//! `block "X"` from template content outside any define, or from a define that is
//! itself reachable, across the chart and its subcharts (Helm shares one namespace
//! of defines). That keeps a library chart such as Bitnami's `common`, which
//! defines lookup helpers a chart may never include, from warning every chart that
//! depends on it. A name built at render time (`include (print ...)`), a chart
//! whose templates the scan cannot read, and a subchart a `condition` disables are
//! not modelled.
//!
//! User documentation: `kubernetes-rendering/resource-guides/helm-chart.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use nyl_core::constants::ANNOTATION_HELM_LOOKUP;

use super::{ANNOTATION_INVALID_CODE, ANNOTATION_MISPLACED_CODE, UNDECLARED_CODE};
use crate::resources::LookupSetting;
use crate::{NylError, Result};

/// The resource whose chart is rendered, for the diagnostics that name where to opt in.
#[derive(Clone, Copy, Debug)]
pub enum LookupOwner<'a> {
    HelmChart { name: &'a str },
    Component { kind: &'a str, name: &'a str },
}

impl LookupOwner<'_> {
    fn describe(&self) -> String {
        match self {
            Self::HelmChart { name } => format!("HelmChart '{name}'"),
            Self::Component { kind, name } => format!("Component '{name}' ({kind})"),
        }
    }
}

/// The effective lookup setting of a HelmChart: `spec.lookup`. The annotation
/// belongs to Components, so one on a HelmChart is refused with where to declare it.
pub fn helm_chart_lookup(
    name: &str,
    annotations: &HashMap<String, String>,
    spec_lookup: Option<&LookupSetting>,
) -> Result<Option<bool>> {
    if annotations.contains_key(ANNOTATION_HELM_LOOKUP) {
        return Err(NylError::Coded {
            code: ANNOTATION_MISPLACED_CODE,
            message: format!(
                "HelmChart '{name}' sets the {ANNOTATION_HELM_LOOKUP} annotation, which only Components take"
            ),
            fix: "Declare `spec.lookup: true` (or `spec.lookup: false`) on the HelmChart and remove the annotation."
                .to_owned(),
        });
    }
    Ok(spec_lookup.map(LookupSetting::is_enabled))
}

/// Refuses a `k8s.nyl/helm-lookup` annotation that is not a string, such as the unquoted
/// `k8s.nyl/helm-lookup: true`, before the generic annotation parse reports it without a code.
pub fn check_annotation_type(resource: &serde_json::Value) -> Result<()> {
    let Some(value) = resource
        .pointer("/metadata/annotations")
        .and_then(|annotations| annotations.get(ANNOTATION_HELM_LOOKUP))
    else {
        return Ok(());
    };
    if value.is_string() {
        return Ok(());
    }
    let name = resource
        .pointer("/metadata/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unnamed>");
    Err(NylError::Coded {
        code: ANNOTATION_INVALID_CODE,
        message: format!("Resource '{name}' sets {ANNOTATION_HELM_LOOKUP} to {value}, which is not a string"),
        fix: format!("Set {ANNOTATION_HELM_LOOKUP} to \"true\" or \"false\" (quoted, so YAML keeps it a string)."),
    })
}

/// The effective lookup setting of a Component invocation: its annotation if
/// it has one, else the `lookup` of the component's `nyl-component.yaml`.
pub fn component_lookup(
    name: &str,
    annotations: &HashMap<String, String>,
    contract_lookup: Option<bool>,
) -> Result<Option<bool>> {
    let annotation = match annotations.get(ANNOTATION_HELM_LOOKUP).map(String::as_str) {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(other) => {
            return Err(NylError::Coded {
                code: ANNOTATION_INVALID_CODE,
                message: format!("Component '{name}' sets {ANNOTATION_HELM_LOOKUP} to {other:?}"),
                fix: format!(
                    "Set {ANNOTATION_HELM_LOOKUP} to \"true\" or \"false\" (quoted, so YAML keeps it a string)."
                ),
            })
        }
    };
    Ok(annotation.or(contract_lookup))
}

/// Warns with [`UNDECLARED_CODE`] when `lookup` is undeclared (`setting` is
/// `None`) and a reachable template of the chart at `chart_path` (a chart
/// directory or a packaged `.tgz`) calls it. `fallback_name` names the chart when
/// its `Chart.yaml` cannot be read.
///
/// The warning is given once per owner and chart for the process, and a chart's
/// scan is shared by every owner that renders it.
pub fn warn_if_undeclared(owner: LookupOwner<'_>, fallback_name: &str, chart_path: &Path, setting: Option<bool>) {
    if setting.is_some() {
        return;
    }
    let owner_text = owner.describe();
    if !first_check(&owner_text, chart_path) {
        return;
    }
    let scan = cached_scan(chart_path);
    if scan.calls.is_empty() {
        return;
    }
    let chart = scan.chart_name.as_deref().unwrap_or(fallback_name);
    let shown: Vec<String> = scan.calls.iter().take(3).map(LookupCall::describe).collect();
    let more = scan.calls.len() - shown.len();
    let mut found = shown.join("; ");
    if more > 0 {
        found = format!("{found}; and {more} more");
    }
    let how = match owner {
        LookupOwner::HelmChart { .. } => {
            "Set `spec.lookup: true` on the HelmChart to let it read the Release's objects, or `spec.lookup: false` \
             to keep rendering without lookup and silence this warning."
        }
        LookupOwner::Component { .. } => {
            "Enable it with the annotation `k8s.nyl/helm-lookup: \"true\"` on the Component, or `lookup: true` in the \
             component's nyl-component.yaml; `k8s.nyl/helm-lookup: \"false\"` or `lookup: false` keeps rendering \
             without lookup and silences this warning."
        }
    };
    tracing::warn!(
        "{UNDECLARED_CODE}: {owner_text} renders chart '{chart}', whose templates appear to call Helm's `lookup` \
         ({found}), but lookup is not enabled, so it finds nothing. {how} (Calls are detected by scanning template \
         sources, so this can miss or over-report.)"
    );
}

type ScanKey = (PathBuf, Option<(u64, std::time::SystemTime)>);

static SCANS: LazyLock<Mutex<HashMap<ScanKey, Arc<ChartScan>>>> = LazyLock::new(Mutex::default);
static CHECKED: LazyLock<Mutex<HashSet<(String, PathBuf)>>> = LazyLock::new(Mutex::default);

/// Whether this owner has not yet had this chart checked in this process.
fn first_check(owner: &str, chart_path: &Path) -> bool {
    CHECKED.lock().map_or(true, |mut checked| {
        checked.insert((owner.to_owned(), chart_path.to_path_buf()))
    })
}

/// The scan of a chart, shared per resolved path. A packaged chart is keyed by its size and
/// modification time; a chart directory is scanned once per process, which suits a command
/// that renders and exits.
fn cached_scan(chart_path: &Path) -> Arc<ChartScan> {
    let stamp = std::fs::metadata(chart_path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)));
    let key = (chart_path.to_path_buf(), stamp);
    if let Some(scan) = SCANS.lock().ok().and_then(|scans| scans.get(&key).cloned()) {
        return scan;
    }
    let scan = Arc::new(scan_chart(chart_path));
    if let Ok(mut scans) = SCANS.lock() {
        scans.insert(key, Arc::clone(&scan));
    }
    scan
}

const MAX_TEMPLATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 8;

/// What kind of chart holds a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChartKind {
    /// The chart being rendered.
    Root,
    /// A dependency under `charts/`.
    Subchart,
    /// A chart of `type: library`, at any depth.
    Library,
}

/// A reachable call of `lookup` in a chart's templates.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LookupCall {
    /// The template file, relative to the scanned chart; `!/` marks a packaged chart.
    pub path: String,
    /// The chart that holds the template.
    pub chart: String,
    pub kind: ChartKind,
    /// The named template (`define`) the call is in, or `None` for template content outside any.
    pub template: Option<String>,
}

impl LookupCall {
    fn describe(&self) -> String {
        let chart = match self.kind {
            ChartKind::Root => None,
            ChartKind::Subchart => Some(format!("subchart {}", self.chart)),
            ChartKind::Library => Some(format!("library chart {}", self.chart)),
        };
        let place = chart
            .into_iter()
            .chain(self.template.as_ref().map(|template| format!("template {template}")))
            .collect::<Vec<_>>()
            .join(", ");
        if place.is_empty() {
            self.path.clone()
        } else {
            format!("in {place} ({})", self.path)
        }
    }
}

/// The result of scanning a chart for `lookup` calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChartScan {
    /// The `name` of the scanned chart's `Chart.yaml`, when it could be read.
    pub chart_name: Option<String>,
    /// The reachable calls, in path order.
    pub calls: Vec<LookupCall>,
}

/// Scans the chart at `chart_path`, a directory or a packaged `.tgz`, for reachable calls of
/// `lookup` in its templates and those of its subcharts (directories and `.tgz` under
/// `charts/`, library charts among them). Symlinks in a chart directory are followed.
/// Anything that cannot be read is skipped.
pub fn scan_chart(chart_path: &Path) -> ChartScan {
    let mut collector = Collector::default();
    let packaged = std::fs::metadata(chart_path).is_ok_and(|metadata| metadata.is_file());
    if packaged {
        let mut bytes = Vec::new();
        let read =
            std::fs::File::open(chart_path).and_then(|file| file.take(MAX_ARCHIVE_BYTES).read_to_end(&mut bytes));
        if read.is_ok() {
            collector.scan_archive(&bytes, "", true, 0);
        }
    } else {
        collector.scan_dir(chart_path, "", true, 0);
    }
    collector.finish()
}

fn is_archive(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tgz"))
}

#[derive(serde::Deserialize, Default)]
struct ChartMeta {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn parse_meta(text: &str) -> ChartMeta {
    serde_saphyr::from_str(text).unwrap_or_default()
}

#[derive(Clone)]
struct ChartId {
    name: String,
    kind: ChartKind,
}

impl ChartId {
    fn new(meta: &ChartMeta, fallback: &str, root: bool) -> Self {
        let kind = if meta.kind.as_deref() == Some("library") {
            ChartKind::Library
        } else if root {
            ChartKind::Root
        } else {
            ChartKind::Subchart
        };
        Self {
            name: meta.name.clone().unwrap_or_else(|| fallback.to_owned()),
            kind,
        }
    }
}

/// Gathers calls and references across every template of a chart, then keeps the calls
/// that are reachable.
#[derive(Default)]
struct Collector {
    root_name: Option<String>,
    visited: HashSet<PathBuf>,
    /// Calls outside any define.
    top: BTreeSet<LookupCall>,
    /// Calls inside each named template.
    defined: BTreeMap<String, BTreeSet<LookupCall>>,
    /// Names referenced from outside any define.
    roots: BTreeSet<String>,
    /// Names each define references.
    edges: BTreeMap<String, BTreeSet<String>>,
}

impl Collector {
    fn add_source(&mut self, chart: &ChartId, path: &str, source: &str) {
        let call = |template: Option<&str>| LookupCall {
            path: path.to_owned(),
            chart: chart.name.clone(),
            kind: chart.kind,
            template: template.map(str::to_owned),
        };
        let mut calls = Vec::new();
        analyze(source, |event| match event {
            Event::Lookup { context } => calls.push(call(context)),
            Event::Reference { from: None, to } => {
                self.roots.insert(to.to_owned());
            }
            Event::Reference { from: Some(from), to } => {
                self.edges.entry(from.to_owned()).or_default().insert(to.to_owned());
            }
        });
        for call in calls {
            match call.template.clone() {
                None => self.top.insert(call),
                Some(template) => self.defined.entry(template).or_default().insert(call),
            };
        }
    }

    fn scan_dir(&mut self, dir: &Path, prefix: &str, root: bool, depth: usize) {
        if depth > MAX_DEPTH || !self.first_visit(dir) {
            return;
        }
        let meta = std::fs::read_to_string(dir.join("Chart.yaml")).map(|text| parse_meta(&text));
        let meta = meta.unwrap_or_default();
        let fallback = dir.file_name().map_or_else(
            || "<unknown-chart>".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let chart = ChartId::new(&meta, &fallback, root);
        if root {
            self.root_name.clone_from(&meta.name);
        }
        self.scan_templates(&dir.join("templates"), &format!("{prefix}templates"), &chart, 0);
        let Ok(entries) = std::fs::read_dir(dir.join("charts")) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `metadata` follows symlinks, so a linked subchart is scanned as well.
            let Ok(metadata) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() {
                self.scan_dir(&entry.path(), &format!("{prefix}charts/{name}/"), false, depth + 1);
            } else if metadata.is_file() && is_archive(&name) {
                let mut bytes = Vec::new();
                let read = std::fs::File::open(entry.path())
                    .and_then(|file| file.take(MAX_ARCHIVE_BYTES).read_to_end(&mut bytes));
                if read.is_ok() {
                    self.scan_archive(&bytes, &format!("{prefix}charts/{name}!/"), false, depth + 1);
                }
            }
        }
    }

    /// Whether a directory is seen for the first time, so symlink loops end.
    fn first_visit(&mut self, dir: &Path) -> bool {
        std::fs::canonicalize(dir).map_or(true, |real| self.visited.insert(real))
    }

    fn scan_templates(&mut self, dir: &Path, label: &str, chart: &ChartId, depth: usize) {
        if depth > 32 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(metadata) = std::fs::metadata(entry.path()) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if metadata.is_dir() {
                if self.first_visit(&entry.path()) {
                    self.scan_templates(&entry.path(), &format!("{label}/{name}"), chart, depth + 1);
                }
            } else if metadata.is_file() {
                let mut source = Vec::new();
                let read = std::fs::File::open(entry.path())
                    .and_then(|file| file.take(MAX_TEMPLATE_BYTES).read_to_end(&mut source));
                if read.is_ok() {
                    self.add_source(chart, &format!("{label}/{name}"), &String::from_utf8_lossy(&source));
                }
            }
        }
    }

    fn scan_archive(&mut self, bytes: &[u8], label: &str, root: bool, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let open = || tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        // Chart.yaml may come after the templates of its chart, so read them first.
        let mut metas: HashMap<String, ChartMeta> = HashMap::new();
        let mut first = open();
        let Ok(entries) = first.entries() else {
            return;
        };
        for mut entry in entries.flatten() {
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let Ok(path) = entry.path().map(|path| path.to_string_lossy().into_owned()) else {
                continue;
            };
            let Some(directory) = path
                .strip_suffix("Chart.yaml")
                .filter(|d| d.is_empty() || d.ends_with('/'))
            else {
                continue;
            };
            let mut text = String::new();
            if (&mut entry).take(MAX_TEMPLATE_BYTES).read_to_string(&mut text).is_ok() {
                let meta = parse_meta(&text);
                if root && directory.matches('/').count() <= 1 {
                    self.root_name.clone_from(&meta.name);
                }
                metas.insert(directory.to_owned(), meta);
            }
        }
        let mut second = open();
        let Ok(entries) = second.entries() else {
            return;
        };
        for mut entry in entries.flatten() {
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let Ok(path) = entry.path().map(|path| path.to_string_lossy().into_owned()) else {
                continue;
            };
            let parts: Vec<&str> = path.split('/').collect();
            let nested = is_archive(&path) && parts.len() >= 2 && parts[parts.len() - 2] == "charts";
            // The chart owning a template is the one whose `templates/` it sits in: the top
            // directory of the archive, or a `charts/<name>` below it.
            let owner = parts
                .iter()
                .enumerate()
                .find(|(index, part)| {
                    **part == "templates" && (*index <= 1 || (*index >= 3 && parts[index - 2] == "charts"))
                })
                .map(|(index, _)| index);
            if !nested && owner.is_none() {
                continue;
            }
            let mut content = Vec::new();
            let limit = if nested { MAX_ARCHIVE_BYTES } else { MAX_TEMPLATE_BYTES };
            if (&mut entry).take(limit).read_to_end(&mut content).is_err() {
                continue;
            }
            if nested {
                self.scan_archive(&content, &format!("{label}{path}!/"), false, depth + 1);
            } else if let Some(index) = owner {
                let directory = if index == 0 {
                    String::new()
                } else {
                    format!("{}/", parts[..index].join("/"))
                };
                let fallback = parts.get(index.wrapping_sub(1)).copied().unwrap_or("<unknown-chart>");
                let meta = metas.get(&directory);
                let chart = ChartId::new(meta.unwrap_or(&ChartMeta::default()), fallback, root && index <= 1);
                self.add_source(&chart, &format!("{label}{path}"), &String::from_utf8_lossy(&content));
            }
        }
    }

    fn finish(self) -> ChartScan {
        let mut reachable: BTreeSet<&str> = BTreeSet::new();
        let mut pending: Vec<&str> = self.roots.iter().map(String::as_str).collect();
        while let Some(name) = pending.pop() {
            if reachable.insert(name) {
                pending.extend(self.edges.get(name).into_iter().flatten().map(String::as_str));
            }
        }
        let mut calls = self.top.clone();
        for (name, found) in &self.defined {
            if reachable.contains(name.as_str()) {
                calls.extend(found.iter().cloned());
            }
        }
        ChartScan {
            chart_name: self.root_name.clone(),
            calls: calls.into_iter().collect(),
        }
    }
}

enum Event<'a> {
    /// A call of `lookup`, inside the named template or outside any.
    Lookup { context: Option<&'a str> },
    /// A template referenced by `include`, `template` or `block`.
    Reference { from: Option<&'a str>, to: &'a str },
}

enum Token {
    Word { text: String, access: bool },
    Str(String),
}

/// Whether template source calls Helm's `lookup` function: the identifier
/// `lookup` inside a `{{ ... }}` action, outside string literals and comments,
/// and not a field or variable (`.Values.lookup`, `$lookup`).
pub fn template_calls_lookup(source: &str) -> bool {
    let mut called = false;
    analyze(source, |event| called |= matches!(event, Event::Lookup { .. }));
    called
}

/// Walks the actions of one template source, tracking which `define` each sits in.
fn analyze(source: &str, mut event: impl FnMut(Event<'_>)) {
    let bytes = source.as_bytes();
    // The enclosing define of each open block (`if`, `range`, `with`, `define`, `block`).
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut index = 0;
    while let Some(start) = find(bytes, index, b"{{") {
        let mut cursor = start + 2;
        if bytes.get(cursor) == Some(&b'-') && bytes.get(cursor + 1).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes[cursor..].starts_with(b"/*") {
            // A comment ends at `*/`, then the action's closing delimiter.
            match find(bytes, cursor + 2, b"*/") {
                Some(end) => {
                    index = find(bytes, end + 2, b"}}").map_or(bytes.len(), |close| close + 2);
                    continue;
                }
                None => return,
            }
        }
        let (tokens, end) = read_action(bytes, cursor);
        index = end;
        let context = stack.last().cloned().flatten();
        let context = context.as_deref();
        if tokens
            .iter()
            .any(|token| matches!(token, Token::Word { text, access: false } if text == "lookup"))
        {
            event(Event::Lookup { context });
        }
        for pair in tokens.windows(2) {
            if let [Token::Word { text, access: false }, Token::Str(name)] = pair {
                if text == "include" || text == "template" {
                    event(Event::Reference {
                        from: context,
                        to: name,
                    });
                }
            }
        }
        let name = match tokens.get(1) {
            Some(Token::Str(name)) => Some(name.as_str()),
            _ => None,
        };
        if let Some(Token::Word { text, access: false }) = tokens.first() {
            match text.as_str() {
                "define" => stack.push(name.map(str::to_owned).or_else(|| context.map(str::to_owned))),
                "block" => {
                    if let Some(name) = name {
                        event(Event::Reference {
                            from: context,
                            to: name,
                        });
                    }
                    stack.push(name.map(str::to_owned).or_else(|| context.map(str::to_owned)));
                }
                "if" | "range" | "with" => stack.push(context.map(str::to_owned)),
                "end" => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
}

/// Reads the tokens of one action body from `from`; returns them and where the action ends.
fn read_action(bytes: &[u8], from: usize) -> (Vec<Token>, usize) {
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut tokens = Vec::new();
    let mut cursor = from;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'}' if bytes.get(cursor + 1) == Some(&b'}') => return (tokens, cursor + 2),
            quote @ (b'"' | b'\'') => {
                let begin = cursor + 1;
                cursor = begin;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += if bytes[cursor] == b'\\' { 2 } else { 1 };
                }
                let end = cursor.min(bytes.len());
                tokens.push(Token::Str(String::from_utf8_lossy(&bytes[begin..end]).into_owned()));
                cursor += 1;
            }
            b'`' => {
                let begin = cursor + 1;
                let end = find(bytes, begin, b"`").unwrap_or(bytes.len());
                tokens.push(Token::Str(String::from_utf8_lossy(&bytes[begin..end]).into_owned()));
                cursor = end + 1;
            }
            byte if is_word(byte) => {
                let begin = cursor;
                while cursor < bytes.len() && is_word(bytes[cursor]) {
                    cursor += 1;
                }
                tokens.push(Token::Word {
                    text: String::from_utf8_lossy(&bytes[begin..cursor]).into_owned(),
                    access: begin > 0 && matches!(bytes[begin - 1], b'.' | b'$'),
                });
            }
            _ => cursor += 1,
        }
    }
    (tokens, bytes.len())
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotations(value: &str) -> HashMap<String, String> {
        HashMap::from([(ANNOTATION_HELM_LOOKUP.to_owned(), value.to_owned())])
    }

    #[test]
    fn scanner_matches_lookup_used_as_a_function() {
        for source in [
            r#"{{ $s := lookup "v1" "Secret" .Release.Namespace "ca" }}"#,
            r#"{{- $s := (lookup "v1" "Secret" "ns" "ca") -}}"#,
            r#"{{if lookup "v1" "Secret" "ns" "ca"}}x{{end}}"#,
            "{{ include \"x\" . }}\n{{ with lookup \"v1\" \"ConfigMap\" \"a\" \"b\" }}{{ end }}",
            r#"{{ "x" | lookup }}"#,
        ] {
            assert!(template_calls_lookup(source), "{source}");
        }
    }

    #[test]
    fn scanner_ignores_what_is_not_a_call() {
        for source in [
            "{{/* lookup \"v1\" \"Secret\" */}}",
            "{{- /* lookup is documented here }} */ -}}\nkind: ConfigMap",
            "{{ .Values.lookup }} {{ $lookup }} {{ $.Values.lookup.enabled }}",
            r#"{{ include "lookup" . }} {{ dict "lookup" 1 }} {{ `lookup` }}"#,
            "# lookup is mentioned in YAML outside an action\nkind: ConfigMap",
            "{{ lookups }} {{ mylookup }} {{ lookup_x }}",
        ] {
            assert!(!template_calls_lookup(source), "{source}");
        }
    }

    #[test]
    fn scanner_resumes_after_a_comment() {
        assert!(template_calls_lookup(
            "{{/* a */}}{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}"
        ));
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, path, *content).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoder, &tar).unwrap();
        encoder.finish().unwrap()
    }

    const LOOKUP: &str = "{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}";

    fn paths(scan: &ChartScan) -> Vec<&str> {
        scan.calls.iter().map(|call| call.path.as_str()).collect()
    }

    #[test]
    fn templates_are_found_in_the_chart_its_subcharts_and_library_charts() {
        let temp = tempfile::TempDir::new().unwrap();
        let chart = temp.path();
        write(&chart.join("Chart.yaml"), "name: root\n");
        write(
            &chart.join("templates/plain.yaml"),
            "kind: ConfigMap\n{{ include \"x\" . }}{{ include \"l\" . }}",
        );
        write(
            &chart.join("templates/_helpers.tpl"),
            &format!("{{{{ define \"x\" }}}}{LOOKUP}{{{{ end }}}}"),
        );
        write(&chart.join("charts/unpacked/templates/deep/secret.yaml"), LOOKUP);
        write(
            &chart.join("charts/clean/templates/cm.yaml"),
            "{{/* lookup */}}kind: ConfigMap",
        );
        write(
            &chart.join("charts/unpacked/charts/lib/Chart.yaml"),
            "name: lib\ntype: library\n",
        );
        write(
            &chart.join("charts/unpacked/charts/lib/templates/_lib.tpl"),
            &format!("{{{{ define \"l\" }}}}{LOOKUP}{{{{ end }}}}"),
        );
        let inner = archive(&[("inner/templates/a.yaml", LOOKUP.as_bytes())]);
        let packed = archive(&[
            ("packed/Chart.yaml", b"name: packed"),
            ("packed/templates/s.yaml", LOOKUP.as_bytes()),
            ("packed/templates/clean.yaml", b"kind: ConfigMap"),
            ("packed/charts/inner-1.0.0.tgz", &inner),
        ]);
        std::fs::write(chart.join("charts/packed-1.0.0.tgz"), packed).unwrap();

        let scan = scan_chart(chart);
        assert_eq!(scan.chart_name.as_deref(), Some("root"));
        assert_eq!(
            paths(&scan),
            [
                "charts/packed-1.0.0.tgz!/packed/charts/inner-1.0.0.tgz!/inner/templates/a.yaml",
                "charts/packed-1.0.0.tgz!/packed/templates/s.yaml",
                "charts/unpacked/charts/lib/templates/_lib.tpl",
                "charts/unpacked/templates/deep/secret.yaml",
                "templates/_helpers.tpl",
            ]
        );
        let library = &scan.calls[2];
        assert_eq!((library.kind, library.chart.as_str()), (ChartKind::Library, "lib"));
        assert_eq!(library.template.as_deref(), Some("l"));
        assert!(library.describe().starts_with("in library chart lib, template l ("));
    }

    /// A packaged chart, as a repository or OCI pull leaves it, scans like a directory: the
    /// chart name comes from its Chart.yaml (even when that comes last), not the cache file name.
    #[test]
    fn a_packaged_chart_is_scanned_and_named_by_its_chart_yaml() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("1a2b3c-1.0.0-deadbeef.tgz");
        std::fs::write(
            &path,
            archive(&[
                ("cilium/templates/ca.yaml", LOOKUP.as_bytes()),
                ("cilium/templates/clean.yaml", b"kind: ConfigMap"),
                ("cilium/Chart.yaml", b"name: cilium\nversion: 1.0.0\n"),
            ]),
        )
        .unwrap();
        let scan = scan_chart(&path);
        assert_eq!(scan.chart_name.as_deref(), Some("cilium"));
        assert_eq!(paths(&scan), ["cilium/templates/ca.yaml"]);
        assert_eq!(scan.calls[0].kind, ChartKind::Root);
        assert_eq!(scan.calls[0].chart, "cilium");

        let clean = temp.path().join("clean.tgz");
        std::fs::write(
            &clean,
            archive(&[("c/Chart.yaml", b"name: c"), ("c/templates/a.yaml", b"x: 1")]),
        )
        .unwrap();
        assert!(scan_chart(&clean).calls.is_empty());
        assert!(scan_chart(&temp.path().join("missing.tgz")).calls.is_empty());
    }

    /// The Bitnami shape: `common` defines a lookup helper next to helpers that do not call it.
    #[test]
    fn a_library_helper_counts_only_where_it_is_reached() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = |dir: &Path| {
            write(&dir.join("charts/common/Chart.yaml"), "name: common\ntype: library\n");
            write(
                &dir.join("charts/common/templates/_secrets.tpl"),
                &format!(
                    "{{{{- define \"common.secrets.passwords.manage\" -}}}}\n{{{{- $s := (lookup \"v1\" \"Secret\" \
                     \"n\" \"s\") -}}}}\n{{{{- include \"common.secrets.inner\" . -}}}}\n{{{{- end -}}}}\n\
                     {{{{- define \"common.secrets.inner\" -}}}}{LOOKUP}{{{{- end -}}}}"
                ),
            );
            write(
                &dir.join("charts/common/templates/_names.tpl"),
                "{{- define \"common.names.fullname\" -}}x{{- end -}}",
            );
        };
        let uses = temp.path().join("uses");
        write(&uses.join("Chart.yaml"), "name: uses\n");
        library(&uses);
        write(
            &uses.join("templates/secret.yaml"),
            "{{- if .Values.enabled }}\nvalue: {{ include \"common.secrets.passwords.manage\" . }}\n{{- end }}",
        );
        let scan = scan_chart(&uses);
        let templates: Vec<_> = scan
            .calls
            .iter()
            .map(|call| call.template.as_deref().unwrap())
            .collect();
        assert_eq!(templates, ["common.secrets.inner", "common.secrets.passwords.manage"]);
        assert!(scan.calls.iter().all(|call| call.kind == ChartKind::Library));

        let skips = temp.path().join("skips");
        write(&skips.join("Chart.yaml"), "name: skips\n");
        library(&skips);
        write(
            &skips.join("templates/cm.yaml"),
            "name: {{ template \"common.names.fullname\" . }}\n",
        );
        assert!(scan_chart(&skips).calls.is_empty());
    }

    #[test]
    fn reachability_follows_chains_blocks_and_cycles_and_top_level_calls_always_count() {
        let temp = tempfile::TempDir::new().unwrap();
        let chart = temp.path();
        write(
            &chart.join("templates/_h.tpl"),
            &format!(
                "{{{{ define \"a\" }}}}{{{{ if true }}}}{{{{ include \"b\" . }}}}{{{{ end }}}}{{{{ end }}}}\
                 {{{{ define \"b\" }}}}{{{{ template \"a\" . }}}}{{{{ include \"c\" . }}}}{{{{ end }}}}\
                 {{{{ define \"c\" }}}}{LOOKUP}{{{{ end }}}}\
                 {{{{ define \"d\" }}}}{LOOKUP}{{{{ end }}}}"
            ),
        );
        write(&chart.join("templates/main.yaml"), "{{ block \"a\" . }}{{ end }}");
        let scan = scan_chart(chart);
        let templates: Vec<_> = scan.calls.iter().map(|call| call.template.clone()).collect();
        assert_eq!(templates, [Some("c".to_owned())], "d is defined and never included");

        write(&chart.join("templates/direct.yaml"), LOOKUP);
        assert_eq!(scan_chart(chart).calls.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_charts_and_templates_are_followed() {
        let temp = tempfile::TempDir::new().unwrap();
        let shared = temp.path().join("shared");
        write(&shared.join("templates/a.yaml"), LOOKUP);
        let chart = temp.path().join("chart");
        write(&chart.join("Chart.yaml"), "name: chart\n");
        std::fs::create_dir_all(chart.join("charts")).unwrap();
        std::os::unix::fs::symlink(&shared, chart.join("charts/linked")).unwrap();
        let extra = temp.path().join("extra");
        write(&extra.join("b.yaml"), LOOKUP);
        std::fs::create_dir_all(chart.join("templates")).unwrap();
        std::os::unix::fs::symlink(&extra, chart.join("templates/more")).unwrap();
        // A loop must end.
        std::os::unix::fs::symlink(&chart, extra.join("loop")).unwrap();
        let scan = scan_chart(&chart);
        assert_eq!(
            paths(&scan),
            ["charts/linked/templates/a.yaml", "templates/more/b.yaml"]
        );
    }

    #[test]
    fn a_chart_is_checked_once_per_owner() {
        let path = Path::new("/nowhere/chart-once");
        assert!(first_check("HelmChart 'a'", path));
        assert!(!first_check("HelmChart 'a'", path));
        assert!(first_check("HelmChart 'b'", path));
        assert!(first_check("HelmChart 'a'", Path::new("/nowhere/other")));
    }

    #[test]
    fn a_chart_without_lookup_calls_reports_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        write(
            &temp.path().join("templates/cm.yaml"),
            "data: {{ .Values.lookup | quote }}\n",
        );
        write(&temp.path().join("values.yaml"), "lookup: true\n");
        assert!(scan_chart(temp.path()).calls.is_empty());
    }

    #[test]
    fn a_component_annotation_wins_over_nyl_component_yaml() {
        let (on, off) = (true, false);
        assert_eq!(component_lookup("c", &HashMap::new(), None).unwrap(), None);
        assert_eq!(component_lookup("c", &HashMap::new(), Some(on)).unwrap(), Some(true));
        assert_eq!(component_lookup("c", &HashMap::new(), Some(off)).unwrap(), Some(false));
        assert_eq!(component_lookup("c", &annotations("true"), None).unwrap(), Some(true));
        assert_eq!(
            component_lookup("c", &annotations("false"), Some(on)).unwrap(),
            Some(false)
        );
        assert_eq!(
            component_lookup("c", &annotations("true"), Some(off)).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn an_annotation_that_is_not_true_or_false_is_refused_with_its_code() {
        for value in ["yes", "True", "", "{}"] {
            let error = component_lookup("c", &annotations(value), Some(true)).unwrap_err();
            assert!(
                matches!(&error, NylError::Coded { code, .. } if *code == ANNOTATION_INVALID_CODE),
                "{value}: {error}"
            );
            assert!(error.to_string().contains("\"true\" or \"false\""), "{error}");
        }
    }

    #[test]
    fn a_helm_chart_declares_lookup_in_its_spec_and_refuses_the_annotation() {
        assert_eq!(helm_chart_lookup("h", &HashMap::new(), None).unwrap(), None);
        assert_eq!(
            helm_chart_lookup("h", &HashMap::new(), Some(&LookupSetting::DISABLED)).unwrap(),
            Some(false)
        );
        let error = helm_chart_lookup("h", &annotations("true"), None).unwrap_err();
        assert!(
            matches!(&error, NylError::Coded { code, .. } if *code == ANNOTATION_MISPLACED_CODE),
            "{error}"
        );
        assert!(error.to_string().contains("spec.lookup"), "{error}");
    }
}
