//! Release extraction, ApplicationGenerator materialization, Kyverno processing, and deduplication.

use super::*;

/// Deduplicate Kubernetes identities with last occurrence winning.
///
/// The returned map contains each duplicated identity and its total occurrence
/// count.
pub(crate) fn deduplicate_manifests(
    manifests: Vec<serde_json::Value>,
) -> Result<(Vec<serde_json::Value>, std::collections::HashMap<ResourceKey, usize>)> {
    use crate::kubernetes::ResourceKey;
    use std::collections::HashMap;

    let mut seen: HashMap<ResourceKey, usize> = HashMap::new();
    let mut deduplicated = Vec::new();
    let mut duplicate_counts: HashMap<ResourceKey, usize> = HashMap::new();

    for manifest in manifests {
        let key = ResourceKey::from_json_value(&manifest)?;

        if let Some(prev_index) = seen.get(&key) {
            // Duplicate found - replace the previous one with this one (last occurrence wins)
            tracing::warn!("Duplicate resource: {} (keeping last occurrence)", key);
            deduplicated[*prev_index] = manifest;
            // Increment count for this duplicate
            *duplicate_counts.entry(key).or_insert(1) += 1;
        } else {
            // First occurrence - initialize count to 1
            duplicate_counts.insert(key.clone(), 1);
            seen.insert(key, deduplicated.len());
            deduplicated.push(manifest);
        }
    }

    // Filter to only resources with count > 1 (actual duplicates)
    let duplicates: HashMap<_, _> = duplicate_counts.into_iter().filter(|(_, count)| *count > 1).collect();

    Ok((deduplicated, duplicates))
}

/// Render one ApplicationGenerator candidate with Jinja while retaining parse diagnostics.
pub(crate) fn render_yaml_file_with_jinja(
    file_path: &Path,
    source_root: &Path,
    engine: &TemplateEngine,
    ctx_json: &serde_json::Value,
) -> Result<(Vec<serde_json::Value>, Option<String>)> {
    let raw = std::fs::read_to_string(file_path)
        .map_err(|e| NylError::Config(format!("Failed to read file {}: {}", file_path.display(), e)))?;
    let source_ctx = crate::util::SourceContext::new(file_path.to_path_buf());
    let rel_path = file_path
        .strip_prefix(source_root)
        .unwrap_or(file_path)
        .display()
        .to_string();

    Ok(match engine.render_named(&rel_path, &raw, ctx_json) {
        Ok(rendered) => match source_ctx.parse_yaml_documents(&rendered) {
            Ok(docs) => (docs, None),
            Err(e) => {
                tracing::warn!(
                    "YAML parse error after Jinja rendering in {}: {}. \
                         Attempting best-effort document extraction.",
                    file_path.display(),
                    e
                );
                (best_effort_parse_yaml_documents(&rendered), Some(e.to_string()))
            }
        },
        Err(e) => {
            tracing::warn!(
                "Jinja template rendering failed for {}: {}. \
                     Attempting best-effort document extraction.",
                file_path.display(),
                e
            );
            (best_effort_parse_yaml_documents(&raw), Some(e.to_string()))
        }
    })
}

