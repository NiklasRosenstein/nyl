//! Bounded review presentation; complete evidence belongs in JSON and the patch artifact.

use super::*;
use crate::render::ProvenanceFrame;
use crate::validation::{ResourceResult, ResourceStatus};

pub(super) const MARKDOWN_LIMIT: usize = 60_000;
const FILES_LIMIT: usize = 10_000;
const PATCH_LIMIT: usize = 20_000;
const NOTICE_RESERVE: usize = 1_024;

/// Bound complete escaped characters, so clipping cannot expose Markdown syntax.
fn value(input: &str) -> String {
    let mut output = String::new();
    for c in input.chars() {
        let escaped = markdown_value(&c.to_string());
        if output.len() + escaped.len() > 2_048 - "… [truncated]".len() {
            output.push_str("… [truncated]");
            break;
        }
        output.push_str(&escaped);
    }
    output
}

fn details(summary: &str, contents: &str) -> String {
    format!("\n<details>\n<summary>{summary}</summary>\n\n{contents}\n</details>\n")
}

fn push_if_fits(output: &mut String, block: &str, limit: usize) -> bool {
    if output.len() + block.len() <= limit {
        output.push_str(block);
        true
    } else {
        false
    }
}

impl Report {
    pub(super) fn markdown(&self, files: bool) -> String {
        let mut output = self.markdown_header();
        let footer = self.artifact_footer();
        let limit = MARKDOWN_LIMIT - footer.len() - NOTICE_RESERVE;
        let mut omitted_errors = 0;
        for error in &self.errors {
            let block = format!("**Error ({}):** {}\n\n", error.stage, value(&error.message));
            if !push_if_fits(&mut output, &block, limit) {
                omitted_errors += 1;
            }
        }
        if omitted_errors > 0 {
            writeln!(output, "{omitted_errors} additional operation errors omitted.\n").unwrap();
        }

        let traces = self.markdown_findings(&mut output, limit);

        if let Some(diff) = &self.diff {
            if files && diff.has_changes {
                let allowance = FILES_LIMIT.min(limit.saturating_sub(output.len()));
                let section = self.file_table(diff, allowance);
                if !push_if_fits(&mut output, &section, limit) {
                    writeln!(
                        output,
                        "Changed-file table omitted: {} changed files. Full list: {}.\n",
                        diff.files.len(),
                        self.evidence_reference()
                    )
                    .unwrap();
                }
            }
            if self.stats_patch && diff.has_changes {
                let allowance = PATCH_LIMIT.min(limit.saturating_sub(output.len()));
                let section = self.patch_preview(allowance);
                if !push_if_fits(&mut output, &section, limit) || section.is_empty() {
                    writeln!(
                        output,
                        "Patch preview omitted to preserve validation findings. {}.\n",
                        self.patch_reference()
                    )
                    .unwrap();
                }
            }
        }
        self.markdown_context(&mut output, traces, limit);
        output.push_str(&footer);
        debug_assert!(output.len() <= MARKDOWN_LIMIT);
        output
    }

