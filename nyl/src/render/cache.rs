//! Versioned, content-addressed storage shared by manifest and tree rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use clap::Args;
use colored::Colorize;
use getrandom::fill as fill_random;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{NylError, Result};

const CACHE_LAYOUT_VERSION: &str = "v1";
const SECRET_KEY_FILE: &str = "secret-fingerprint-key";

/// Cache controls shared by all manifest-rendering commands.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderCacheArgs {
    /// Retrieve external inputs again and rebuild cached render artifacts.
    #[arg(long, conflicts_with = "no_cache")]
    pub refresh: bool,

    /// Use no persistent cache reads or writes.
    #[arg(long)]
    pub no_cache: bool,
}

impl RenderCacheArgs {
    pub fn mode(self) -> CacheMode {
        if self.no_cache {
            CacheMode::Disabled
        } else if self.refresh {
            CacheMode::Refresh
        } else {
            CacheMode::Default
        }
    }
}

/// Persistent cache behavior for one command invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    #[default]
    Default,
    Refresh,
    Disabled,
}

/// Rendering layer that performed one observable cache action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheLayer {
    Target,
    Release,
    Helm,
}

/// External source operation performed while rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceOperation {
    RemoteManifestDownload,
    HelmChartPull,
    GitRepositoryClone,
    GitRepositoryReuse,
    GitRefRefresh,
    GitWorktreeCreate,
    GitWorktreeReuse,
}

/// Outcome of one cache lookup or publication decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheOutcome {
    Hit,
    Miss,
    Invalidated,
    Bypassed,
    Refreshed,
    Stored,
    Corrupt,
}

