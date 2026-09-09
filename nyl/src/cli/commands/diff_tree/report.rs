//! Comparison data and destination-specific report formatting.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

use crate::render::cache::CacheStats;
use crate::util::{ansi_style, sanitize_url};
use crate::{NylError, Result};

use super::{is_null_output, ComparisonSummary, DiffSelection, ResolvedBaseline};

#[derive(Clone, Copy, Debug)]
pub(super) enum ReportFormat {
    Text,
    Markdown,
    Json,
}

#[derive(Clone, Debug)]
pub(super) struct ReportOutput {
    pub format: ReportFormat,
    pub path: PathBuf,
}

impl FromStr for ReportOutput {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (format, path) = value.split_once(':').ok_or("expected FORMAT:PATH")?;
        let format = match format {
            "text" => ReportFormat::Text,
            "markdown" => ReportFormat::Markdown,
            "json" => ReportFormat::Json,
            _ => return Err("report format must be text, markdown, or json".into()),
        };
        if path.is_empty() {
            return Err("report output path must not be empty".into());
        }
        Ok(Self {
            format,
            path: path.into(),
        })
    }
}

/// Validate every destination before rendering or opening output files.
pub(super) fn validate_outputs(diff: &Path, reports: &[ReportOutput]) -> Result<()> {
    let mut stdout = false;
    let mut files = BTreeSet::new();
    #[cfg(unix)]
    let mut identities = BTreeSet::new();
    for path in std::iter::once(diff).chain(reports.iter().map(|output| output.path.as_path())) {
        if is_null_output(path) {
            continue;
        }
        if path == Path::new("-") {
            if stdout {
                return Err(NylError::config(
                    "Multiple outputs select stdout; use --output PATH for the diff",
                ));
            }
            stdout = true;
            continue;
        }
        if path.as_os_str().is_empty() || path.is_dir() {
            return Err(NylError::config(format!("Output must name a file: {}", path.display())));
        }
        if !files.insert(resolve_output_path(path)?) {
            return Err(NylError::config(format!(
                "Duplicate output destination: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::metadata(path) {
            use std::os::unix::fs::MetadataExt as _;
            if !identities.insert((metadata.dev(), metadata.ino())) {
                return Err(NylError::config(format!("Duplicate output file: {}", path.display())));
            }
        }
    }
    Ok(())
}

fn resolve_output_path(path: &Path) -> Result<PathBuf> {
    let mut resolved = std::env::current_dir()?;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            _ => {
                resolved.push(component.as_os_str());
                match resolved.canonicalize() {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(resolved)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FileStatus {
    Added,
    Modified,
    Deleted,
}

impl FileStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct FileStats {
    pub path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub lines_added: Option<usize>,
    pub lines_removed: Option<usize>,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct DiffStats {
    pub has_changes: bool,
    pub files_changed: usize,
    pub files_added: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub binary_files: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files: Vec<FileStats>,
}

pub(super) struct TreeDiff {
    pub patch: String,
    pub stats: DiffStats,
}

impl TreeDiff {
    pub fn between(base: &BTreeMap<PathBuf, Vec<u8>>, desired: &BTreeMap<PathBuf, Vec<u8>>) -> Result<Self> {
        let mut patch = String::new();
        let mut stats = DiffStats::default();
        for path in base.keys().chain(desired.keys()).collect::<BTreeSet<_>>() {
            let old = base.get(path);
            let new = desired.get(path);
            if old == new {
                continue;
            }
            let status = match (old, new) {
                (None, _) => {
                    stats.files_added += 1;
                    FileStatus::Added
                }
                (_, None) => {
                    stats.files_deleted += 1;
                    FileStatus::Deleted
                }
                _ => {
                    stats.files_modified += 1;
                    FileStatus::Modified
                }
            };
            let path = path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            let a = patch_path(&format!("a/{path}"));
            let b = patch_path(&format!("b/{path}"));
            writeln!(patch, "diff --git {a} {b}").unwrap();
            match status {
                FileStatus::Added => {
                    patch.push_str("new file mode 100644\n");
                }
                FileStatus::Deleted => {
                    patch.push_str("deleted file mode 100644\n");
                }
                FileStatus::Modified => {}
            }
            writeln!(
                patch,
                "index {}..{}{}",
                blob_id(old)?,
                blob_id(new)?,
                if old.is_some() && new.is_some() { " 100644" } else { "" }
            )
            .unwrap();
            let old_label = if old.is_none() { "/dev/null" } else { &a };
            let new_label = if new.is_none() { "/dev/null" } else { &b };
            let old_bytes = old.map_or(&[][..], Vec::as_slice);
            let new_bytes = new.map_or(&[][..], Vec::as_slice);
            let text = std::str::from_utf8(old_bytes)
                .ok()
                .zip(std::str::from_utf8(new_bytes).ok())
                .filter(|_| !old_bytes.contains(&0) && !new_bytes.contains(&0));
            let (lines_added, lines_removed) = if let Some((old, new)) = text {
                let diff = TextDiff::from_lines(old, new);
                let mut added = 0;
                let mut removed = 0;
                for change in diff.iter_all_changes() {
                    match change.tag() {
                        ChangeTag::Insert => added += 1,
                        ChangeTag::Delete => removed += 1,
                        ChangeTag::Equal => {}
                    }
                }
                write!(
                    patch,
                    "{}",
                    diff.unified_diff().context_radius(3).header(old_label, new_label)
                )
                .unwrap();
                stats.lines_added += added;
                stats.lines_removed += removed;
                (Some(added), Some(removed))
            } else {
                stats.binary_files += 1;
                writeln!(patch, "Binary files {old_label} and {new_label} differ").unwrap();
                (None, None)
            };
            stats.files.push(FileStats {
                path,
                status,
                binary: text.is_none(),
                lines_added,
                lines_removed,
            });
        }
        stats.files_changed = stats.files.len();
        stats.has_changes = stats.files_changed > 0;
        Ok(Self { patch, stats })
    }
}

fn blob_id(bytes: Option<&Vec<u8>>) -> Result<git2::Oid> {
    bytes.map_or(Ok(git2::Oid::ZERO_SHA1), |bytes| {
        git2::Oid::hash_object(git2::ObjectType::Blob, bytes).map_err(|error| NylError::Git(error.into()))
    })
}

fn patch_path(path: &str) -> String {
    if path.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\') {
        return path.to_owned();
    }
    let mut quoted = String::from("\"");
    for byte in path.bytes() {
        match byte {
            b'"' | b'\\' => {
                quoted.push('\\');
                quoted.push(char::from(byte));
            }
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => {
                write!(quoted, "\\{byte:03o}").unwrap();
            }
        }
    }
    quoted.push('"');
    quoted
}

#[derive(Serialize)]
pub(super) struct Report {
    schema_version: u32,
    comparison: Comparison,
    pub diff: DiffStats,
    render: CacheStats,
}

#[derive(Serialize)]
struct Comparison {
    target: String,
    selection: Selection,
    desired_source: Source,
    baseline: Baseline,
    desired_publication: Publication,
    diff_output: String,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Selection {
    Tree,
    Catalog,
    Applications { applications: Vec<String> },
}

impl Selection {
    fn description(&self) -> String {
        match self {
            Self::Tree => "entire rendered tree".into(),
            Self::Catalog => "Argo CD catalog".into(),
            Self::Applications { applications } if applications.is_empty() => "all Applications".into(),
            Self::Applications { applications } => format!("Applications {}", applications.join(", ")),
        }
    }
}

#[derive(Serialize)]
struct Source {
    repository: Option<String>,
    commit: Option<String>,
    dirty: bool,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Baseline {
    Published {
        commit: String,
        publication: Publication,
    },
    Source {
        repository: String,
        revision: String,
        commit: String,
        publication: Publication,
    },
}

#[derive(Serialize)]
struct Publication {
    cluster: String,
    repository: String,
    publish_url: Option<String>,
    revision: String,
    path_prefix: String,
}

impl Publication {
    fn from_tree(tree: &crate::gitops::CompiledTargetTree) -> Self {
        Self {
            cluster: tree.cluster.metadata.name.clone(),
            repository: sanitize_url(&tree.repository.repo_url),
            publish_url: tree.repository.publish_url.as_deref().map(sanitize_url),
            revision: tree.target.spec.publication.revision.clone(),
            path_prefix: tree.target.publication_path_prefix().to_owned(),
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Cluster", self.cluster.clone()),
            ("Repository", self.repository.clone()),
            ("Revision", self.revision.clone()),
            (
                "Path",
                if self.path_prefix.is_empty() {
                    ".".into()
                } else {
                    self.path_prefix.clone()
                },
            ),
        ]
    }
}

impl Report {
    pub fn new(summary: &ComparisonSummary<'_>, diff: DiffStats, render: CacheStats) -> Self {
        let selection = match summary.selection {
            DiffSelection::Tree => Selection::Tree,
            DiffSelection::Catalog => Selection::Catalog,
            DiffSelection::Applications(applications) => Selection::Applications {
                applications: applications.iter().cloned().collect(),
            },
        };
        let baseline = match summary.baseline {
            ResolvedBaseline::Published(baseline) => Baseline::Published {
                commit: baseline.commit.to_string(),
                publication: Publication::from_tree(summary.desired),
            },
            ResolvedBaseline::Source(baseline) => Baseline::Source {
                repository: sanitize_url(&baseline.repository),
                revision: baseline.revision.clone(),
                commit: baseline.commit.to_string(),
                publication: Publication::from_tree(&baseline.compiled),
            },
        };
        Self {
            schema_version: 1,
            comparison: Comparison {
                target: summary.target.to_owned(),
                selection,
                desired_source: Source {
                    repository: summary.desired_source_repository.map(sanitize_url),
                    commit: summary.desired_source_commit.map(str::to_owned),
                    dirty: summary.desired_dirty,
                },
                baseline,
                desired_publication: Publication::from_tree(summary.desired),
                diff_output: summary.output.to_string_lossy().into_owned(),
            },
            diff,
            render,
        }
    }

    fn context_sections(&self) -> Vec<(&'static str, Vec<(&'static str, String)>)> {
        let source = &self.comparison.desired_source;
        let mut sections = vec![(
            "Desired source",
            vec![
                (
                    "Repository",
                    source
                        .repository
                        .clone()
                        .unwrap_or_else(|| "<local Git repository>".into()),
                ),
                (
                    "Commit",
                    source.commit.clone().unwrap_or_else(|| "<uncommitted>".into()),
                ),
                ("Working tree", if source.dirty { "dirty" } else { "clean" }.into()),
            ],
        )];
        match &self.comparison.baseline {
            Baseline::Published { commit, publication } => {
                let mut fields = publication.fields();
                fields.insert(3, ("Commit", commit.clone()));
                sections.push(("Published baseline", fields));
            }
            Baseline::Source {
                repository,
                revision,
                commit,
                publication,
            } => {
                sections.push((
                    "Source baseline",
                    vec![
                        ("Repository", repository.clone()),
                        ("Revision", revision.clone()),
                        ("Commit", commit.clone()),
                    ],
                ));
                sections.push(("Desired publication", self.comparison.desired_publication.fields()));
                sections.push(("Baseline publication", publication.fields()));
            }
        }
        sections
    }

    pub fn format(&self, format: ReportFormat, files: bool, color: bool) -> Result<String> {
        match format {
            ReportFormat::Json => Ok(format!("{}\n", serde_json::to_string_pretty(self)?)),
            ReportFormat::Text => Ok(self.text(files, color)),
            ReportFormat::Markdown => Ok(self.markdown(files)),
        }
    }

    fn file_totals(&self) -> String {
        let d = &self.diff;
        format!(
            "{} changed · {} added · {} modified · {} deleted",
            d.files_changed, d.files_added, d.files_modified, d.files_deleted
        )
    }

    fn text(&self, files: bool, color: bool) -> String {
        let mut output = format!(
            "{}\n  {:<20}{}\n  {:<20}{}\n",
            ansi_style("Rendered tree comparison", "1", color),
            "Deployment target",
            ansi_style(display_value(&self.comparison.target), "1;36", color),
            "View",
            display_value(&self.comparison.selection.description())
        );
        for (heading, fields) in self.context_sections() {
            writeln!(output, "  {}", ansi_style(heading, "1;36", color)).unwrap();
            for (label, value) in fields {
                let code = if label == "Working tree" {
                    if value == "dirty" {
                        "1;33"
                    } else {
                        "32"
                    }
                } else {
                    "36"
                };
                writeln!(
                    output,
                    "    {label:<18}{}",
                    ansi_style(display_value(&value), code, color)
                )
                .unwrap();
            }
        }
        let destination = &self.comparison.diff_output;
        writeln!(
            output,
            "  {:<20}{}\n",
            "Diff output",
            ansi_style(
                if destination == "-" {
                    "stdout".into()
                } else {
                    display_value(destination)
                },
                "36",
                color
            )
        )
        .unwrap();
        if !self.diff.has_changes {
            writeln!(
                output,
                "Deployment target {} has no rendered differences",
                display_value(&self.comparison.target)
            )
            .unwrap();
        }
        writeln!(
            output,
            "{}\n  {:<12}{}\n  {:<12}{} {}",
            ansi_style("Rendered differences", "1", color),
            "Files",
            self.file_totals(),
            "Lines",
            ansi_style(format!("+{}", self.diff.lines_added), "32", color),
            ansi_style(format!("−{}", self.diff.lines_removed), "31", color)
        )
        .unwrap();
        if self.diff.binary_files > 0 {
            writeln!(
                output,
                "  Binary      {} (excluded from line totals)",
                self.diff.binary_files
            )
            .unwrap();
        }
        if files && self.diff.has_changes {
            writeln!(output, "\n  {:<8} {:>8} {:>8}  File", "Status", "Added", "Removed").unwrap();
            for file in &self.diff.files {
                let (added, removed) = line_labels(file);
                writeln!(
                    output,
                    "  {:<8} {:>8} {:>8}  {}",
                    file.status.label(),
                    ansi_style(format!("{added:>8}"), "32", color),
                    ansi_style(format!("{removed:>8}"), "31", color),
                    display_value(&file.path)
                )
                .unwrap();
            }
        }
        if self.render.has_reportable_work() {
            writeln!(output, "\n{}", self.render.format_with_color(color)).unwrap();
        }
        output
    }

    fn markdown(&self, files: bool) -> String {
        let mut output = format!("## Rendered tree comparison\n\n| Field | Value |\n| --- | --- |\n| Deployment target | {} |\n| View | {} |\n| Diff output | {} |\n",
            markdown_value(&self.comparison.target), markdown_value(&self.comparison.selection.description()),
            markdown_value(if self.comparison.diff_output == "-" { "stdout" } else { &self.comparison.diff_output }));
        for (heading, fields) in self.context_sections() {
            writeln!(output, "\n### {heading}\n\n| Field | Value |\n| --- | --- |").unwrap();
            for (label, value) in fields {
                writeln!(output, "| {label} | {} |", markdown_value(&value)).unwrap();
            }
        }
        writeln!(
            output,
            "\n### Rendered differences\n\n{}; **+{} −{} lines**.\n",
            self.file_totals(),
            self.diff.lines_added,
            self.diff.lines_removed
        )
        .unwrap();
        if !self.diff.has_changes {
            output.push_str("No rendered differences.\n");
        }
        if self.diff.binary_files > 0 {
            writeln!(
                output,
                "\n{} binary files excluded from line totals.",
                self.diff.binary_files
            )
            .unwrap();
        }
        if files && self.diff.has_changes {
            output.push_str("\n| File | Status | Added | Removed |\n| --- | --- | ---: | ---: |\n");
            for file in &self.diff.files {
                let (added, removed) = line_labels(file);
                writeln!(
                    output,
                    "| {} | {} | {added} | {removed} |",
                    markdown_value(&file.path),
                    file.status.label()
                )
                .unwrap();
            }
        }
        if self.render.has_reportable_work() {
            // Indented code keeps arbitrary bypass reasons literal in Markdown.
            output.push_str("\n### Render statistics\n\n");
            for line in self.render.format_with_color(false).lines().skip(1) {
                writeln!(output, "    {}", display_value(line)).unwrap();
            }
        }
        output
    }
}

fn line_labels(file: &FileStats) -> (String, String) {
    (
        file.lines_added.map_or_else(|| "binary".into(), |n| format!("+{n}")),
        file.lines_removed.map_or_else(|| "binary".into(), |n| format!("−{n}")),
    )
}

fn display_value(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn markdown_value(value: &str) -> String {
    let mut output = String::new();
    for c in display_value(value).chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.' | '!' | '|' => {
                output.push('\\');
                output.push(c);
            }
            _ => output.push(c),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &[u8])]) -> BTreeMap<PathBuf, Vec<u8>> {
        files
            .iter()
            .map(|(path, bytes)| (PathBuf::from(path), bytes.to_vec()))
            .collect()
    }

    fn report(diff: DiffStats) -> Report {
        Report {
            schema_version: 1,
            comparison: Comparison {
                target: "production".into(),
                selection: Selection::Tree,
                desired_source: Source {
                    repository: None,
                    commit: None,
                    dirty: true,
                },
                baseline: Baseline::Published {
                    commit: "0123456".into(),
                    publication: publication(),
                },
                desired_publication: publication(),
                diff_output: "-".into(),
            },
            diff,
            render: CacheStats::default(),
        }
    }

    fn publication() -> Publication {
        Publication {
            cluster: "cluster".into(),
            repository: "https://example.invalid/repo".into(),
            publish_url: None,
            revision: "deploy".into(),
            path_prefix: "production".into(),
        }
    }

    #[test]
    fn test_diff_counts_share_patch_line_operations() {
        let base = tree(&[
            ("deleted", b"one\ntwo\n"),
            ("modified", b"context\nold\n"),
            ("same", b"same\n"),
        ]);
        let desired = tree(&[
            ("added", b"one\ntwo\nthree\n"),
            ("modified", b"context\nnew\n"),
            ("same", b"same\n"),
        ]);
        let diff = TreeDiff::between(&base, &desired).unwrap();
        assert_eq!(
            (
                diff.stats.files_added,
                diff.stats.files_modified,
                diff.stats.files_deleted
            ),
            (1, 1, 1)
        );
        assert_eq!((diff.stats.lines_added, diff.stats.lines_removed), (4, 3));
        assert_eq!(
            diff.stats.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["added", "deleted", "modified"]
        );
        assert!(diff.patch.contains("--- /dev/null\n+++ b/added"));
        assert!(diff.patch.contains("--- a/deleted\n+++ /dev/null"));
        let patch_added = diff
            .patch
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count();
        let patch_removed = diff
            .patch
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count();
        assert_eq!(
            (diff.stats.lines_added, diff.stats.lines_removed),
            (patch_added, patch_removed)
        );
    }

    #[test]
    fn test_diff_counts_provenance_and_unterminated_lines() {
        for (old, new) in [
            (
                "# Nyl-Provenance: Resource: old\nkind: Pod\n",
                "# Nyl-Provenance: Resource: new\nkind: Pod\n",
            ),
            ("line", "line\n"),
            ("line\r\n", "line\n"),
        ] {
            let diff =
                TreeDiff::between(&tree(&[("file", old.as_bytes())]), &tree(&[("file", new.as_bytes())])).unwrap();
            assert_eq!((diff.stats.lines_added, diff.stats.lines_removed), (1, 1));
            assert!(diff.stats.has_changes);
        }
        let diff = TreeDiff::between(&tree(&[("file", b"line")]), &tree(&[("file", b"line\n")])).unwrap();
        assert!(diff.patch.contains("\\ No newline at end of file"));
    }

    #[test]
    fn test_empty_and_binary_changes_have_explicit_presence_and_counts() {
        let base = tree(&[("deleted-empty", b""), ("binary", b"\xff"), ("same-binary", b"\xff")]);
        let desired = tree(&[
            ("added-empty", b""),
            ("binary", b"\xfe"),
            ("same-binary", b"\xff"),
            ("nul", b"x\0y"),
        ]);
        let diff = TreeDiff::between(&base, &desired).unwrap();
        assert!(diff.stats.has_changes);
        assert_eq!(diff.stats.files_changed, 4);
        assert_eq!(diff.stats.binary_files, 2);
        assert_eq!((diff.stats.lines_added, diff.stats.lines_removed), (0, 0));
        assert!(diff
            .patch
            .contains("diff --git a/added-empty b/added-empty\nnew file mode 100644\n"));
        assert!(diff
            .patch
            .contains("diff --git a/deleted-empty b/deleted-empty\ndeleted file mode 100644\n"));
        assert!(diff.patch.contains("Binary files a/binary and b/binary differ"));
        for file in diff.stats.files.iter().filter(|f| f.binary) {
            assert_eq!((file.lines_added, file.lines_removed), (None, None));
        }
    }

    #[test]
    fn test_patch_applies_text_and_empty_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        let base = tree(&[("deleted-empty", b""), ("modified", b"old"), ("deleted", b"deleted\n")]);
        let desired = tree(&[
            ("added-empty", b""),
            ("modified", b"new\n"),
            ("added", b"added\n"),
            ("space name", b"space\n"),
            ("unicode-Ä.yaml", b"unicode\n"),
        ]);
        for (path, bytes) in &base {
            std::fs::write(temp.path().join(path), bytes).unwrap();
        }
        let diff = TreeDiff::between(&base, &desired).unwrap();
        assert_cmd::Command::new("git")
            .current_dir(temp.path())
            .timeout(std::time::Duration::from_secs(10))
            // Preserve rendered bytes regardless of the user's Git line-ending settings.
            .args(["-c", "core.autocrlf=false", "-c", "core.eol=lf", "apply"])
            .write_stdin(diff.patch)
            .assert()
            .success();
        for (path, bytes) in &desired {
            assert_eq!(&std::fs::read(temp.path().join(path)).unwrap(), bytes);
        }
        for path in base.keys().filter(|path| !desired.contains_key(*path)) {
            assert!(!temp.path().join(path).exists());
        }
    }

    #[test]
    fn test_zero_diff_is_a_complete_report() {
        let base = tree(&[("empty", b""), ("text", b"line\n"), ("binary", b"\xff")]);
        let diff = TreeDiff::between(&base, &base).unwrap();
        assert!(diff.patch.is_empty());
        assert!(!diff.stats.has_changes);
        let report = report(diff.stats);
        let json: serde_json::Value =
            serde_json::from_str(&report.format(ReportFormat::Json, false, true).unwrap()).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["diff"]["files"], serde_json::json!([]));
        assert_eq!(json["diff"]["lines_added"], 0);
        assert!(report.text(true, false).contains("has no rendered differences"));
        assert!(report.markdown(true).contains("No rendered differences."));
    }

    #[test]
    fn test_formats_preserve_counts_and_escape_paths() {
        let path = "a|b`<tag>&[link](url)\n.yaml";
        let report = report(
            TreeDiff::between(&BTreeMap::new(), &tree(&[(path, b"new\n")]))
                .unwrap()
                .stats,
        );
        let text = report.format(ReportFormat::Text, true, false).unwrap();
        let colored = report.format(ReportFormat::Text, true, true).unwrap();
        let markdown = report.format(ReportFormat::Markdown, true, true).unwrap();
        let json = report.format(ReportFormat::Json, false, true).unwrap();
        assert!(text.contains("1 changed · 1 added · 0 modified · 0 deleted"));
        assert!(text.contains("+1 −0"));
        assert!(text.contains("a|b`<tag>&[link](url)\\n.yaml"));
        assert!(colored.contains("\x1b[32m+1\x1b[0m"));
        assert!(colored.contains("\x1b[31m−0\x1b[0m"));
        assert!(markdown.contains("| a\\|b\\`&lt;tag&gt;&amp;\\[link\\]\\(url\\)\\\\n\\.yaml | added | +1 | −0 |"));
        for artifact in [&text, &markdown, &json] {
            assert!(!artifact.contains('\x1b'));
        }
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["diff"]["files"][0]["path"], path);
        assert_eq!(value["diff"]["lines_added"], 1);
        assert!(!report.text(false, false).contains("a|b"));
        assert!(!report.markdown(false).contains("| File |"));
    }

    #[test]
    fn test_outputs_reject_collisions_before_creating_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("new/report.json");
        let output = |path: PathBuf| ReportOutput {
            format: ReportFormat::Json,
            path,
        };
        assert!(validate_outputs(Path::new("-"), &[output("-".into())]).is_err());
        assert!(validate_outputs(&path, &[output(temp.path().join("new/../new/report.json"))]).is_err());
        assert!(validate_outputs(&path, &[output("-".into()), output("-".into())]).is_err());
        assert!(validate_outputs(&path, &[output("-".into())]).is_ok());
        assert!(!temp.path().join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_outputs_reject_symlink_and_hardlink_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("report");
        std::fs::write(&file, "report").unwrap();
        let symlink = temp.path().join("symlink");
        std::os::unix::fs::symlink(temp.path(), &symlink).unwrap();
        let hardlink = temp.path().join("hardlink");
        std::fs::hard_link(&file, &hardlink).unwrap();
        for alias in [symlink.join("report"), hardlink] {
            assert!(validate_outputs(
                &file,
                &[ReportOutput {
                    format: ReportFormat::Text,
                    path: alias
                }]
            )
            .is_err());
        }
    }

    #[test]
    fn test_report_destination_parser_keeps_colons_in_paths() {
        let destination: ReportOutput = "json:artifacts/name:revision.json".parse().unwrap();
        assert_eq!(destination.path, Path::new("artifacts/name:revision.json"));
        for invalid in ["yaml:file", "json", "json:"] {
            assert!(invalid.parse::<ReportOutput>().is_err());
        }
    }
}