pub(crate) fn process_application_generator(
    generator: &crate::resources::ApplicationGenerator,
    _base_dir: &str,
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    template_context: &TemplateContext,
    render_cache: Option<&crate::render::cache::RenderCache>,
) -> Result<Vec<serde_json::Value>> {
    let target_name = template_context
        .target
        .as_ref()
        .and_then(|target| target.pointer("/metadata/name"))
        .and_then(serde_json::Value::as_str);
    let source_selectors = application_generator_source_selectors(generator);
    tracing::debug!(
        "Processing ApplicationGenerator {}: repoURL={}, targetRevision={}, selectors={}",
        generator.metadata.name,
        generator.spec.source.repo_url,
        generator.spec.source.target_revision,
        source_selectors.join(", ")
    );

    let source_root = resolve_application_generator_source_path(generator, credential_provider, render_cache)?;
    tracing::debug!(
        "ApplicationGenerator {} resolved source root to {}",
        generator.metadata.name,
        source_root.display()
    );

    // Discover candidate files from path/paths selectors, then apply include/exclude filters.
    let yaml_files = find_yaml_files_filtered(
        &source_root,
        &source_selectors,
        &generator.spec.source.include,
        &generator.spec.source.exclude,
    )?;
    tracing::debug!(
        "ApplicationGenerator {} discovered {} YAML file(s) after include/exclude filters",
        generator.metadata.name,
        yaml_files.len()
    );
    let scanned_file_count = yaml_files.len();

    let engine = TemplateEngine::new();
    let ctx_json = template_context.to_json();

    let mut applications = Vec::new();
    let mut missing_release_files = Vec::new();
    let mut missing_release_count = 0usize;

    for file_path in yaml_files {
        tracing::debug!("Reading YAML file: {}", file_path.display());

        let (docs, render_error) = render_yaml_file_with_jinja(&file_path, &source_root, &engine, &ctx_json)?;

        // Extract Release
        let (release, _) = extract_release(&docs)?;

        if let Some(release) = release {
            // Generate ArgoCD Application
            let mut app =
                create_argocd_application_from_generator(&release, &file_path, &source_root, generator, target_name)?;

            // If rendering or parsing failed, create a "husk" application: add error info and disable auto-sync
            if let Some(ref error_msg) = render_error {
                let rel_path = file_path
                    .strip_prefix(&source_root)
                    .unwrap_or(&file_path)
                    .display()
                    .to_string();
                disable_automated_sync(&mut app);
                append_render_error_info(&mut app, &rel_path, error_msg)?;
                tracing::warn!(
                    "Generated husk ArgoCD Application {} from Release in {} (rendering or parsing failed: {})",
                    release.metadata.name,
                    file_path.display(),
                    error_msg
                );
            } else {
                tracing::debug!(
                    "Generated ArgoCD Application {} from Release in {}",
                    release.metadata.name,
                    file_path.display()
                );
            }
            applications.push(app);
        } else {
            tracing::trace!("No Release found in {}, skipping", file_path.display());
            missing_release_count += 1;
            let display_path = file_path
                .strip_prefix(&source_root)
                .map_or_else(|_| file_path.display().to_string(), normalize_relative_path_to_posix);
            missing_release_files.push(display_path);
        }
    }

    if missing_release_count > 0 {
        tracing::warn!(
            "{}",
            missing_release_warning_message(
                generator,
                missing_release_count,
                scanned_file_count,
                &missing_release_files
            )
        );
    }

    tracing::debug!(
        "ApplicationGenerator {} generated {} ArgoCD Application(s) total",
        generator.metadata.name,
        applications.len()
    );
    Ok(applications)
}

/// Parse a multi-document YAML string on a best-effort basis.
///
/// Splits the input on YAML document separators (`---`) and tries to parse each
/// document individually. Documents that fail to parse (e.g., because they contain
/// unrendered Jinja syntax) are silently skipped. This allows extracting parseable
/// documents (like Release) even when other documents in the file are unparseable.
pub(crate) fn best_effort_parse_yaml_documents(raw: &str) -> Vec<serde_json::Value> {
    let mut docs = Vec::new();
    for doc_str in split_yaml_documents(raw) {
        let trimmed = doc_str.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = crate::yaml::parse_yaml_documents_k8s_compatible(trimmed) {
            docs.extend(parsed);
        }
    }
    docs
}

/// Split a YAML string into individual document strings by `---` separators.
pub(crate) fn split_yaml_documents(raw: &str) -> Vec<&str> {
    let mut docs = Vec::new();
    let mut start = 0;
    let mut offset = 0;

    for line in raw.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        if line_content == "---" {
            if offset > start {
                docs.push(&raw[start..offset]);
            }
            start = offset + line.len();
        }
        offset += line.len();
    }

    // Handle last line without trailing newline
    if offset > start {
        let remainder = &raw[start..offset];
        if !remainder.trim().is_empty() {
            docs.push(remainder);
        }
    }
    docs
}

/// Remove automated sync policy from an ArgoCD Application manifest.
pub(crate) fn disable_automated_sync(app: &mut serde_json::Value) {
    if let Some(spec) = app.get_mut("spec").and_then(|v| v.as_object_mut()) {
        if let Some(sync_policy) = spec.get_mut("syncPolicy").and_then(|v| v.as_object_mut()) {
            sync_policy.remove("automated");
        }
    }
}

/// Add a rendering/parsing error entry to the Application's spec.info field.
pub(crate) fn append_render_error_info(app: &mut serde_json::Value, file_path: &str, error_msg: &str) -> Result<()> {
    let spec = app
        .get_mut("spec")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| NylError::Config("Generated Application is missing spec".to_string()))?;
    let info_value = spec
        .entry("info".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !info_value.is_array() {
        let previous = std::mem::take(info_value);
        *info_value = serde_json::Value::Array(vec![previous]);
    }
    let info_items = info_value
        .as_array_mut()
        .ok_or_else(|| NylError::Config("Application spec.info is not an array".to_string()))?;
    info_items.push(serde_json::json!({
        "name": "nyl-render-error",
        "value": format!("Failed to render or parse {}: {}", file_path, error_msg),
    }));
    Ok(())
}

