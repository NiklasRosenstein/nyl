/// OCI registry chart pulling via `helm pull`
///
/// Downloads and caches Helm charts from OCI registries (e.g. ghcr.io).
/// Charts are cached locally to avoid redundant pulls.
///
/// # Cache Layout
///
/// ```text
/// $NYL_CACHE_DIR/helm/oci/
/// └── {repo_hash}-{version}/  # Extracted chart directory
/// ```
use crate::{NylError, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

/// Pulls Helm charts from OCI registries and caches them locally
pub struct OciChartPuller {
    cache_dir: PathBuf,
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
        })
    }

    /// Create a puller with an explicit cache directory (useful for testing)
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into().join("helm").join("oci"),
        }
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
    /// Path to the extracted chart directory
    pub fn pull(&self, repository: &str, version: &str, chart_name: Option<&str>) -> Result<PathBuf> {
        let chart_dir = self.chart_cache_path(repository, version);

        // Return cached chart if it exists and contains Chart.yaml
        if chart_dir.join("Chart.yaml").exists() {
            return Ok(chart_dir);
        }

        // Ensure cache directory exists
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| NylError::Process(format!("Failed to create chart cache directory: {}", e)))?;

        // Pull into a unique temp directory to avoid collisions with concurrent pulls
        let tmp_dir = tempfile::Builder::new()
            .prefix(".pull-tmp-")
            .tempdir_in(&self.cache_dir)
            .map_err(|e| NylError::Process(format!("Failed to create temp pull directory: {}", e)))?;

        let is_oci = repository.starts_with("oci://");

        let mut cmd = Command::new("helm");
        cmd.arg("pull");

        if is_oci {
            // OCI:         helm pull oci://registry/path/chart --version <ver>
            cmd.arg(repository);
        } else {
            // Traditional: helm pull --repo <repo-url> <chart-name> --version <ver>
            let name = chart_name
                .ok_or_else(|| NylError::Config("Chart name is required for non-OCI Helm repositories".to_string()))?;
            cmd.arg("--repo").arg(repository).arg(name);
        }

        cmd.arg("--version").arg(version).arg("--untar").arg("-d").arg(tmp_dir.path());

        let output = cmd
            .output()
            .map_err(|e| NylError::Process(format!("Failed to execute helm pull: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NylError::HelmChart(format!(
                "helm pull failed for {}@{}: {}",
                repository, version, stderr
            )));
        }

        // helm pull --untar extracts to <tmp_dir>/<chart-name>/
        let extracted_name = if is_oci {
            extract_chart_name(repository)
        } else {
            chart_name.unwrap_or("chart").to_string()
        };
        let extracted = tmp_dir.path().join(&extracted_name);

        if !extracted.join("Chart.yaml").exists() {
            return Err(NylError::HelmChart(format!(
                "Chart.yaml not found after pulling {}@{} (expected at {})",
                repository,
                version,
                extracted.display()
            )));
        }

        // Move to the versioned cache path
        match std::fs::rename(&extracted, &chart_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another concurrent pull populated the cache between our existence
                // check and this rename. Treat this as a cache hit, but verify the
                // cache looks valid before proceeding.
                if !chart_dir.join("Chart.yaml").exists() {
                    return Err(NylError::Process(format!("Failed to move cached chart: {}", e)));
                }
            }
            Err(e) => {
                return Err(NylError::Process(format!("Failed to move cached chart: {}", e)));
            }
        }

        // tmp_dir will be automatically cleaned up when it goes out of scope

        Ok(chart_dir)
    }

    /// Compute the cache path for a given repository and version
    fn chart_cache_path(&self, repository: &str, version: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(repository.as_bytes());
        let repo_hash = hex::encode(hasher.finalize());

        let safe_version = Self::sanitize_version(version);
        self.cache_dir.join(format!("{}-{}", &repo_hash[..16], safe_version))
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

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        assert_eq!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_versions() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-def5678");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_repos() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner1/nyl/chart", "0.1.0");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner2/nyl/chart", "0.1.0");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_pull_returns_cached_chart() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let repo = "oci://ghcr.io/owner/nyl/chart";
        let version = "0.1.0";

        // Pre-populate the cache
        let cache_path = puller.chart_cache_path(repo, version);
        std::fs::create_dir_all(&cache_path).unwrap();
        std::fs::write(
            cache_path.join("Chart.yaml"),
            "apiVersion: v2\nname: chart\nversion: 0.1.0\n",
        )
        .unwrap();

        // Pull should return the cached path without running helm
        let result = puller.pull(repo, version, None).unwrap();
        assert_eq!(result, cache_path);
        assert!(result.join("Chart.yaml").exists());
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
        assert_eq!(OciChartPuller::sanitize_version("../../../etc/passwd"), ".._.._.._etc_passwd");
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
        assert_eq!(OciChartPuller::sanitize_version("!@#$%"), "_____");
    }
}