    fn markdown_header(&self) -> String {
        let target = self
            .comparison
            .target
            .as_deref()
            .or(self.request.target.as_deref())
            .unwrap_or("target unresolved");
        let mut output = format!("## Nyl · {}\n\n", value(target));
        let source = self.comparison.desired_source.as_ref();
        let commit = source.and_then(|s| s.commit.as_deref()).unwrap_or(if source.is_some() {
            "uncommitted"
        } else {
            "unavailable"
        });
        let baseline = match &self.comparison.baseline {
            Some(Baseline::Published { commit, .. } | Baseline::Source { commit, .. }) => commit.as_str(),
            None => self.request.source_ref.as_deref().unwrap_or("unavailable"),
        };
        writeln!(
            output,
            "Source {} · {} baseline {}\n",
            value(commit),
            self.request.against,
            value(baseline)
        )
        .unwrap();
        if source.is_some_and(|s| s.dirty) {
            output.push_str("**Working tree: dirty.**\n\n");
        }
        writeln!(output, "**{}**\n", value(&self.validation_summary())).unwrap();
        writeln!(
            output,
            "Validation scope: complete desired target. Diff scope: {}.\n",
            value(&self.comparison.selection.description())
        )
        .unwrap();
        if let Some(diff) = &self.diff {
            writeln!(
                output,
                "**Rendered differences:** {}; **+{} −{} lines**.\n",
                self.file_totals(),
                diff.lines_added,
                diff.lines_removed
            )
            .unwrap();
            if !diff.has_changes {
                output.push_str("No rendered differences.\n\n");
            }
            if diff.binary_files > 0 {
                writeln!(
                    output,
                    "{} binary files excluded from line totals.\n",
                    diff.binary_files
                )
                .unwrap();
            }
        } else {
            output.push_str("**Diff unavailable.**\n\n");
        }
        if self.diff_policy_failed {
            output.push_str("**Difference policy failed** (`--fail-on-diff`).\n\n");
        }
        for (name, state) in [
            ("Discovery", &self.stages.discovery),
            ("Rendering", &self.stages.render),
            ("Comparison", &self.stages.comparison),
        ] {
            if matches!(state, StageState::Failed) {
                writeln!(output, "**{name} failed.**\n").unwrap();
            }
        }

        output
    }

    fn markdown_findings(&self, output: &mut String, limit: usize) -> Vec<(&ResourceResult, usize)> {
        let mut traces = Vec::new();
        if let Some(validation) = &self.validation.report {
            let failures = validation
                .resources
                .iter()
                .filter(|r| matches!(r.status, ResourceStatus::Invalid | ResourceStatus::Error))
                .collect::<Vec<_>>();
            if !failures.is_empty() {
                output.push_str("### Validation findings\n\n");
                let mut shown_resources = 0;
                let mut shown_findings = 0;
                let total_findings: usize = failures.iter().map(|r| r.findings.len()).sum();
                let mut additional = String::new();
                for (index, resource) in failures.iter().copied().enumerate() {
                    let prefix = resource_prefix(resource);
                    let mut block = prefix;
                    let mut findings = 0;
                    for finding in &resource.findings {
                        let entry = format!(
                            "- **{}:** {}\n",
                            value(match finding.path.as_deref() {
                                Some("") => "Resource root",
                                Some(path) => path,
                                None => "Field unavailable",
                            }),
                            value(&finding.message)
                        );
                        if output.len() + additional.len() + block.len() + entry.len() + 512 > limit {
                            break;
                        }
                        block.push_str(&entry);
                        findings += 1;
                    }
                    if findings == 0 && !resource.findings.is_empty() {
                        continue;
                    }
                    if resource.findings.is_empty() {
                        block.push_str("Validator returned no finding message.\n");
                    }
                    if findings < resource.findings.len() {
                        writeln!(
                            block,
                            "\n{} further findings omitted for this resource.",
                            resource.findings.len() - findings
                        )
                        .unwrap();
                    }
                    writeln!(block, "\n<nyl-provenance-{index}>").unwrap();
                    if output.len() + additional.len() + block.len() + 512 > limit {
                        continue;
                    }
                    shown_resources += 1;
                    shown_findings += findings;
                    // Expansion traces have lower priority than findings and diff previews.
                    traces.push((resource, index));
                    if index < 2 {
                        output.push_str(&block);
                    } else {
                        additional.push_str(&block);
                    }
                }
                if !additional.is_empty() {
                    output.push_str(&details(
                        &format!(
                            "More validation failures ({} resource{})",
                            failures.len().saturating_sub(2),
                            if failures.len() == 3 { "" } else { "s" }
                        ),
                        &additional,
                    ));
                }
                if shown_resources < failures.len() || shown_findings < total_findings {
                    writeln!(output, "Showing {shown_resources} of {} failing resources and {shown_findings} of {total_findings} findings. Full evidence: {}.\n", failures.len(), self.evidence_reference()).unwrap();
                }
            }
        }
        traces
    }

