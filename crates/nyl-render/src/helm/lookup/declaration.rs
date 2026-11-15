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
//! Detecting a call is a scan of template sources, a heuristic: [`template_calls_lookup`]
//! matches the identifier `lookup` used as a function inside `{{ ... }}` actions and
//! skips comments, strings, and field accesses such as `.Values.lookup`.
//! A template that builds the call dynamically, or a chart whose
//! templates the scan cannot read, is not reported.
//!
//! User documentation: `kubernetes-rendering/resource-guides/helm-chart.md`.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use nyl_core::constants::ANNOTATION_HELM_LOOKUP;

use super::{ANNOTATION_INVALID_CODE, ANNOTATION_MISPLACED_CODE, OBJECT_DEPRECATED_CODE, UNDECLARED_CODE};
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
    if spec_lookup.is_some_and(LookupSetting::is_legacy_object) {
        tracing::warn!(
            "{OBJECT_DEPRECATED_CODE}: HelmChart '{name}' declares `spec.lookup: {{}}`, which is deprecated. Write \
             `spec.lookup: true`; `{{}}` still means the same for now."
        );
    }
    Ok(spec_lookup.map(LookupSetting::is_enabled))
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
/// `None`) and a template of the chart at `chart_dir` calls it.
pub fn warn_if_undeclared(owner: LookupOwner<'_>, chart: &str, chart_dir: &Path, setting: Option<bool>) {
    if setting.is_some() {
        return;
    }
    let templates = lookup_templates(chart_dir);
    if templates.is_empty() {
        return;
    }
    let shown: Vec<&str> = templates.iter().take(3).map(String::as_str).collect();
    let more = templates.len() - shown.len();
    let mut found = shown.join(", ");
    if more > 0 {
        found = format!("{found} and {more} more");
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
        "{UNDECLARED_CODE}: {} renders chart '{chart}', whose templates appear to call Helm's `lookup` ({found}), but \
         lookup is not enabled, so it finds nothing. {how} (Calls are detected by scanning template sources, so this \
         can miss or over-report.)",
        owner.describe()
    );
}

const MAX_TEMPLATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 8;

/// The templates of the chart at `chart_dir`, including those of its subcharts
/// (directories and `.tgz` under `charts/`, library charts among them), that call
/// `lookup`, as chart-relative paths. Anything that cannot be read is skipped.
pub fn lookup_templates(chart_dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    scan_chart_dir(chart_dir, "", 0, &mut found);
    found.sort();
    found
}

fn is_archive(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tgz"))
}

fn scan_chart_dir(dir: &Path, prefix: &str, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    scan_templates_dir(&dir.join("templates"), &format!("{prefix}templates"), 0, found);
    let Ok(entries) = std::fs::read_dir(dir.join("charts")) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else { continue };
        let label = format!("{prefix}charts/{name}/");
        if kind.is_dir() {
            scan_chart_dir(&entry.path(), &label, depth + 1, found);
        } else if kind.is_file() && is_archive(&name) {
            if let Ok(file) = std::fs::File::open(entry.path()) {
                scan_archive(file, &format!("{prefix}charts/{name}!/"), depth + 1, found);
            }
        }
    }
}

fn scan_templates_dir(dir: &Path, label: &str, depth: usize, found: &mut Vec<String>) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            scan_templates_dir(&entry.path(), &format!("{label}/{name}"), depth + 1, found);
        } else if kind.is_file() {
            let mut source = Vec::new();
            let read = std::fs::File::open(entry.path())
                .and_then(|file| file.take(MAX_TEMPLATE_BYTES).read_to_end(&mut source));
            if read.is_ok() && template_calls_lookup(&String::from_utf8_lossy(&source)) {
                found.push(format!("{label}/{name}"));
            }
        }
    }
}