/// Aggregated cache activity for one rendering command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    layers: BTreeMap<CacheLayer, CacheLayerStats>,
    target_reuse: Option<TargetReuse>,
    release_helm_renders_avoided: usize,
    sources: BTreeMap<SourceOperation, usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetReuse {
    releases: usize,
    helm_renders: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CacheLayerStats {
    outcomes: BTreeMap<CacheOutcome, usize>,
    bypass_reasons: BTreeMap<String, usize>,
}

impl CacheStats {
    fn observe(&mut self, layer: CacheLayer, outcome: CacheOutcome, reasons: &[String]) {
        let stats = self.layers.entry(layer).or_default();
        *stats.outcomes.entry(outcome).or_default() += 1;
        if outcome == CacheOutcome::Bypassed {
            for reason in reasons {
                *stats.bypass_reasons.entry(reason.clone()).or_default() += 1;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.sources.is_empty()
    }

    fn has_reportable_work(&self) -> bool {
        self.target_reuse.is_some()
            || self.layers.values().any(CacheLayerStats::has_reportable_work)
            || !self.sources.is_empty()
    }

    fn source_count(&self, operation: SourceOperation) -> usize {
        self.sources.get(&operation).copied().unwrap_or_default()
    }

    fn source_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let remote_manifests = self.source_count(SourceOperation::RemoteManifestDownload);
        if remote_manifests > 0 {
            lines.push(stat_line(
                "Remote manifests",
                styled_count(remote_manifests, "downloaded", false),
            ));
        }
        let helm_pulls = self.source_count(SourceOperation::HelmChartPull);
        if helm_pulls > 0 {
            lines.push(stat_line("Helm charts", styled_count(helm_pulls, "pulled", false)));
        }
        let repository_clones = self.source_count(SourceOperation::GitRepositoryClone);
        let repository_reuses = self.source_count(SourceOperation::GitRepositoryReuse);
        let ref_refreshes = self.source_count(SourceOperation::GitRefRefresh);
        if repository_clones + repository_reuses + ref_refreshes > 0 {
            lines.push(stat_line(
                "Git repositories",
                styled_work_counts(&[
                    (repository_clones, "cloned", false),
                    (repository_reuses, "reused", true),
                    (
                        ref_refreshes,
                        if ref_refreshes == 1 {
                            "ref refreshed"
                        } else {
                            "refs refreshed"
                        },
                        false,
                    ),
                ]),
            ));
        }
        let worktree_creations = self.source_count(SourceOperation::GitWorktreeCreate);
        let worktree_reuses = self.source_count(SourceOperation::GitWorktreeReuse);
        if worktree_creations + worktree_reuses > 0 {
            lines.push(stat_line(
                "Git worktrees",
                styled_work_counts(&[
                    (worktree_creations, "created", false),
                    (worktree_reuses, "reused", true),
                ]),
            ));
        }
        lines
    }
}

impl fmt::Display for CacheStats {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = vec!["Render statistics".bold().to_string()];
        let mut cache_lines = Vec::new();
        if let Some(reuse) = &self.target_reuse {
            cache_lines.push(stat_line("Target tree", styled_outcome("reused", true)));
            if reuse.releases > 0 {
                cache_lines.push(stat_line(
                    "Release renders",
                    styled_count(reuse.releases, "avoided", true),
                ));
            }
            if reuse.helm_renders > 0 {
                cache_lines.push(stat_line(
                    "Helm renders",
                    styled_count(reuse.helm_renders, "avoided", true),
                ));
            }
        } else {
            if let Some(stats) = self
                .layers
                .get(&CacheLayer::Target)
                .filter(|stats| stats.has_reportable_work())
            {
                let mut value = styled_outcome("rebuilt", false);
                append_bypass_reasons(&mut value, stats);
                cache_lines.push(stat_line("Target tree", value));
            }
            if let Some(stats) = self
                .layers
                .get(&CacheLayer::Release)
                .filter(|stats| stats.has_reportable_work())
            {
                let mut value = styled_work_counts(&[
                    (stats.count(CacheOutcome::Hit), "reused", true),
                    (stats.completed_work(), "rendered", false),
                ]);
                append_bypass_reasons(&mut value, stats);
                cache_lines.push(stat_line("Release renders", value));
            }
            let avoided_helm = self.release_helm_renders_avoided;
            if avoided_helm > 0
                || self
                    .layers
                    .get(&CacheLayer::Helm)
                    .is_some_and(CacheLayerStats::has_reportable_work)
            {
                let stats = self.layers.get(&CacheLayer::Helm);
                let mut value = styled_work_counts(&[
                    (avoided_helm, "avoided", true),
                    (stats.map_or(0, |stats| stats.count(CacheOutcome::Hit)), "reused", true),
                    (stats.map_or(0, CacheLayerStats::completed_work), "rendered", false),
                ]);
                if let Some(stats) = stats {
                    append_bypass_reasons(&mut value, stats);
                }
                cache_lines.push(stat_line("Helm renders", value));
            }
        }
        if !cache_lines.is_empty() {
            lines.push(format!("  {}", "Cache".cyan().bold()));
            lines.extend(cache_lines);
        }
        let source_lines = self.source_lines();
        if !source_lines.is_empty() {
            lines.push(format!("  {}", "Sources".cyan().bold()));
            lines.extend(source_lines);
        }
        write!(formatter, "{}", lines.join("\n"))
    }
}

fn stat_line(label: &str, value: String) -> String {
    format!("    {label:<20}{value}")
}

impl CacheLayerStats {
    fn has_reportable_work(&self) -> bool {
        self.count(CacheOutcome::Hit) > 0 || self.completed_work() > 0
    }

    fn count(&self, outcome: CacheOutcome) -> usize {
        self.outcomes.get(&outcome).copied().unwrap_or_default()
    }

    fn completed_work(&self) -> usize {
        self.count(CacheOutcome::Stored) + self.count(CacheOutcome::Bypassed)
    }
}

fn styled_outcome(outcome: &str, positive: bool) -> String {
    if positive {
        outcome.green().to_string()
    } else {
        outcome.to_string()
    }
}

fn styled_count(count: usize, outcome: &str, positive: bool) -> String {
    format!(
        "{} {}",
        count.to_string().cyan().bold(),
        styled_outcome(outcome, positive)
    )
}

fn styled_work_counts(counts: &[(usize, &str, bool)]) -> String {
    counts
        .iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, outcome, positive)| styled_count(*count, outcome, *positive))
        .collect::<Vec<_>>()
        .join(&format!(" {} ", "·".dimmed()))
}

fn append_bypass_reasons(value: &mut String, stats: &CacheLayerStats) {
    if stats.bypass_reasons.is_empty() {
        return;
    }
    value.push_str(" (not cacheable: ");
    for (index, (reason, count)) in stats.bypass_reasons.iter().enumerate() {
        if index > 0 {
            value.push_str(", ");
        }
        let _ = write!(value, "{reason}: {count}");
    }
    value.push(')');
}