    fn markdown_context(&self, output: &mut String, traces: Vec<(&ResourceResult, usize)>, limit: usize) {
        let mut omitted_context = false;
        for (resource, index) in traces {
            let trace = resource_trace(resource);
            if trace.is_empty() {
                *output = output.replacen(&format!("<nyl-provenance-{index}>"), "", 1);
            } else {
                let block = details("Expansion trace and schema", &trace);
                let marker = format!("<nyl-provenance-{index}>");
                if output.len() - marker.len() + block.len() <= limit {
                    *output = output.replacen(&marker, &block, 1);
                } else {
                    *output = output.replacen(&marker, "", 1);
                    omitted_context = true;
                }
            }
        }
        let mut context = format!(
            "Comparison: {}. Diff output: {}.\n",
            self.comparison.mode.label(),
            value(&self.comparison.diff_output)
        );
        for (heading, fields) in self.context_sections() {
            writeln!(context, "\n### {heading}\n\n| Field | Value |\n| --- | --- |").unwrap();
            for (label, content) in fields {
                writeln!(context, "| {label} | {} |", value(&content)).unwrap();
            }
        }
        if !push_if_fits(output, &details("Comparison context", &context), limit) {
            omitted_context = true;
        }
        if let Some(render) = &self.render {
            if render.has_reportable_work() {
                let mut statistics = String::from("### Render statistics\n\n");
                for line in render.format_with_color(false).lines().skip(1) {
                    writeln!(statistics, "{}  ", value(line)).unwrap();
                }
                if !push_if_fits(output, &details("Render statistics", &statistics), limit) {
                    omitted_context = true;
                }
            }
        }
        if omitted_context {
            output.push_str("Additional provenance, context, or render statistics omitted for size.\n\n");
        }
    }

    fn artifact_footer(&self) -> String {
        let mut footer = String::new();
        if let Some(url) = &self.artifacts_url {
            writeln!(
                footer,
                "\n[Full reports and patch artifacts][nyl-artifacts]\n\n[nyl-artifacts]: {}\n",
                link_url(url)
            )
            .unwrap();
        }
        if self.patch.is_some()
            && self.comparison.diff_output != "-"
            && !is_null_output(Path::new(&self.comparison.diff_output))
        {
            writeln!(footer, "Full patch: {}.\n", value(&self.comparison.diff_output)).unwrap();
        }
        for label in ["JSON report", "Text report"] {
            if let Some((_, path)) = self.outputs.iter().find(|(kind, _)| kind == label) {
                writeln!(footer, "{label}: {}.\n", value(path)).unwrap();
            }
        }
        footer
    }

