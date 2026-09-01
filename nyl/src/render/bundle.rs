//! Release bundle and include loading.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use glob::Pattern;

use super::{best_effort_parse_yaml_documents, RenderProvenance, RenderResource};
use crate::resources::{extract_release, Release};
use crate::template::{TemplateContext, TemplateEngine};
use crate::{NylError, Result};

#[derive(Debug)]
pub(crate) struct LoadedReleaseBundle {
    pub resources: Vec<RenderResource>,
    pub inputs: Vec<PathBuf>,
}

/// Load one manifest entrypoint and any files selected by its Release.
#[cfg(test)]
pub(crate) fn load_release_bundle(path: &Path, context: &TemplateContext) -> Result<LoadedReleaseBundle> {
    load_release_bundle_with_root(path, context, None)
}

pub(crate) fn load_release_bundle_with_root(
    path: &Path,
    context: &TemplateContext,
    provenance_root: Option<&Path>,
) -> Result<LoadedReleaseBundle> {
    let mut resources = load_resource_file(path, context, provenance_root)?;
    let values = resources
        .iter()
        .map(|resource| resource.value.clone())
        .collect::<Vec<_>>();
    let (release, _) = extract_release(&values)?;
    let mut inputs = vec![path.to_path_buf()];
    let Some(release) = release else {
        return Ok(LoadedReleaseBundle { resources, inputs });
    };

    let include_paths = resolve_release_includes(path, &release)?;
    for include_path in &include_paths {
        let included = load_resource_file(include_path, context, provenance_root)?;
        let included_values = included
            .iter()
            .map(|resource| resource.value.clone())
            .collect::<Vec<_>>();
        let (nested_release, _) = extract_release(&included_values)?;
        if nested_release.is_some() {
            return Err(NylError::config(format!(
                "Release {:?} includes {}, which contains another Release resource",
                release.metadata.name,
                include_path.display()
            )));
        }
        resources.extend(included);
    }
    inputs.extend(include_paths);
    Ok(LoadedReleaseBundle { resources, inputs })
}

/// Check for a literal Release envelope without rendering the candidate file.
pub(crate) fn has_static_release(path: &Path) -> Result<bool> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| NylError::config(format!("Failed to read {}: {error}", path.display())))?;
    Ok(best_effort_parse_yaml_documents(&raw).iter().any(Release::is_release))
}

fn resolve_release_includes(path: &Path, release: &Release) -> Result<Vec<PathBuf>> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let entry_path = path
        .canonicalize()
        .map_err(|error| NylError::config(format!("Failed to resolve Release file {}: {error}", path.display())))?;
    let mut selected = BTreeSet::new();
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };

    for pattern_text in &release.spec.include {
        let pattern = Pattern::new(pattern_text).map_err(|error| {
            NylError::config(format!(
                "Invalid Release {:?} include pattern {pattern_text:?}: {error}",
                release.metadata.name
            ))
        })?;
        let mut pattern_matches = 0usize;
        for entry in walkdir::WalkDir::new(directory).follow_links(false) {
            let entry = entry.map_err(|error| {
                NylError::config(format!(
                    "Failed to inspect Release {:?} include directory {}: {error}",
                    release.metadata.name,
                    directory.display()
                ))
            })?;
            let relative = entry.path().strip_prefix(directory).map_err(|error| {
                NylError::config(format!(
                    "Failed to resolve Release {:?} include candidate {}: {error}",
                    release.metadata.name,
                    entry.path().display()
                ))
            })?;
            if relative.as_os_str().is_empty() || !pattern.matches_path_with(relative, options) {
                continue;
            }
            if entry.file_type().is_symlink() {
                return Err(NylError::config(format!(
                    "Release {:?} include pattern {pattern_text:?} matches symbolic link {}",
                    release.metadata.name,
                    entry.path().display()
                )));
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let extension = entry.path().extension().and_then(|extension| extension.to_str());
            if !matches!(extension, Some("yaml" | "yml" | "json")) {
                return Err(NylError::config(format!(
                    "Release {:?} include pattern {pattern_text:?} matches unsupported manifest file {}",
                    release.metadata.name,
                    entry.path().display()
                )));
            }
            let candidate = entry.path().canonicalize().map_err(|error| {
                NylError::config(format!(
                    "Failed to resolve included file {}: {error}",
                    entry.path().display()
                ))
            })?;
            if candidate == entry_path {
                continue;
            }
            pattern_matches += 1;
            selected.insert(candidate);
        }
        if pattern_matches == 0 {
            return Err(NylError::config(format!(
                "Release {:?} include pattern {pattern_text:?} matched no additional manifest files",
                release.metadata.name
            )));
        }
    }
    Ok(selected.into_iter().collect())
}

fn load_resource_file(
    path: &Path,
    context: &TemplateContext,
    provenance_root: Option<&Path>,
) -> Result<Vec<RenderResource>> {
    // Validate that path is a file, not a directory
    if !path.exists() {
        return Err(NylError::Config(format!("File not found: {}", path.display())));
    }
    if !path.is_file() {
        return Err(NylError::Config(format!(
            "Path must be a file, not a directory: {}. \
            Please specify a YAML/JSON entry file path.",
            path.display()
        )));
    }

    let engine = TemplateEngine::new();
    let ctx_json = context.to_json();
    let mut resources = Vec::new();

    let ext = path.extension().and_then(|s| s.to_str());
    if !matches!(ext, Some("yaml" | "yml" | "json")) {
        return Ok(resources);
    }

    // Skip nyl project configuration files — they are not manifests
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "nyl" | "nyl-project" | "nyl-secrets") {
        return Ok(resources);
    }

    tracing::debug!("Reading manifest file: {}", path.display());
    let raw = std::fs::read_to_string(path).map_err(|e| NylError::Config(format!("Failed to read file: {e}")))?;
    let rendered = engine.render_named(&path.display().to_string(), &raw, &ctx_json)?;
    let source_ctx = crate::util::SourceContext::new(path.to_path_buf());
    let source_path = provenance_path(path, provenance_root);
    resources.extend(
        source_ctx
            .parse_yaml_documents(&rendered)?
            .into_iter()
            .enumerate()
            .map(|(index, value)| RenderResource {
                value,
                provenance: RenderProvenance::source(source_path.clone(), index + 1),
            }),
    );

    Ok(resources)
}

fn provenance_path(path: &Path, root: Option<&Path>) -> PathBuf {
    if let Some(root) = root {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_path_buf();
        }
        if let (Ok(path), Ok(root)) = (path.canonicalize(), root.canonicalize()) {
            if let Ok(relative) = path.strip_prefix(root) {
                return relative.to_path_buf();
            }
        }
    }
    crate::util::path_for_display(path)
}
