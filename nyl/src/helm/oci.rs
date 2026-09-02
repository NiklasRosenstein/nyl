/// OCI registry chart pulling via `helm pull`
///
/// Downloads original Helm chart archives and stores immutable content by
/// digest. The shared artifact resolver controls source-cache and vendor
/// freshness.
///
/// # Cache Layout
///
/// ```text
/// $NYL_CACHE_DIR/helm/oci/
/// └── {repo_hash}-{version}-{content_hash}.tgz
/// ```
use crate::{NylError, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pulls Helm charts from OCI registries and caches them locally
pub struct OciChartPuller {
    cache_dir: PathBuf,
    render_cache: Option<crate::render::cache::RenderCache>,
    artifact_resolver: Option<crate::render::artifact::ArtifactResolver>,
}

impl OciChartPuller {
    /// Create a new OCI chart puller
    ///
    /// Uses `NYL_CACHE_DIR` if set, otherwise falls back to `.nyl/cache/` in the
    /// current directory.
    pub fn new() -> Result<Self> {
        let root = if let Ok(cache_dir) = std::env::var("NYL_CACHE_DIR") {
            PathBuf::from(cache_dir)
        } else {
            std::env::current_dir()?.join(".nyl").join("cache")
        };

        Ok(Self {
            cache_dir: root.join("helm").join("oci"),
            render_cache: None,
            artifact_resolver: None,
        })
    }

    /// Create a puller with an explicit cache directory (useful for testing)
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into().join("helm").join("oci"),
            render_cache: None,
            artifact_resolver: None,
        }
    }

    #[must_use]
    pub fn with_render_cache(mut self, cache: Option<crate::render::cache::RenderCache>) -> Self {
        self.render_cache = cache;
        self
    }

    #[must_use]
    pub fn with_artifact_resolver(mut self, resolver: Option<crate::render::artifact::ArtifactResolver>) -> Self {
        self.artifact_resolver = resolver;
        self
    }

    /// Pull a chart from an OCI or traditional Helm repository
    ///
    /// # Arguments
    /// * `repository` - OCI URL (`oci://ghcr.io/owner/repo/chart`) or traditional
    ///   repo index URL (`https://example.com/charts`)
    /// * `version` - Chart version like `0.1.0` or `0.1.0-sha-abc1234`
    /// * `chart_name` - Chart name within the repository. Required for traditional
    ///   (non-OCI) repos; ignored for OCI where the name is the last URL path segment.
    ///
    /// # Returns
    /// Path to the packaged chart archive
    pub fn pull(&self, repository: &str, version: &str, chart_name: Option<&str>) -> Result<PathBuf> {
        let is_oci = repository.starts_with("oci://");
        let effective_name = if is_oci {
            extract_chart_name(repository)
        } else {
            chart_name
                .ok_or_else(|| NylError::Config("Chart name is required for non-OCI Helm repositories".to_string()))?
                .to_owned()
        };
        let request = crate::render::artifact::ArtifactRequest::HelmChart {
            repository: repository.to_owned(),
            name: effective_name.clone(),
            version: version.to_owned(),
        };
        if let Some(path) = self.resolve_existing(repository, version, &effective_name, &request)? {
            return Ok(path);
        }
        self.pull_remote(repository, version, &effective_name, is_oci, &request)
    }

    fn resolve_existing(
        &self,
        repository: &str,
        version: &str,
        chart_name: &str,
        request: &crate::render::artifact::ArtifactRequest,
    ) -> Result<Option<PathBuf>> {
        if let Some(resolver) = &self.artifact_resolver {
            if let Some(artifact) = resolver.lookup(request)? {
                return Ok(Some(artifact.path));
            }
            if let Some(legacy) = self.find_legacy_chart(repository, version, chart_name)? {
                return self.import_legacy_chart(&legacy, version, request).map(Some);
            }
        }
        Ok(None)
    }

    fn import_legacy_chart(
        &self,
        legacy: &Path,
        version: &str,
        request: &crate::render::artifact::ArtifactRequest,
    ) -> Result<PathBuf> {
        tracing::info!(
            chart = %request.display(),
            path = %legacy.display(),
            "Importing legacy extracted Helm chart cache"
        );
        let package_dir = tempfile::tempdir()?;
        let output = Command::new("helm")
            .arg("package")
            .arg(legacy)
            .arg("--destination")
            .arg(package_dir.path())
            .output()
            .map_err(|error| NylError::Process(format!("Failed to execute helm package: {error}")))?;
        if !output.status.success() {
            return Err(NylError::HelmChart(format!(
                "Failed to package legacy cached chart {}: {}",
                request.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let archive = std::fs::read_dir(package_dir.path())?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("tgz"))
            .ok_or_else(|| NylError::HelmChart("helm package produced no .tgz archive".to_owned()))?;
        let resolver = self
            .artifact_resolver
            .as_ref()
            .expect("legacy import is used only with the exact artifact resolver");
        Ok(resolver
            .store(
                request,
                &archive,
                crate::render::artifact::ArtifactFormat::HelmArchive,
                Some(version.to_owned()),
            )?
            .path)
    }

    fn pull_remote(
        &self,
        repository: &str,
        version: &str,
        chart_name: &str,
        is_oci: bool,
        request: &crate::render::artifact::ArtifactRequest,
    ) -> Result<PathBuf> {
        let coordinate_path = self.chart_cache_path(repository, chart_name, version);
        tracing::debug!("Pulling Helm chart from {}", crate::util::sanitize_url(repository));
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| NylError::Process(format!("Failed to create chart cache directory: {}", e)))?;
        let tmp_dir = tempfile::Builder::new()
            .prefix(".pull-tmp-")
            .tempdir_in(&self.cache_dir)
            .map_err(|e| NylError::Process(format!("Failed to create temp pull directory: {}", e)))?;
        let mut cmd = Command::new("helm");
        cmd.arg("pull");
        if is_oci {
            cmd.arg(repository);
        } else {
            cmd.arg("--repo").arg(repository).arg(chart_name);
        }
        cmd.arg("--version").arg(version).arg("-d").arg(tmp_dir.path());
        tracing::debug!("Executing helm command: {:?}", cmd);
        let output = cmd
            .output()
            .map_err(|e| NylError::Process(format!("Failed to execute helm pull: {}", e)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NylError::HelmChart(format!(
                "helm pull failed for {}: {}",
                request.display(),
                stderr
            )));
        }
        if let Some(cache) = &self.render_cache {
            cache.observe_source(crate::render::cache::SourceOperation::HelmChartPull);
        }
        tracing::debug!("Helm chart pulled successfully");
        let archives = std::fs::read_dir(tmp_dir.path())?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("tgz"))
            .collect::<Vec<_>>();
        let [archive] = archives.as_slice() else {
            return Err(NylError::HelmChart(format!(
                "helm pull for {} produced {} chart archives; expected exactly one",
                request.display(),
                archives.len()
            )));
        };
        if let Some(resolver) = &self.artifact_resolver {
            return Ok(resolver
                .store(
                    request,
                    archive,
                    crate::render::artifact::ArtifactFormat::HelmArchive,
                    Some(version.to_owned()),
                )?
                .path);
        }
        Self::install_archive(archive, &coordinate_path)
    }

    fn install_archive(archive: &Path, coordinate_path: &Path) -> Result<PathBuf> {
        let digest = file_digest(archive)?;
        let coordinate_name = coordinate_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("chart");
        let chart_dir = coordinate_path.with_file_name(format!("{coordinate_name}-{}.tgz", &digest[..16]));
        if chart_dir.is_file() {
            tracing::debug!("Reusing identical Helm chart content: {}", chart_dir.display());
            return Ok(chart_dir);
        }

        match std::fs::rename(archive, &chart_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !chart_dir.is_file() {
                    return Err(NylError::Process(format!("Failed to move cached chart: {}", e)));
                }
            }
            Err(e) => {
                return Err(NylError::Process(format!("Failed to move cached chart: {}", e)));
            }
        }
        Ok(chart_dir)
    }

    /// Compute the cache path for a given repository and version
    fn chart_cache_path(&self, repository: &str, chart_name: &str, version: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(repository.as_bytes());
        hasher.update([0]);
        hasher.update(chart_name.as_bytes());
        let repo_hash = hex::encode(hasher.finalize());

        let safe_version = Self::sanitize_version(version);
        self.cache_dir.join(format!("{}-{}", &repo_hash[..16], safe_version))
    }

    fn find_legacy_chart(&self, repository: &str, version: &str, chart_name: &str) -> Result<Option<PathBuf>> {
        let mut hasher = Sha256::new();
        hasher.update(repository.as_bytes());
        let prefix = format!(
            "{}-{}-",
            &hex::encode(hasher.finalize())[..16],
            Self::sanitize_version(version)
        );
        let entries = match std::fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix))
            })
            .filter(|path| {
                std::fs::read_to_string(path.join("Chart.yaml"))
                    .ok()
                    .and_then(|contents| serde_norway::from_str::<serde_json::Value>(&contents).ok())
                    .and_then(|chart| chart.get("name").and_then(serde_json::Value::as_str).map(str::to_owned))
                    .as_deref()
                    == Some(chart_name)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
        Ok(candidates.pop())
    }

    /// Sanitize a chart version string for safe use as a filesystem path component.
    ///
    /// This restricts the version to a safe subset of characters and replaces any
    /// disallowed character with an underscore, preventing path traversal via
    /// separators or `..`.
    fn sanitize_version(version: &str) -> String {
        let sanitized: String = version
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        if sanitized.is_empty() {
            "unknown".to_string()
        } else {
            sanitized
        }
    }
}