pub(crate) fn missing_release_warning_message(
    generator: &crate::resources::ApplicationGenerator,
    missing_release_count: usize,
    scanned_file_count: usize,
    skipped_files: &[String],
) -> String {
    let selectors = application_generator_source_selectors(generator);
    let selectors_text = if selectors.is_empty() {
        "<none>".to_string()
    } else {
        selectors.join(", ")
    };
    let base = format!(
        "ApplicationGenerator {} (repoURL={}, targetRevision={}, source paths={}): skipped {}/{} file(s) because no Release was found.",
        generator.metadata.name,
        generator.spec.source.repo_url,
        generator.spec.source.target_revision,
        selectors_text,
        missing_release_count,
        scanned_file_count
    );
    if skipped_files.is_empty() {
        base
    } else {
        format!("{} Skipped files: {}", base, skipped_files.join(", "))
    }
}

pub(crate) fn application_generator_source_selectors(
    generator: &crate::resources::ApplicationGenerator,
) -> Vec<String> {
    if let Some(path) = &generator.spec.source.path {
        return vec![path.clone()];
    }
    generator.spec.source.paths.clone().unwrap_or_default()
}

/// Resolve ApplicationGenerator source to a local path.
///
/// If `NYL_APPGEN_REPO_PATH_OVERRIDE` is set, all ApplicationGenerators are resolved
/// against that local repository root and no Git clone/worktree is used.
/// Otherwise, falls back to Git resolution via repoURL + targetRevision.
pub(crate) fn resolve_application_generator_source_path(
    generator: &crate::resources::ApplicationGenerator,
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    render_cache: Option<&crate::render::cache::RenderCache>,
) -> Result<PathBuf> {
    const APPGEN_REPO_PATH_OVERRIDE: &str = "NYL_APPGEN_REPO_PATH_OVERRIDE";

    if let Ok(override_root_raw) = std::env::var(APPGEN_REPO_PATH_OVERRIDE) {
        let override_root_raw = override_root_raw.trim();
        if !override_root_raw.is_empty() {
            let override_root = resolve_override_root_path(APPGEN_REPO_PATH_OVERRIDE, override_root_raw)?;
            if !override_root.exists() {
                return Err(NylError::Config(format!(
                    "Environment variable {} points to a path that does not exist: {}",
                    APPGEN_REPO_PATH_OVERRIDE,
                    override_root.display()
                )));
            }
            if !override_root.is_dir() {
                return Err(NylError::Config(format!(
                    "Environment variable {} must point to a directory, got: {}",
                    APPGEN_REPO_PATH_OVERRIDE,
                    override_root.display()
                )));
            }

            tracing::debug!(
                "Using {} for ApplicationGenerator {} (repoURL={}, targetRevision={})",
                APPGEN_REPO_PATH_OVERRIDE,
                generator.metadata.name,
                generator.spec.source.repo_url,
                generator.spec.source.target_revision
            );
            if let Some(cache) = render_cache {
                cache.observe_source(crate::render::cache::SourceOperation::GitWorktreeReuse);
            }
            return Ok(override_root);
        }
    }

    if let Some(local_repo_root) = try_resolve_application_generator_source_from_local_git_repo(generator) {
        if let Some(cache) = render_cache {
            cache.observe_source(crate::render::cache::SourceOperation::GitWorktreeReuse);
        }
        return Ok(local_repo_root);
    }

    let mut git_manager =
        crate::git::GitManager::with_credential_provider(credential_provider)?.with_render_cache(render_cache.cloned());
    Ok(git_manager.resolve_ref(
        &generator.spec.source.repo_url,
        Some(&generator.spec.source.target_revision),
        None,
    )?)
}