    fn evidence_reference(&self) -> &'static str {
        if self.artifacts_url.is_some() {
            "[CI artifacts][nyl-artifacts]"
        } else if self.outputs.iter().any(|(kind, _)| kind == "JSON report") {
            "JSON report listed below"
        } else {
            "export JSON with --stats-output json:PATH"
        }
    }

    fn patch_reference(&self) -> &'static str {
        if self.artifacts_url.is_some() {
            "Full patch: [CI artifacts][nyl-artifacts]"
        } else if self.comparison.diff_output == "-" {
            "Full patch is written to stdout"
        } else if is_null_output(Path::new(&self.comparison.diff_output)) {
            "Save the full patch with --output PATH"
        } else {
            "Full patch is listed below"
        }
    }

    fn file_table(&self, diff: &DiffStats, allowance: usize) -> String {
        let mut table = String::from("| File | Status | Added | Removed |\n| --- | --- | ---: | ---: |\n");
        let mut shown = 0;
        for file in &diff.files {
            let (added, removed) = line_labels(file);
            let row = format!(
                "| {} | {} | {added} | {removed} |\n",
                value(&file.path),
                file.status.label()
            );
            if table.len() + row.len() + 300 > allowance {
                break;
            }
            table.push_str(&row);
            shown += 1;
        }
        if shown < diff.files.len() {
            writeln!(
                table,
                "\nShowing {shown} of {} changed files. Full file list: {}.",
                diff.files.len(),
                self.evidence_reference()
            )
            .unwrap();
        }
        if diff.files.len() > 2 {
            details(&format!("Changed files ({})", diff.files.len()), &table)
        } else {
            format!("\n{table}\n")
        }
    }

    fn patch_preview(&self, allowance: usize) -> String {
        let Some(patch) = &self.patch else {
            return String::new();
        };
        let mut preview = String::new();
        let mut shown = 0;
        let mut total = 0;
        let mut last_file = None;
        for (index, file) in patch_files(patch).enumerate() {
            let hunks: Vec<_> = file.match_indices("\n@@ ").map(|(offset, _)| offset + 1).collect();
            let header_end = hunks.first().copied().unwrap_or(file.len());
            let header = &file[..header_end];
            let entries: Vec<_> = if hunks.is_empty() {
                vec![file]
            } else {
                hunks
                    .iter()
                    .enumerate()
                    .map(|(i, start)| &file[*start..hunks.get(i + 1).copied().unwrap_or(file.len())])
                    .collect()
            };
            for entry in entries {
                total += 1;
                let header = if hunks.is_empty() || last_file == Some(index) {
                    ""
                } else {
                    header
                };
                // Literal formatting only expands bytes; oversized units cannot fit.
                if header.len() + entry.len() + 400 > allowance {
                    continue;
                }
                let block = literal_patch(&format!("{header}{entry}"));
                let fence_length = fence_size(&preview).max(fence_size(&block));
                if preview.len() + block.len() + 2 * fence_length + 400 <= allowance {
                    preview.push_str(&block);
                    shown += 1;
                    last_file = Some(index);
                }
            }
        }
        let notice = if shown < total {
            format!(
                "\nPatch preview truncated: showing {shown} of {total} hunks or metadata-only file entries. {}.\n",
                self.patch_reference()
            )
        } else {
            String::new()
        };
        let fence = "`".repeat(fence_size(&preview));
        details("Patch preview", &format!("{fence}diff\n{preview}\n{fence}\n{notice}"))
    }
}

fn resource_prefix(result: &ResourceResult) -> String {
    let r = &result.resource;
    let name = r.namespace.as_deref().map_or_else(
        || r.name.as_deref().unwrap_or("unnamed").to_owned(),
        |ns| format!("{ns}/{}", r.name.as_deref().unwrap_or("unnamed")),
    );
    let mut output = format!(
        "**{} {}** · {} · destination {} · {}\n\n",
        value(&r.kind),
        value(&name),
        value(&r.api_version),
        value(&result.destination),
        value(&result.validator)
    );
    if let Some(ProvenanceFrame::Source { path, document }) = result
        .provenance
        .0
        .iter()
        .find(|f| matches!(f, ProvenanceFrame::Source { .. }))
    {
        writeln!(
            output,
            "Source: {} · document {document}  ",
            value(&path.to_string_lossy())
        )
        .unwrap();
    } else {
        output.push_str("Source provenance unavailable.  \n");
    }
    writeln!(output, "Rendered: {}\n", value(&result.rendered_location.to_string())).unwrap();
    output
}

fn resource_trace(result: &ResourceResult) -> String {
    let mut output = String::new();
    for frame in &result.provenance.0 {
        let line = match frame {
            ProvenanceFrame::Source { path, document } => format!("Source: {} · document {document}", path.display()),
            ProvenanceFrame::Resource { identity } => format!("Expanded from: {identity}"),
            ProvenanceFrame::Generated { operation } => format!("Generated: {operation}"),
            ProvenanceFrame::Remote { repository, revision } => format!("Repository: {repository} @ {revision}"),
        };
        writeln!(output, "- {}", value(&line)).unwrap();
    }
    if let Some(origin) = &result.schema_origin {
        writeln!(
            output,
            "- Schema: {}",
            value(&serde_json::to_string(origin).expect("schema origin serializes"))
        )
        .unwrap();
    }
    output
}

