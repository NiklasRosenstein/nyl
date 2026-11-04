//! Shared utilities: file system and project path helpers, source context,
//! and small formatting helpers. Content digests live in `nyl_core::digest`.

pub mod deprecation;
pub mod fs;
pub mod project_path;
pub mod source_context;

pub use fs::{find_config_file, path_for_display, resolve_path, resolve_paths};
pub use source_context::{parse_resource, SourceContext};

/// Style one value without changing process-wide color policy.
pub fn ansi_style(value: impl std::fmt::Display, code: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}

/// Recursively merge JSON objects, replacing arrays and scalar values.
pub fn deep_merge_value(base: Option<serde_json::Value>, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (Some(serde_json::Value::Object(mut base)), serde_json::Value::Object(overlay)) => {
            for (key, overlay_value) in overlay {
                let base_value = base.remove(&key);
                base.insert(key, deep_merge_value(base_value, overlay_value));
            }
            serde_json::Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

/// Execute a command and return output
pub fn execute_command(_cmd: &str, _args: &[&str]) -> crate::Result<String> {
    // Stub implementation
    Ok(String::new())
}

/// A repository URL without userinfo, for provenance keys.
///
/// A parseable URL is always re-serialized, so a key does not depend on
/// whether credentials are embedded in the configured URL.
pub fn credential_free_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Err(_) => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_merge_recurses_through_objects_and_replaces_other_values() {
        let base = serde_json::json!({
            "nested": {"cluster": true, "shared": "cluster"},
            "array": ["cluster"]
        });
        let overlay = serde_json::json!({
            "nested": {"target": true, "shared": "target"},
            "array": ["target"]
        });
        assert_eq!(
            deep_merge_value(Some(base), overlay),
            serde_json::json!({
                "nested": {"cluster": true, "target": true, "shared": "target"},
                "array": ["target"]
            })
        );
    }
}