impl CacheMode {
    pub fn reads(self) -> bool {
        self == Self::Default
    }

    pub fn writes(self) -> bool {
        self != Self::Disabled
    }
}

/// One observed renderer dependency. Only a digest is persisted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordedDependency {
    pub kind: String,
    pub digest: String,
}

/// Dependencies and the artifact produced from them by one successful action.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyRecord {
    pub version: u32,
    pub action: String,
    pub dependencies: BTreeMap<String, RecordedDependency>,
    #[serde(rename = "artifactDigest")]
    pub artifact_digest: String,
}

impl DependencyRecord {
    pub fn same_inputs(&self, other: &Self) -> bool {
        self.version == other.version && self.action == other.action && self.dependencies == other.dependencies
    }

    /// Names the inputs that prevent this record from reusing another one.
    pub fn changed_inputs(&self, other: &Self) -> Vec<String> {
        let mut changed = BTreeSet::new();
        if self.version != other.version {
            changed.insert("record version".to_string());
        }
        if self.action != other.action {
            changed.insert("action".to_string());
        }
        for name in self.dependencies.keys().chain(other.dependencies.keys()) {
            if self.dependencies.get(name) != other.dependencies.get(name) {
                changed.insert(name.clone());
            }
        }
        changed.into_iter().collect()
    }
}

/// Collects every input that can affect a cached renderer action.
#[derive(Clone, Debug)]
pub struct DependencyRecorder {
    dependencies: BTreeMap<String, RecordedDependency>,
    secret_key: [u8; 32],
    cacheable: bool,
}

impl DependencyRecorder {
    fn new(secret_key: [u8; 32]) -> Self {
        Self {
            dependencies: BTreeMap::new(),
            secret_key,
            cacheable: true,
        }
    }

    pub fn record_bytes(&mut self, name: impl Into<String>, kind: impl Into<String>, bytes: &[u8]) {
        self.dependencies.insert(
            name.into(),
            RecordedDependency {
                kind: kind.into(),
                digest: sha256(bytes),
            },
        );
    }

    pub fn record_value<T: Serialize>(&mut self, name: impl Into<String>, value: &T) -> Result<()> {
        self.record_bytes(name, "value", &serde_json::to_vec(value)?);
        Ok(())
    }

    pub fn record_file(&mut self, name: impl Into<String>, path: &Path) -> Result<()> {
        self.record_bytes(name, "file", &fs::read(path)?);
        Ok(())
    }

    pub fn record_path_file(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize()?;
        self.record_file(format!("file:{}", path.display()), &path)
    }

    pub fn record_directory(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize()?;
        let mut contents = Vec::new();
        for entry in WalkDir::new(&path).follow_links(false).sort_by_file_name() {
            let entry =
                entry.map_err(|error| NylError::config(format!("Failed to inspect {}: {error}", path.display())))?;
            if entry.file_type().is_symlink() {
                self.mark_uncacheable();
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&path)
                .expect("walk entry is beneath dependency root");
            let relative = crate::resources::relative_path_to_posix("cache directory dependency", relative)?;
            contents.extend_from_slice(&(relative.len() as u64).to_le_bytes());
            contents.extend_from_slice(relative.as_bytes());
            let bytes = fs::read(entry.path())?;
            contents.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            contents.extend_from_slice(&bytes);
        }
        self.record_bytes(format!("directory:{}", path.display()), "directory", &contents);
        Ok(())
    }

    pub fn replay_directories(&mut self, record: &DependencyRecord) -> Result<()> {
        for (name, dependency) in &record.dependencies {
            if dependency.kind == "directory" {
                let path = name
                    .strip_prefix("directory:")
                    .ok_or_else(|| NylError::config(format!("Invalid cached directory dependency name {name:?}")))?;
                if let Err(error) = self.record_directory(Path::new(path)) {
                    tracing::debug!(path, %error, "Recorded cache directory is unavailable; treating it as changed");
                    self.record_bytes(name.clone(), "directory", b"unavailable");
                }
            }
        }
        Ok(())
    }