fn link_url(url: &str) -> String {
    url.replace('(', "%28")
        .replace(')', "%29")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

fn fence_size(text: &str) -> usize {
    let mut run = 0;
    let mut longest = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    3.max(longest + 1)
}

fn literal_patch(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn patch_files(patch: &str) -> impl Iterator<Item = &str> {
    let mut starts = vec![0];
    starts.extend(patch.match_indices("\ndiff --git ").map(|(offset, _)| offset + 1));
    starts.push(patch.len());
    starts
        .into_iter()
        .scan(None, move |previous, current| {
            let start = previous.replace(current);
            Some(start.map(|start| &patch[start..current]))
        })
        .flatten()
        .filter(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Provenance;
    use crate::validation::{Finding, ResourceIdentity, ResourceLocation, ValidationReport};

    fn sample_report(count: usize) -> Report {
        use clap::Parser as _;
        let cli = crate::cli::Cli::try_parse_from([
            "nyl",
            "diff-tree",
            "--target",
            "production",
            "--stats-patch",
            "--output",
            "rendered.diff",
            "--stats-output",
            "json:report.json",
            "--stats-artifacts-url",
            "https://example.invalid/artifacts",
        ])
        .unwrap();
        let crate::cli::Commands::DiffTree(args) = cli.command else {
            panic!("diff-tree command")
        };
        let mut report = Report::new(&args);
        let desired = (0..count)
            .map(|i| {
                (
                    PathBuf::from(format!("file-{i:05}.yaml")),
                    format!("name: resource-{i}\n").into_bytes(),
                )
            })
            .collect();
        report.compared(TreeDiff::between(&BTreeMap::new(), &desired, DiffMode::Raw).unwrap());
        report
    }

    fn invalid_resource(index: usize, findings: usize) -> ResourceResult {
        ResourceResult {
            validator: "kubeconform".into(),
            destination: "kasoku".into(),
            resource: ResourceIdentity {
                api_version: "postgresql.cnpg.io/v1".into(),
                kind: "Cluster".into(),
                namespace: Some("rise".into()),
                name: Some(format!("rise-db-{index}")),
            },
            rendered_location: ResourceLocation {
                path: "workloads/rise/resources.yaml".into(),
                document: index + 1,
                items: vec![0],
            },
            status: ResourceStatus::Invalid,
            findings: (0..findings)
                .map(|_| Finding {
                    path: Some("/spec/affinity".into()),
                    message: "got null, want object".into(),
                })
                .collect(),
            provenance: Provenance(vec![ProvenanceFrame::Source {
                path: "applications/rise.yaml".into(),
                document: 4,
            }]),
            schema_origin: None,
        }
    }

    #[test]
    fn test_file_table_collapses_above_two_and_reserves_patch_space() {
        for count in [0, 1, 2, 3, 5_000] {
            let report = sample_report(count);
            let markdown = report.markdown(true);
            assert_eq!(markdown.contains("<summary>Changed files"), count > 2);
            assert!(markdown.len() <= MARKDOWN_LIMIT);
            if count == 5_000 {
                assert!(markdown.contains("of 5000 changed files"));
                assert!(markdown.contains("Patch preview truncated"));
                assert!(markdown.contains("+name: resource-0\n"));
                let json: serde_json::Value =
                    serde_json::from_str(&report.format(ReportFormat::Json, false, false).unwrap()).unwrap();
                assert_eq!(json["diff"]["files"].as_array().unwrap().len(), count);
                assert!(report.patch.as_ref().unwrap().contains("+name: resource-4999\n"));
            }
        }
    }

    #[test]
    fn test_findings_have_identity_locations_and_nested_provenance() {
        let mut report = sample_report(3);
        let mut validation = ValidationReport {
            resources: (0..3).map(|i| invalid_resource(i, 2)).collect(),
            ..Default::default()
        };
        validation.resources[1].findings[0].path = Some(String::new());
        validation.resources[2].findings[0].path = None;
        validation.finish();
        report.validation(
            crate::validation::TreeValidationOutcome {
                report: Some(validation),
                result: Err(NylError::ValidationReported("invalid".into())),
            },
            "",
        );
        let markdown = report.markdown(true);
        assert!(markdown.contains("Validation failed: 0 valid · 3 invalid"));
        assert!(markdown.contains("Cluster rise/rise\\-db\\-0"));
        assert!(markdown.contains("/spec/affinity"));
        assert!(markdown.contains("got null, want object"));
        assert!(markdown.contains("Source: applications/rise\\.yaml · document 4"));
        assert!(markdown.contains("document 1\\) · items\\[0\\]"));
        assert!(markdown.contains("More validation failures (1 resource)"));
        assert!(markdown.find("Expansion trace and schema").unwrap() < markdown.find("Changed files").unwrap());
        assert!(markdown.contains("Resource root"));
        assert!(markdown.contains("Field unavailable"));
        assert!(report.result().is_err());
    }

    #[test]
    fn test_large_and_hostile_findings_stay_bounded_and_complete_in_json() {
        for count in [1, 2, 3, 300] {
            let mut report = sample_report(500);
            let mut validation = ValidationReport {
                resources: (0..count).map(|i| invalid_resource(i, 30)).collect(),
                ..Default::default()
            };
            for resource in &mut validation.resources {
                resource.resource.name = Some("界<&`".repeat(2_000));
                for finding in &mut resource.findings {
                    finding.message = "</details><script>界&\u{1b}\n".repeat(500);
                }
            }
            validation.finish();
            report.validation(
                crate::validation::TreeValidationOutcome {
                    report: Some(validation),
                    result: Err(NylError::ValidationReported("invalid".into())),
                },
                "",
            );
            let markdown = report.markdown(true);
            assert!(markdown.len() <= MARKDOWN_LIMIT, "{} bytes", markdown.len());
            assert!(!markdown.contains("<script>"));
            assert!(!markdown.contains("<nyl-provenance"));
            assert!(!markdown.contains('\u{1b}'));
            assert_eq!(
                markdown.matches("<details>").count(),
                markdown.matches("</details>").count()
            );
            assert!(markdown.contains("findings omitted") || markdown.contains("of 9000 findings"));
            let json: serde_json::Value =
                serde_json::from_str(&report.format(ReportFormat::Json, false, false).unwrap()).unwrap();
            assert_eq!(
                json["validation"]["report"]["resources"].as_array().unwrap().len(),
                count
            );
            assert_eq!(
                json["validation"]["report"]["resources"][0]["findings"][0]["message"],
                "</details><script>界&\u{1b}\n".repeat(500)
            );
        }
    }

    #[test]
    fn test_patch_skips_oversized_hunks_and_keeps_safe_complete_units() {
        let mut report = sample_report(0);
        let desired = BTreeMap::from([
            (PathBuf::from("a-large"), "x\n".repeat(30_000).into_bytes()),
            (PathBuf::from("b-fences"), b"```\n</details>\n".to_vec()),
            (PathBuf::from("c-binary"), vec![0, 255]),
            (PathBuf::from("d-empty"), Vec::new()),
        ]);
        report.compared(TreeDiff::between(&BTreeMap::new(), &desired, DiffMode::Raw).unwrap());
        let markdown = report.markdown(false);
        assert!(markdown.contains("````diff\n"));
        assert!(markdown.contains("+```\n+</details>\n"));
        assert!(markdown.contains("Binary files /dev/null and b/c-binary differ"));
        assert!(markdown.contains("diff --git a/d-empty b/d-empty"));
        assert!(markdown.contains("showing 3 of 4 hunks"));
        assert!(!markdown.contains("diff --git a/a-large"));
        assert!(report.patch.as_ref().unwrap().contains("diff --git a/a-large"));
    }

    #[test]
    fn test_validation_statuses_do_not_claim_unchecked_resources_passed() {
        for (status, expected) in [
            (ResourceStatus::Valid, "Validation passed"),
            (ResourceStatus::Skipped, "No resources validated"),
            (ResourceStatus::NotChecked, "Validation could not complete"),
            (ResourceStatus::Error, "Validation could not complete"),
        ] {
            let mut report = sample_report(0);
            let mut resource = invalid_resource(0, 0);
            resource.status = status;
            let mut validation = ValidationReport {
                resources: vec![resource],
                ..Default::default()
            };
            validation.finish();
            report.validation(
                crate::validation::TreeValidationOutcome {
                    report: Some(validation),
                    result: Ok(()),
                },
                "",
            );
            assert!(report.markdown(false).contains(expected));
        }
        let report = sample_report(0);
        assert!(report.markdown(false).contains("Validation not run"));
    }
}