fn scan_archive(reader: impl Read, label: &str, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader.take(MAX_ARCHIVE_BYTES)));
    let Ok(entries) = archive.entries() else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Ok(path) = entry.path().map(|path| path.to_string_lossy().into_owned()) else {
            continue;
        };
        let in_templates = path.split('/').any(|part| part == "templates");
        let nested = is_archive(&path) && path.split('/').any(|part| part == "charts");
        if !in_templates && !nested {
            continue;
        }
        let mut bytes = Vec::new();
        let limit = if nested { MAX_ARCHIVE_BYTES } else { MAX_TEMPLATE_BYTES };
        if entry.take(limit).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if nested {
            scan_archive(bytes.as_slice(), &format!("{label}{path}!/"), depth + 1, found);
        } else if template_calls_lookup(&String::from_utf8_lossy(&bytes)) {
            found.push(format!("{label}{path}"));
        }
    }
}

/// Whether template source calls Helm's `lookup` function: the identifier
/// `lookup` inside a `{{ ... }}` action, outside string literals and comments,
/// and not a field or variable (`.Values.lookup`, `$lookup`).
pub fn template_calls_lookup(source: &str) -> bool {
    let bytes = source.as_bytes();
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
                None => return false,
            }
        }
        let (called, end) = action_calls_lookup(bytes, cursor);
        if called {
            return true;
        }
        index = end;
    }
    false
}

/// Scans one action body from `from`; returns whether it calls `lookup` and
/// where the action ends.
fn action_calls_lookup(bytes: &[u8], from: usize) -> (bool, usize) {
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut cursor = from;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'}' if bytes.get(cursor + 1) == Some(&b'}') => return (false, cursor + 2),
            quote @ (b'"' | b'\'') => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += if bytes[cursor] == b'\\' { 2 } else { 1 };
                }
                cursor += 1;
            }
            b'`' => {
                cursor = find(bytes, cursor + 1, b"`").map_or(bytes.len(), |end| end + 1);
            }
            byte if is_word(byte) => {
                let begin = cursor;
                while cursor < bytes.len() && is_word(bytes[cursor]) {
                    cursor += 1;
                }
                let access = begin > 0 && matches!(bytes[begin - 1], b'.' | b'$');
                if &bytes[begin..cursor] == b"lookup" && !access {
                    return (true, cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    (false, bytes.len())
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

    #[test]
    fn templates_are_found_in_the_chart_its_subcharts_and_library_charts() {
        let temp = tempfile::TempDir::new().unwrap();
        let chart = temp.path();
        write(&chart.join("templates/plain.yaml"), "kind: ConfigMap\n");
        write(
            &chart.join("templates/_helpers.tpl"),
            "{{ define \"x\" }}{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}{{ end }}",
        );
        write(
            &chart.join("charts/unpacked/templates/deep/secret.yaml"),
            "{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}",
        );
        write(
            &chart.join("charts/clean/templates/cm.yaml"),
            "{{/* lookup */}}kind: ConfigMap",
        );
        write(
            &chart.join("charts/unpacked/charts/lib/templates/_lib.tpl"),
            "{{ define \"l\" }}{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}{{ end }}",
        );
        let inner = archive(&[("inner/templates/a.yaml", b"{{ lookup \"v1\" \"Secret\" \"n\" \"s\" }}")]);
        let packed = archive(&[
            ("packed/Chart.yaml", b"name: packed"),
            (
                "packed/templates/s.yaml",
                b"{{ $x := lookup \"v1\" \"Secret\" \"n\" \"s\" }}",
            ),
            ("packed/templates/clean.yaml", b"kind: ConfigMap"),
            ("packed/charts/inner-1.0.0.tgz", &inner),
        ]);
        std::fs::write(chart.join("charts/packed-1.0.0.tgz"), packed).unwrap();

        assert_eq!(
            lookup_templates(chart),
            [
                "charts/packed-1.0.0.tgz!/packed/charts/inner-1.0.0.tgz!/inner/templates/a.yaml",
                "charts/packed-1.0.0.tgz!/packed/templates/s.yaml",
                "charts/unpacked/charts/lib/templates/_lib.tpl",
                "charts/unpacked/templates/deep/secret.yaml",
                "templates/_helpers.tpl",
            ]
        );
    }

    #[test]
    fn a_chart_without_lookup_calls_reports_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        write(
            &temp.path().join("templates/cm.yaml"),
            "data: {{ .Values.lookup | quote }}\n",
        );
        write(&temp.path().join("values.yaml"), "lookup: true\n");
        assert!(lookup_templates(temp.path()).is_empty());
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