pub(crate) fn try_resolve_application_generator_source_from_local_git_repo(
    generator: &crate::resources::ApplicationGenerator,
) -> Option<PathBuf> {
    let cwd = match resolve_current_pwd() {
        Ok(path) => path,
        Err(err) => {
            tracing::trace!(
                "Skipping local git worktree reuse for ApplicationGenerator {}: failed to resolve current directory: {}",
                generator.metadata.name,
                err
            );
            return None;
        }
    };

    let repo = match git2::Repository::discover(&cwd) {
        Ok(repo) => repo,
        Err(err) => {
            tracing::trace!(
                "Skipping local git worktree reuse for ApplicationGenerator {}: no Git repository discovered from {}: {}",
                generator.metadata.name,
                cwd.display(),
                err
            );
            return None;
        }
    };

    let Some(repo_root) = repo_root_path(&repo) else {
        tracing::trace!(
            "Skipping local git worktree reuse for ApplicationGenerator {}: discovered repository has no worktree root",
            generator.metadata.name
        );
        return None;
    };

    let requested_url = crate::git::normalize_git_url_for_equality(&generator.spec.source.repo_url);
    let remote_urls = local_git_remote_urls(&repo);
    if remote_urls.is_empty() {
        tracing::trace!(
            "Skipping local git worktree reuse for ApplicationGenerator {}: discovered repository has no remote URLs",
            generator.metadata.name
        );
        return None;
    }

    if !remote_urls
        .iter()
        .any(|remote_url| crate::git::normalize_git_url_for_equality(remote_url) == requested_url)
    {
        let local_remote_urls = remote_urls
            .iter()
            .map(|url| crate::util::sanitize_url(url))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::trace!(
            "Skipping local git worktree reuse for ApplicationGenerator {}: repoURL mismatch (requested={}, local remotes=[{}])",
            generator.metadata.name,
            crate::util::sanitize_url(&generator.spec.source.repo_url),
            local_remote_urls
        );
        return None;
    }

    let requested_revision = generator.spec.source.target_revision.trim();
    if requested_revision == "HEAD" {
        tracing::debug!(
            "Reusing local git worktree for ApplicationGenerator {} at {} (repoURL={}, targetRevision=HEAD)",
            generator.metadata.name,
            repo_root.display(),
            crate::util::sanitize_url(&generator.spec.source.repo_url)
        );
        return Some(repo_root);
    }

    let Some(current_branch) = current_local_branch_name(&repo) else {
        tracing::trace!(
            "Skipping local git worktree reuse for ApplicationGenerator {}: current checkout is detached HEAD and targetRevision requires branch match (requested={})",
            generator.metadata.name,
            requested_revision
        );
        return None;
    };

    if requested_revision != current_branch {
        tracing::trace!(
            "Skipping local git worktree reuse for ApplicationGenerator {}: targetRevision mismatch (requested={}, current branch={})",
            generator.metadata.name,
            requested_revision,
            current_branch
        );
        return None;
    }

    tracing::debug!(
        "Reusing local git worktree for ApplicationGenerator {} at {} (repoURL={}, targetRevision={})",
        generator.metadata.name,
        repo_root.display(),
        crate::util::sanitize_url(&generator.spec.source.repo_url),
        requested_revision
    );
    Some(repo_root)
}

pub(crate) fn repo_root_path(repo: &git2::Repository) -> Option<PathBuf> {
    repo.workdir().map(Path::to_path_buf).or_else(|| {
        if repo.is_bare() {
            Some(repo.path().to_path_buf())
        } else {
            repo.path().parent().map(Path::to_path_buf)
        }
    })
}

pub(crate) fn local_git_remote_urls(repo: &git2::Repository) -> Vec<String> {
    let mut urls = std::collections::BTreeSet::new();
    let Ok(remotes) = repo.remotes() else {
        return Vec::new();
    };

    for remote_name in remotes.iter().flatten() {
        let Ok(remote) = repo.find_remote(remote_name) else {
            continue;
        };

        if let Some(url) = remote.url() {
            urls.insert(url.to_string());
        }
        if let Some(push_url) = remote.pushurl() {
            urls.insert(push_url.to_string());
        }
    }

    urls.into_iter().collect()
}

pub(crate) fn current_local_branch_name(repo: &git2::Repository) -> Option<String> {
    let Ok(head) = repo.head() else {
        return None;
    };
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(str::to_string)
}

/// Resolve override root path from env var value.
///
/// Relative values are resolved against shell `PWD` if present, otherwise
/// against the process current directory.
pub(crate) fn resolve_override_root_path(env_var_name: &str, raw: &str) -> Result<PathBuf> {
    if raw == "@git" {
        return resolve_git_repo_root_from_current_pwd(env_var_name);
    }

    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        return Ok(candidate);
    }

    Ok(resolve_current_pwd()?.join(candidate))
}

pub(crate) fn resolve_git_repo_root_from_current_pwd(env_var_name: &str) -> Result<PathBuf> {
    let cwd = resolve_current_pwd()?;
    let repo = git2::Repository::discover(&cwd).map_err(|e| {
        NylError::Config(format!(
            "Environment variable {} is set to @git, but no Git repository root could be discovered from {}: {}",
            env_var_name,
            cwd.display(),
            e
        ))
    })?;

    if let Some(workdir) = repo.workdir() {
        return Ok(workdir.to_path_buf());
    }

    let repo_path = repo.path();
    if let Some(parent) = repo_path.parent() {
        return Ok(parent.to_path_buf());
    }

    Err(NylError::Config(format!(
        "Environment variable {} is set to @git, but the discovered Git repository has no parent directory: {}",
        env_var_name,
        repo_path.display()
    )))
}

pub(crate) fn resolve_current_pwd() -> Result<PathBuf> {
    if let Ok(pwd) = std::env::var("PWD") {
        let pwd_path = PathBuf::from(pwd);
        if pwd_path.is_absolute() {
            return Ok(pwd_path);
        }
    }

    std::env::current_dir()
        .map_err(|e| NylError::Config(format!("Failed to get current directory for path resolution: {}", e)))
}