fn file_digest(path: &std::path::Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path)?);
    Ok(hex::encode(hasher.finalize()))
}

/// Extract the chart name from an OCI repository URL
///
/// The chart name is the last path segment of the OCI URL.
/// e.g. `oci://ghcr.io/owner/repo/chart` → `chart`
fn extract_chart_name(repository: &str) -> String {
    repository
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("chart")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_chart_name() {
        assert_eq!(extract_chart_name("oci://ghcr.io/owner/repo/mychart"), "mychart");
        assert_eq!(extract_chart_name("oci://ghcr.io/niklasrosenstein/nyl/chart"), "chart");
        assert_eq!(extract_chart_name("oci://registry.example.com/charts/nginx"), "nginx");
    }

    #[test]
    fn test_extract_chart_name_trailing_slash() {
        assert_eq!(extract_chart_name("oci://ghcr.io/owner/repo/mychart/"), "mychart");
    }

    #[test]
    fn test_chart_cache_path_deterministic() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "chart", "0.1.0-sha-abc1234");
        assert_eq!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_versions() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "chart", "0.1.0-sha-def5678");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_repos() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner1/nyl/chart", "chart", "0.1.0");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner2/nyl/chart", "chart", "0.1.0");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_chart_names() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());
        assert_ne!(
            puller.chart_cache_path("https://charts.example.com", "one", "1.0.0"),
            puller.chart_cache_path("https://charts.example.com", "two", "1.0.0")
        );
    }

    #[test]
    fn test_sanitize_version_safe() {
        assert_eq!(OciChartPuller::sanitize_version("1.0.0"), "1.0.0");
        assert_eq!(OciChartPuller::sanitize_version("1.0.0-alpha"), "1.0.0-alpha");
        assert_eq!(OciChartPuller::sanitize_version("1.0.0_beta"), "1.0.0_beta");
    }

    #[test]
    fn test_sanitize_version_path_traversal() {
        // Path separators should be sanitized
        assert_eq!(
            OciChartPuller::sanitize_version("../../../etc/passwd"),
            ".._.._.._etc_passwd"
        );
        assert_eq!(OciChartPuller::sanitize_version("1.0/../../bad"), "1.0_.._.._bad");
        assert_eq!(OciChartPuller::sanitize_version(".."), "..");
    }

    #[test]
    fn test_sanitize_version_special_chars() {
        // Special characters should be replaced with underscores
        assert_eq!(OciChartPuller::sanitize_version("1.0.0+build"), "1.0.0_build");
        assert_eq!(OciChartPuller::sanitize_version("v1.0.0!@#$"), "v1.0.0____");
    }

    #[test]
    fn test_sanitize_version_empty() {
        // Empty version should return "unknown"
        assert_eq!(OciChartPuller::sanitize_version(""), "unknown");
    }

    #[test]
    fn test_sanitize_version_only_special_chars() {
        // Version with only special characters gets all replaced with underscores
        assert_eq!(OciChartPuller::sanitize_version("!@#$%"), "_____");
    }
}
