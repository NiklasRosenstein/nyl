//! Warnings for deprecated API versions found while loading project files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use nyl_core::resources::schema::deprecated_api_version;

/// Where the documentation lists deprecations and when their support ends.
pub const DEPRECATIONS_URL: &str = "https://niklasrosenstein.github.io/nyl/reference/deprecations/";

/// The warning for one document under a deprecated API version.
pub fn deprecated_api_version_warning(path: &Path, document: &serde_json::Value) -> Option<String> {
    let deprecated = deprecated_api_version(document)?;
    let name = document
        .pointer("/metadata/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unnamed>");
    Some(format!(
        "{}: {} {name:?} uses deprecated apiVersion {}; set apiVersion to {}. Support for {} ends in a future release, see {DEPRECATIONS_URL}",
        path.display(),
        deprecated.kind,
        deprecated.api_version,
        deprecated.replacement,
        deprecated.api_version,
    ))
}

/// Warn once per process for each deprecated document, however often it is loaded.
pub fn warn_deprecated_api_version(path: &Path, document: &serde_json::Value) {
    static WARNED: OnceLock<Mutex<BTreeSet<(PathBuf, String)>>> = OnceLock::new();
    let Some(warning) = deprecated_api_version_warning(path, document) else {
        return;
    };
    let key = (path.to_path_buf(), warning.clone());
    let first = WARNED
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .map_or(true, |mut warned| warned.insert(key));
    if first {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deprecated_api_version_warning_names_the_replacement() {
        let cluster = json!({"apiVersion": "k8s.gitops.nyl/v1", "kind": "Cluster", "metadata": {"name": "primary"}});
        let warning = deprecated_api_version_warning(Path::new("config/cluster.yaml"), &cluster).unwrap();
        assert!(
            warning.contains("config/cluster.yaml: Cluster \"primary\""),
            "{warning}"
        );
        assert!(warning.contains("set apiVersion to k8s.nyl/v1"), "{warning}");
        let current = json!({"apiVersion": "k8s.nyl/v1", "kind": "Cluster", "metadata": {"name": "primary"}});
        assert!(deprecated_api_version_warning(Path::new("x"), &current).is_none());
    }
}