/// Find YAML files matching path/paths selectors and include/exclude patterns.
pub(crate) fn find_yaml_files_filtered(
    source_root: &Path,
    selectors: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<Vec<std::path::PathBuf>> {
    if !source_root.exists() {
        return Err(NylError::Config(format!(
            "Source path does not exist: {}",
            source_root.display()
        )));
    }
    if !source_root.is_dir() {
        return Err(NylError::Config(format!(
            "Source path must be a directory: {}",
            source_root.display()
        )));
    }

    let mut warnings = ScanWarnings::default();
    let mut candidates = std::collections::BTreeSet::new();

    for selector in selectors {
        collect_selector_candidates(source_root, selector, &mut candidates, &mut warnings)?;
    }

    let mut files = Vec::new();
    for candidate in candidates {
        let rel = candidate.strip_prefix(source_root).map_err(|e| {
            NylError::Config(format!(
                "Failed to compute relative path for {} under {}: {}",
                candidate.display(),
                source_root.display(),
                e
            ))
        })?;

        if !matches_glob_patterns(rel, include)? {
            continue;
        }
        if matches_glob_patterns(rel, exclude)? {
            continue;
        }
        files.push(candidate);
    }

    if warnings.unreadable_entries > 0 {
        let examples = if warnings.examples.is_empty() {
            String::new()
        } else {
            format!(" Examples: {}", warnings.examples.join(" | "))
        };
        tracing::warn!(
            "Skipped {} unreadable path(s) while scanning {}.{}",
            warnings.unreadable_entries,
            source_root.display(),
            examples
        );
    }

    Ok(files)
}

#[derive(Default)]
pub(crate) struct ScanWarnings {
    unreadable_entries: usize,
    examples: Vec<String>,
}

pub(crate) fn collect_selector_candidates(
    source_root: &Path,
    selector: &str,
    candidates: &mut std::collections::BTreeSet<PathBuf>,
    warnings: &mut ScanWarnings,
) -> Result<()> {
    if selector_has_glob(selector) {
        let pattern_path = source_root.join(selector);
        let pattern_str = pattern_path.to_string_lossy().to_string();
        let entries = glob(&pattern_str)
            .map_err(|e| NylError::Config(format!("Invalid source selector glob '{}': {}", selector, e)))?;

        for entry in entries {
            match entry {
                Ok(path) => collect_path_candidate(path, candidates, warnings),
                Err(e) => record_scan_warning(warnings, format!("{}", e)),
            }
        }
        return Ok(());
    }

    let selected = source_root.join(selector);
    if !selected.exists() {
        return Err(NylError::Config(format!(
            "Source selector '{}' does not exist under {}",
            selector,
            source_root.display()
        )));
    }
    collect_path_candidate(selected, candidates, warnings);
    Ok(())
}

pub(crate) fn collect_path_candidate(
    path: PathBuf,
    candidates: &mut std::collections::BTreeSet<PathBuf>,
    warnings: &mut ScanWarnings,
) {
    if path.is_file() {
        candidates.insert(path);
        return;
    }

    if path.is_dir() {
        let read_dir = match std::fs::read_dir(&path) {
            Ok(read_dir) => read_dir,
            Err(e) => {
                record_scan_warning(warnings, format!("{}: {}", path.display(), e));
                return;
            }
        };

        for entry in read_dir {
            match entry {
                Ok(entry) => {
                    let child = entry.path();
                    if child.is_file() {
                        candidates.insert(child);
                    }
                }
                Err(e) => {
                    record_scan_warning(warnings, format!("{}: {}", path.display(), e));
                }
            }
        }
    }
}

pub(crate) fn record_scan_warning(warnings: &mut ScanWarnings, message: String) {
    warnings.unreadable_entries += 1;
    if warnings.examples.len() < 3 {
        warnings.examples.push(message);
    }
}

pub(crate) fn selector_has_glob(selector: &str) -> bool {
    selector.contains('*') || selector.contains('?') || selector.contains('[')
}

/// Check if relative path matches any glob pattern.
/// Patterns with path separators match the full relative path.
/// Patterns without separators match basename only.
pub(crate) fn matches_glob_patterns(relative_path: &Path, patterns: &[String]) -> Result<bool> {
    let rel_posix = normalize_relative_path_to_posix(relative_path);
    let file_name = relative_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    for pattern in patterns {
        let glob_pattern = Pattern::new(pattern)
            .map_err(|e| NylError::Config(format!("Invalid include/exclude glob pattern '{}': {}", pattern, e)))?;
        let target = if pattern.contains('/') || pattern.contains('\\') {
            rel_posix.as_str()
        } else {
            file_name
        };
        if glob_pattern.matches(target) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) const NYL_CUSTOMIZATION_WARNING_NAME: &str = "release-customization-warning";
pub(crate) const IMMUTABLE_APPLICATION_PATH_PATTERNS: &[&str] = &[
    "apiVersion",
    "kind",
    "metadata.name",
    "metadata.namespace",
    "spec.project",
    "spec.source.repoURL",
    "spec.source.path",
    "spec.source.targetRevision",
    "spec.source.plugin.name",
    "spec.source.plugin.env.**",
    "spec.destination.server",
    "spec.destination.namespace",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IgnoredOverrideReason {
    Disallowed,
    Invalid,
    Unsupported,
}

impl IgnoredOverrideReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Disallowed => "disallowed",
            Self::Invalid => "invalid",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IgnoredOverride {
    path: String,
    reason: IgnoredOverrideReason,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum OverrideLeafOperation {
    Append,
    Replace,
}

#[derive(Debug, Clone)]
pub(crate) struct OverrideLeaf {
    segments: Vec<String>,
    /// Canonical dotted path (without `+` prefixes), used for policy checks.
    path: String,
    /// Original key of the leaf segment (e.g. `+syncOptions`), used in warning messages.
    original_key: String,
    value: serde_json::Value,
    operation: OverrideLeafOperation,
}

impl OverrideLeaf {
    /// Return the display path for warning messages, using the original key (with `+` prefix if present).
    fn display_path(&self) -> String {
        let mut segments = self.segments.clone();
        if let Some(last) = segments.last_mut() {
            last.clone_from(&self.original_key);
        }
        join_field_path_segments(&segments)
    }
}

/// Create ArgoCD Application from generator config
pub(crate) fn create_argocd_application_from_generator(
    release: &Release,
    file_path: &Path,
    source_root: &Path,
    generator: &crate::resources::ApplicationGenerator,
    target_name: Option<&str>,
) -> Result<serde_json::Value> {
    // Calculate subdirectory relative to repository root
    let rel_dir = file_path
        .strip_prefix(source_root)
        .unwrap_or(file_path)
        .parent()
        .unwrap_or(Path::new(""));

    // Normalize the relative directory to POSIX-style separators for ArgoCD.
    let rel_dir_normalized = normalize_relative_path_to_posix(rel_dir);
    let template_input = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| NylError::Config(format!("Invalid file name: {}", file_path.display())))?;

    // Application path is the directory containing the Release, relative to repository root.
    let path_str = if rel_dir_normalized.is_empty() {
        ".".to_string()
    } else {
        rel_dir_normalized
    };

    let mut plugin_env = vec![
        serde_json::json!({"name": "NYL_RELEASE_NAME", "value": release.metadata.name}),
        serde_json::json!({"name": "NYL_RELEASE_NAMESPACE", "value": release.metadata.namespace}),
        serde_json::json!({"name": "NYL_CMP_TEMPLATE_INPUT", "value": template_input}),
    ];
    if let Some(target_name) = target_name {
        plugin_env.push(serde_json::json!({"name": "NYL_CMP_TARGET", "value": target_name}));
    }
    plugin_env.extend(
        generator
            .spec
            .source
            .plugin_env
            .iter()
            .map(|(name, value)| serde_json::json!({"name": name, "value": value})),
    );

    // Build the Application manifest
    let mut app = serde_json::json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {
            "name": release.metadata.name,
            "namespace": generator.spec.destination.namespace,
        },
        "spec": {
            "project": generator.spec.project,
            "source": {
                "repoURL": generator.spec.source.repo_url,
                "path": path_str,
                "targetRevision": generator.spec.source.target_revision,
                "plugin": {
                    "name": "nyl-v2",
                    "env": plugin_env,
                },
            },
            "destination": {
                "server": generator.spec.destination.server,
                "namespace": release.metadata.namespace,
            },
        },
    });

    // Add labels if present
    if !generator.spec.labels.is_empty() {
        app["metadata"]["labels"] = serde_json::to_value(&generator.spec.labels)?;
    }

    // Add annotations if present
    if !generator.spec.annotations.is_empty() {
        app["metadata"]["annotations"] = serde_json::to_value(&generator.spec.annotations)?;
    }

    // Add sync policy if present
    if let Some(ref sync_policy) = generator.spec.sync_policy {
        app["spec"]["syncPolicy"] = serde_json::to_value(sync_policy)?;
    }

    apply_release_customization_overrides(&mut app, release, generator)?;

    Ok(app)
}

pub(crate) fn apply_release_customization_overrides(
    app: &mut serde_json::Value,
    release: &Release,
    generator: &crate::resources::ApplicationGenerator,
) -> Result<()> {
    let Some(application_override) = release
        .spec
        .argocd
        .as_ref()
        .and_then(|argocd| argocd.application_override.clone())
    else {
        return Ok(());
    };

    let mut override_leaves = Vec::new();
    let mut prefix = Vec::new();
    flatten_override_leaves(
        &serde_json::Value::Object(application_override),
        &mut prefix,
        &mut override_leaves,
    );

    if override_leaves.is_empty() {
        return Ok(());
    }

    let mut replace_leaves = Vec::new();
    let mut append_leaves = Vec::new();
    let mut ignored = Vec::new();

    let customization =
        generator
            .spec
            .release_customization
            .clone()
            .unwrap_or(crate::resources::ReleaseCustomizationPolicy {
                allowed_paths: None,
                denied_paths: Vec::new(),
            });
    let allowed_paths = customization.effective_allowed_paths();
    let denied_paths = &customization.denied_paths;

    for leaf in override_leaves {
        if !is_supported_application_field_path(&leaf.path) {
            ignored.push(IgnoredOverride {
                path: leaf.display_path(),
                reason: IgnoredOverrideReason::Unsupported,
            });
            continue;
        }

        if path_matches_any(&leaf.path, IMMUTABLE_APPLICATION_PATH_PATTERNS)? {
            ignored.push(IgnoredOverride {
                path: leaf.display_path(),
                reason: IgnoredOverrideReason::Disallowed,
            });
            continue;
        }

        let denied = path_matches_any(&leaf.path, denied_paths)?;
        let allowed = path_matches_any(&leaf.path, &allowed_paths)?;
        if denied || !allowed {
            ignored.push(IgnoredOverride {
                path: leaf.display_path(),
                reason: IgnoredOverrideReason::Disallowed,
            });
        } else {
            match leaf.operation {
                OverrideLeafOperation::Replace => replace_leaves.push(leaf),
                OverrideLeafOperation::Append => {
                    if is_supported_application_array_field_path(&leaf.path) {
                        append_leaves.push(leaf);
                    } else {
                        ignored.push(IgnoredOverride {
                            path: leaf.display_path(),
                            reason: IgnoredOverrideReason::Invalid,
                        });
                    }
                }
            }
        }
    }

    if !replace_leaves.is_empty() {
        let override_value = build_override_value(&replace_leaves);
        *app = deep_merge_value(Some(app.clone()), override_value);
    }

    for leaf in append_leaves {
        let serde_json::Value::Array(items) = &leaf.value else {
            ignored.push(IgnoredOverride {
                path: leaf.display_path(),
                reason: IgnoredOverrideReason::Invalid,
            });
            continue;
        };
        let items = items.clone();
        if let Err(reason) = append_override_items(app, &leaf.segments, items) {
            ignored.push(IgnoredOverride {
                path: leaf.display_path(),
                reason,
            });
        }
    }

    if !ignored.is_empty() {
        append_customization_warning(app, &ignored)?;
    }

    Ok(())
}

pub(crate) fn flatten_override_leaves(
    value: &serde_json::Value,
    prefix: &mut Vec<String>,
    leaves: &mut Vec<OverrideLeaf>,
) {
    if let serde_json::Value::Object(map) = value {
        if map.is_empty() {
            return;
        }
        for (key, child) in map {
            let (canonical_key, operation) = parse_override_key(key);
            prefix.push(canonical_key);
            if matches!(operation, OverrideLeafOperation::Append) {
                leaves.push(OverrideLeaf {
                    segments: prefix.clone(),
                    path: join_field_path_segments(prefix),
                    original_key: key.clone(),
                    value: child.clone(),
                    operation: OverrideLeafOperation::Append,
                });
            } else {
                flatten_override_leaves(child, prefix, leaves);
            }
            prefix.pop();
        }
        return;
    }

    leaves.push(OverrideLeaf {
        segments: prefix.clone(),
        path: join_field_path_segments(prefix),
        original_key: prefix.last().cloned().unwrap_or_default(),
        value: value.clone(),
        operation: OverrideLeafOperation::Replace,
    });
}

pub(crate) fn parse_override_key(key: &str) -> (String, OverrideLeafOperation) {
    if let Some(stripped) = key.strip_prefix('+') {
        if !stripped.is_empty() {
            return (stripped.to_string(), OverrideLeafOperation::Append);
        }
    }
    (key.to_string(), OverrideLeafOperation::Replace)
}

pub(crate) fn build_override_value(leaves: &[OverrideLeaf]) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    for leaf in leaves {
        insert_override_leaf(&mut root, &leaf.segments, leaf.value.clone());
    }
    serde_json::Value::Object(root)
}