    pub fn replay_filesystem_dependencies(&mut self, record: &DependencyRecord) -> Result<()> {
        for (name, dependency) in &record.dependencies {
            let path = match dependency.kind.as_str() {
                "file" => name.strip_prefix("file:"),
                "directory" => name.strip_prefix("directory:"),
                _ => continue,
            };
            let Some(path) = path else {
                continue;
            };
            let path = Path::new(path);
            let result = if dependency.kind == "directory" {
                self.record_directory(path)
            } else {
                self.record_path_file(path)
            };
            if let Err(error) = result {
                tracing::debug!(path = %path.display(), %error, "Recorded cache filesystem dependency is unavailable; treating it as changed");
                self.record_bytes(name.clone(), dependency.kind.clone(), b"unavailable");
            }
        }
        Ok(())
    }

    pub fn filesystem_dependencies(&self) -> BTreeMap<String, RecordedDependency> {
        self.dependencies
            .iter()
            .filter(|(_, dependency)| matches!(dependency.kind.as_str(), "file" | "directory"))
            .map(|(name, dependency)| (name.clone(), dependency.clone()))
            .collect()
    }

    pub fn extend_filesystem_dependencies(&mut self, dependencies: &BTreeMap<String, RecordedDependency>) {
        self.dependencies.extend(
            dependencies
                .iter()
                .filter(|(_, dependency)| matches!(dependency.kind.as_str(), "file" | "directory"))
                .map(|(name, dependency)| (name.clone(), dependency.clone())),
        );
    }

    /// Records public template context normally and sensitive context with the cache-local key.
    pub fn record_template_context(&mut self, context: &serde_json::Value) -> Result<()> {
        let mut public = context.clone();
        let object = public
            .as_object_mut()
            .ok_or_else(|| NylError::config("Template context must be an object"))?;
        let secrets = object.remove("secrets").unwrap_or(serde_json::Value::Null);
        let environment = object.remove("env").unwrap_or(serde_json::Value::Null);
        self.record_value("context", &public)?;
        self.record_secret("context:secrets", &serde_json::to_vec(&secrets)?);
        self.record_secret("context:environment", &serde_json::to_vec(&environment)?);
        Ok(())
    }

    /// Records a sensitive value without persisting it or a guessable plain digest.
    pub fn record_secret(&mut self, name: impl Into<String>, value: &[u8]) {
        self.dependencies.insert(
            name.into(),
            RecordedDependency {
                kind: "secret".to_string(),
                digest: hmac_sha256(&self.secret_key, value),
            },
        );
    }

    /// Prevents lookup or publication when a renderer dependency cannot be observed completely.
    pub fn mark_uncacheable(&mut self) {
        self.cacheable = false;
    }

    pub fn is_cacheable(&self) -> bool {
        self.cacheable
    }

    pub fn finish(self, action: impl Into<String>, artifact_digest: String) -> DependencyRecord {
        DependencyRecord {
            version: 1,
            action: action.into(),
            dependencies: self.dependencies,
            artifact_digest,
        }
    }
}

/// Cache storage shared by bundle, Helm, source, and target-tree rendering.
#[derive(Clone, Debug)]
pub struct RenderCache {
    root: PathBuf,
    mode: CacheMode,
    secret_key: [u8; 32],
    renderer_tools: Arc<OnceLock<BTreeMap<String, RecordedDependency>>>,
    source_cache: Option<Arc<tempfile::TempDir>>,
    stats: Arc<Mutex<CacheStats>>,
}

/// Prints the shared cache summary when a command leaves its rendering scope.
pub struct CacheReporter {
    cache: RenderCache,
}

impl Drop for CacheReporter {
    fn drop(&mut self) {
        let stats = self.cache.stats();
        if stats.has_reportable_work() {
            eprintln!("\n{stats}");
        }
    }
}

impl RenderCache {
    pub fn new(project_root: &Path, mode: CacheMode) -> Result<Self> {
        let cache_root =
            std::env::var_os("NYL_CACHE_DIR").map_or_else(|| project_root.join(".nyl/cache"), PathBuf::from);
        Self::with_root(cache_root, mode)
    }

    pub fn with_root(cache_root: impl Into<PathBuf>, mode: CacheMode) -> Result<Self> {
        let root = cache_root.into().join("gitops").join(CACHE_LAYOUT_VERSION);
        let secret_key = if mode == CacheMode::Disabled {
            random_key()?
        } else {
            load_or_create_secret_key(&root)?
        };
        let source_cache = if mode == CacheMode::Disabled {
            Some(Arc::new(tempfile::TempDir::new()?))
        } else {
            None
        };
        Ok(Self {
            root,
            mode,
            secret_key,
            renderer_tools: Arc::new(OnceLock::new()),
            source_cache,
            stats: Arc::new(Mutex::new(CacheStats::default())),
        })
    }

    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Explicit source cache root used to keep --no-cache checkouts ephemeral.
    pub fn external_cache_root(&self) -> Option<&Path> {
        self.source_cache.as_deref().map(tempfile::TempDir::path)
    }

    pub fn recorder(&self) -> DependencyRecorder {
        DependencyRecorder::new(self.secret_key)
    }

    pub fn observe(&self, layer: CacheLayer, outcome: CacheOutcome, reasons: &[String]) {
        tracing::debug!(?layer, ?outcome, ?reasons, "Rendering cache event");
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(layer, outcome, reasons);
    }

    pub fn observe_source(&self, operation: SourceOperation) {
        tracing::debug!(?operation, "Rendering source operation");
        let mut stats = self.stats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *stats.sources.entry(operation).or_default() += 1;
    }

    pub fn observe_target_hit(&self, releases: usize, helm_renders: usize) {
        tracing::debug!(
            layer = ?CacheLayer::Target,
            outcome = ?CacheOutcome::Hit,
            releases,
            helm_renders,
            "Rendering cache event"
        );
        let mut stats = self.stats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.observe(CacheLayer::Target, CacheOutcome::Hit, &[]);
        stats.target_reuse = Some(TargetReuse { releases, helm_renders });
    }

    pub fn observe_release_hit(&self, helm_renders: usize) {
        tracing::debug!(
            layer = ?CacheLayer::Release,
            outcome = ?CacheOutcome::Hit,
            helm_renders,
            "Rendering cache event"
        );
        let mut stats = self.stats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.observe(CacheLayer::Release, CacheOutcome::Hit, &[]);
        stats.release_helm_renders_avoided += helm_renders;
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn reporter(&self) -> CacheReporter {
        CacheReporter { cache: self.clone() }
    }

    pub fn record_renderer_tools(&self, recorder: &mut DependencyRecorder) -> Result<()> {
        if self.renderer_tools.get().is_none() {
            let dependencies = renderer_tool_dependencies()?;
            let _ = self.renderer_tools.set(dependencies);
        }
        recorder.dependencies.extend(
            self.renderer_tools
                .get()
                .expect("renderer tool dependencies were initialized")
                .clone(),
        );
        Ok(())
    }

    pub fn load_artifact<T: DeserializeOwned>(&self, kind: &str, digest: &str) -> Result<Option<T>> {
        if !self.mode.reads() {
            return Ok(None);
        }
        let path = self.artifact_path(kind, digest)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if sha256(&bytes) != digest {
            tracing::warn!(path = %path.display(), "Ignoring corrupt GitOps cache artifact");
            return Ok(None);
        }
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Ignoring invalid GitOps cache artifact");
                Ok(None)
            }
        }
    }

    pub fn store_artifact<T: Serialize>(&self, kind: &str, value: &T) -> Result<Option<String>> {
        if !self.mode.writes() {
            return Ok(None);
        }
        let bytes = serde_json::to_vec(value)?;
        let digest = sha256(&bytes);
        let path = self.artifact_path(kind, &digest)?;
        atomic_write_once(&path, &bytes)?;
        Ok(Some(digest))
    }

    pub fn load_record(&self, action: &str, key: &str) -> Result<Option<DependencyRecord>> {
        if !self.mode.reads() {
            return Ok(None);
        }
        let path = self.record_path(action, key)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(record) => Ok(Some(record)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Ignoring invalid GitOps cache dependency record");
                Ok(None)
            }
        }
    }

    pub fn store_record(&self, action: &str, key: &str, record: &DependencyRecord) -> Result<()> {
        if !self.mode.writes() {
            return Ok(());
        }
        let path = self.record_path(action, key)?;
        atomic_replace(&path, &serde_json::to_vec(record)?)
    }

    fn artifact_path(&self, kind: &str, digest: &str) -> Result<PathBuf> {
        validate_segment("artifact kind", kind)?;
        validate_digest(digest)?;
        Ok(self
            .root
            .join("artifacts")
            .join(kind)
            .join(&digest[..2])
            .join(format!("{digest}.json")))
    }

    fn record_path(&self, action: &str, key: &str) -> Result<PathBuf> {
        validate_segment("cache action", action)?;
        Ok(self
            .root
            .join("records")
            .join(action)
            .join(format!("{}.json", sha256(key.as_bytes()))))
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn renderer_tool_dependencies() -> Result<BTreeMap<String, RecordedDependency>> {
    let mut dependencies = BTreeMap::new();
    let executable = std::env::current_exe()?;
    dependencies.insert(
        "tool:nyl".to_string(),
        RecordedDependency {
            kind: "file".to_string(),
            digest: sha256(&fs::read(executable)?),
        },
    );
    for (tool, arguments) in [("helm", &["version", "--short"][..]), ("kyverno", &["version"][..])] {
        dependencies.insert(format!("tool:{tool}"), tool_dependency(tool, arguments));
    }
    Ok(dependencies)
}

fn tool_dependency(tool: &str, arguments: &[&str]) -> RecordedDependency {
    let Some(path) = find_executable(tool) else {
        return RecordedDependency {
            kind: "tool".to_string(),
            digest: sha256(b"unavailable"),
        };
    };
    let output = std::process::Command::new(&path).args(arguments).output();
    let mut fingerprint = path.as_os_str().as_encoded_bytes().to_vec();
    match output {
        Ok(output) => {
            fingerprint.extend_from_slice(&output.stdout);
            fingerprint.extend_from_slice(&output.stderr);
            fingerprint.extend_from_slice(&output.status.code().unwrap_or(-1).to_le_bytes());
        }
        Err(error) => fingerprint.extend_from_slice(error.to_string().as_bytes()),
    }
    RecordedDependency {
        kind: "tool".to_string(),
        digest: sha256(&fingerprint),
    }
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(NylError::config(format!("Invalid {label} {value:?}")));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NylError::config(format!("Invalid cache artifact digest {value:?}")));
    }
    Ok(())
}