pub(crate) fn insert_override_leaf(
    root: &mut serde_json::Map<String, serde_json::Value>,
    segments: &[String],
    value: serde_json::Value,
) {
    if segments.is_empty() {
        return;
    }
    if segments.len() == 1 {
        root.insert(segments[0].clone(), value);
        return;
    }

    let entry = root
        .entry(segments[0].clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(map) = entry.as_object_mut() {
        insert_override_leaf(map, &segments[1..], value);
    }
}

pub(crate) fn path_matches_any(path: &str, patterns: &[impl AsRef<str>]) -> Result<bool> {
    for pattern in patterns {
        if path_matches_glob(path, pattern.as_ref())? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn coerce_to_object(value: &mut serde_json::Value) -> std::result::Result<(), IgnoredOverrideReason> {
    match value {
        serde_json::Value::Object(_) => Ok(()),
        serde_json::Value::Null => {
            *value = serde_json::Value::Object(serde_json::Map::new());
            Ok(())
        }
        _ => Err(IgnoredOverrideReason::Invalid),
    }
}

pub(crate) fn append_override_items(
    current: &mut serde_json::Value,
    segments: &[String],
    items: Vec<serde_json::Value>,
) -> std::result::Result<(), IgnoredOverrideReason> {
    if segments.is_empty() {
        return Err(IgnoredOverrideReason::Invalid);
    }

    coerce_to_object(current)?;
    let map = current.as_object_mut().unwrap();

    if segments.len() == 1 {
        let entry = map
            .entry(segments[0].clone())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        match entry {
            serde_json::Value::Array(array) => {
                array.extend(items);
                Ok(())
            }
            serde_json::Value::Null => {
                *entry = serde_json::Value::Array(items);
                Ok(())
            }
            _ => Err(IgnoredOverrideReason::Invalid),
        }
    } else {
        let entry = map
            .entry(segments[0].clone())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        coerce_to_object(entry)?;
        append_override_items(entry, &segments[1..], items)
    }
}

pub(crate) fn append_customization_warning(app: &mut serde_json::Value, ignored: &[IgnoredOverride]) -> Result<()> {
    let mut grouped: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for item in ignored {
        grouped.entry(item.reason.as_str()).or_default().push(item.path.clone());
    }
    for values in grouped.values_mut() {
        values.sort();
        values.dedup();
    }

    let summary = grouped
        .iter()
        .map(|(reason, paths)| format!("{}={} ({})", reason, paths.len(), summarize_paths(paths)))
        .collect::<Vec<_>>()
        .join("; ");
    let warning_value = format!("Ignored Release applicationOverride fields: {}", summary);

    let spec = app
        .get_mut("spec")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| NylError::Config("Generated Application is missing spec".to_string()))?;
    let info_value = spec
        .entry("info".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !info_value.is_array() {
        let previous = std::mem::take(info_value);
        *info_value = serde_json::Value::Array(vec![previous]);
    }

    let info_items = info_value
        .as_array_mut()
        .ok_or_else(|| NylError::Config("Application spec.info is not an array".to_string()))?;

    let mut existing_index = None;
    for (idx, item) in info_items.iter().enumerate() {
        if item
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|name| name == NYL_CUSTOMIZATION_WARNING_NAME)
        {
            existing_index = Some(idx);
            break;
        }
    }

    let warning_item = serde_json::json!({
        "name": NYL_CUSTOMIZATION_WARNING_NAME,
        "value": warning_value,
    });
    if let Some(idx) = existing_index {
        info_items[idx] = warning_item;
    } else {
        info_items.push(warning_item);
    }

    Ok(())
}

pub(crate) fn summarize_paths(paths: &[String]) -> String {
    const LIMIT: usize = 5;
    if paths.len() <= LIMIT {
        return paths.join(", ");
    }
    let head = paths.iter().take(LIMIT).cloned().collect::<Vec<_>>();
    format!("{}, +{} more", head.join(", "), paths.len() - LIMIT)
}

/// Normalize a relative path to POSIX-style separators.
pub(crate) fn normalize_relative_path_to_posix(path: &Path) -> String {
    let mut normalized = String::new();
    for component in path.components() {
        if let std::path::Component::Normal(os_str) = component {
            if !normalized.is_empty() {
                normalized.push('/');
            }
            normalized.push_str(&os_str.to_string_lossy());
        }
    }
    normalized
}

pub(crate) fn normalize_emitted_manifests(manifests: &mut [serde_json::Value]) {
    for manifest in manifests {
        strip_empty_metadata_labels(manifest);
    }
}

pub(crate) fn resolve_strip_empty_metadata_labels_mode(
    project_mode: StripEmptyMetadataLabelsMode,
    release: Option<&Release>,
) -> StripEmptyMetadataLabelsMode {
    release
        .and_then(|release| release.spec.strip_empty_metadata_labels)
        .unwrap_or(project_mode)
}

pub(crate) fn prepare_manifests_for_output(
    manifests: &[serde_json::Value],
    strip_empty_metadata_labels: bool,
) -> Vec<serde_json::Value> {
    let mut emitted_manifests = manifests.to_vec();
    if strip_empty_metadata_labels {
        normalize_emitted_manifests(&mut emitted_manifests);
    }
    emitted_manifests
}

fn strip_empty_metadata_labels(manifest: &mut serde_json::Value) {
    let Some(metadata) = manifest.get_mut("metadata").and_then(|value| value.as_object_mut()) else {
        return;
    };

    let should_remove = metadata
        .get("labels")
        .and_then(|value| value.as_object())
        .is_some_and(serde_json::Map::is_empty);

    if should_remove {
        metadata.remove("labels");
    }
}