fn load_or_create_secret_key(root: &Path) -> Result<[u8; 32]> {
    let path = root.join(SECRET_KEY_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            return bytes
                .try_into()
                .map_err(|_| NylError::config(format!("Invalid cache key {}", path.display())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let key = random_key()?;
    atomic_write_once(&path, &key)?;
    let bytes = fs::read(&path)?;
    bytes
        .try_into()
        .map_err(|_| NylError::config(format!("Invalid cache key {}", path.display())))
}

fn random_key() -> Result<[u8; 32]> {
    let mut key = [0_u8; 32];
    fill_random(&mut key).map_err(|error| NylError::config(format!("Failed to generate cache key: {error}")))?;
    Ok(key)
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8; 32], value: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut inner_key = [0x36_u8; BLOCK_SIZE];
    let mut outer_key = [0x5c_u8; BLOCK_SIZE];
    for (index, byte) in key.iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(value);
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner.finalize());
    hex::encode(outer.finalize())
}

fn atomic_write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("cache file has parent");
    create_private_dir_all(parent)?;
    if path.is_file() {
        if fs::read(path)? == bytes {
            return Ok(());
        }
        return atomic_replace(path, bytes);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, bytes)?;
    set_private_file_permissions(temporary.path())?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("cache file has parent");
    create_private_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, bytes)?;
    set_private_file_permissions(temporary.path())?;
    temporary.persist(path).map(|_| ()).map_err(|error| error.error.into())
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_corruption_is_a_safe_cache_miss() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();
        let digest = cache.store_artifact("target", &vec!["manifest"]).unwrap().unwrap();
        fs::write(cache.artifact_path("target", &digest).unwrap(), b"corrupt").unwrap();

        assert_eq!(cache.load_artifact::<Vec<String>>("target", &digest).unwrap(), None);
    }

    #[test]
    fn disabled_cache_neither_reads_nor_writes() {
        let temp = tempfile::TempDir::new().unwrap();
        let persistent = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();
        let digest = persistent.store_artifact("target", &vec!["manifest"]).unwrap().unwrap();
        let disabled = RenderCache::with_root(temp.path(), CacheMode::Disabled).unwrap();

        assert_eq!(disabled.load_artifact::<Vec<String>>("target", &digest).unwrap(), None);
        assert_eq!(disabled.store_artifact("target", &vec!["other"]).unwrap(), None);
        let source_cache = disabled.external_cache_root().unwrap();
        assert!(source_cache.is_dir());
        assert!(!source_cache.starts_with(temp.path()));
    }

    #[test]
    fn secret_dependencies_use_a_cache_local_key() {
        let first_root = tempfile::TempDir::new().unwrap();
        let second_root = tempfile::TempDir::new().unwrap();
        let first = RenderCache::with_root(first_root.path(), CacheMode::Default).unwrap();
        let second = RenderCache::with_root(second_root.path(), CacheMode::Default).unwrap();
        let mut first_recording = first.recorder();
        let mut second_recording = second.recorder();
        first_recording.record_secret("token", b"shared secret");
        second_recording.record_secret("token", b"shared secret");

        assert_ne!(
            first_recording.dependencies["token"].digest,
            second_recording.dependencies["token"].digest
        );
    }

    #[test]
    fn shared_statistics_explain_cache_effectiveness() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();
        let clone = cache.clone();

        cache.observe(CacheLayer::Target, CacheOutcome::Miss, &[]);
        cache.observe(
            CacheLayer::Target,
            CacheOutcome::Bypassed,
            &["remote Helm chart".to_string(), "RemoteManifest".to_string()],
        );
        clone.observe(CacheLayer::Release, CacheOutcome::Hit, &[]);
        clone.observe(CacheLayer::Release, CacheOutcome::Hit, &[]);
        clone.observe(
            CacheLayer::Helm,
            CacheOutcome::Bypassed,
            &["remote Helm chart".to_string()],
        );

        assert_eq!(
            cache.stats().to_string(),
            "Render statistics\n  Cache\n    Target tree         rebuilt (not cacheable: RemoteManifest: 1, remote Helm chart: 1)\n    \
             Release renders     2 reused\n    Helm renders        1 rendered (not cacheable: remote Helm chart: 1)"
        );
    }

    #[test]
    fn target_hit_explains_short_circuited_rendering_work() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();

        cache.observe_target_hit(38, 65);

        assert_eq!(
            cache.stats().to_string(),
            "Render statistics\n  Cache\n    Target tree         reused\n    Release renders     38 avoided\n    Helm renders        65 avoided"
        );
    }

    #[test]
    fn release_reuse_reports_its_avoided_helm_work() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();

        for helm_renders in [2, 3, 0] {
            cache.observe_release_hit(helm_renders);
        }
        cache.observe(CacheLayer::Release, CacheOutcome::Invalidated, &[]);
        cache.observe(CacheLayer::Release, CacheOutcome::Stored, &[]);

        assert_eq!(
            cache.stats().to_string(),
            "Render statistics\n  Cache\n    Release renders     3 reused · 1 rendered\n    Helm renders        5 avoided"
        );
    }

    #[test]
    fn test_source_statistics_name_the_operations_that_were_performed() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = RenderCache::with_root(temp.path(), CacheMode::Default).unwrap();

        for operation in [
            SourceOperation::RemoteManifestDownload,
            SourceOperation::HelmChartPull,
            SourceOperation::GitRepositoryReuse,
            SourceOperation::GitRepositoryReuse,
            SourceOperation::GitRefRefresh,
            SourceOperation::GitWorktreeReuse,
        ] {
            cache.observe_source(operation);
        }

        assert_eq!(
            cache.stats().to_string(),
            "Render statistics\n  Sources\n    Remote manifests    1 downloaded\n    Helm charts         1 pulled\n    Git repositories    2 reused · 1 ref refreshed\n    Git worktrees       1 reused"
        );
    }

    #[test]
    fn dependency_records_name_changed_added_and_removed_inputs() {
        let dependency = |digest: &str| RecordedDependency {
            kind: "value".to_string(),
            digest: digest.to_string(),
        };
        let previous = DependencyRecord {
            version: 1,
            action: "render".to_string(),
            dependencies: BTreeMap::from([
                ("changed".to_string(), dependency("old")),
                ("removed".to_string(), dependency("same")),
                ("stable".to_string(), dependency("same")),
            ]),
            artifact_digest: "old-artifact".to_string(),
        };
        let current = DependencyRecord {
            version: 1,
            action: "render".to_string(),
            dependencies: BTreeMap::from([
                ("added".to_string(), dependency("new")),
                ("changed".to_string(), dependency("new")),
                ("stable".to_string(), dependency("same")),
            ]),
            artifact_digest: "new-artifact".to_string(),
        };

        assert_eq!(previous.changed_inputs(&current), ["added", "changed", "removed"]);
    }
}
