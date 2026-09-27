use std::collections::BTreeMap;

use schemars::schema_for;

use super::ProjectFile;

/// Path of the `nyl.toml` schema within the published schema directory.
pub const PROJECT_CONFIG_SCHEMA_PATH: &str = "nyl.schema.json";

/// Generate JSON Schema for `nyl.toml`.
pub fn generate_project_config_schema() -> serde_json::Value {
    let schema = schema_for!(ProjectFile);
    serde_json::to_value(schema).expect("schema serialization should never fail")
}

/// The complete published schema set: every resource artifact from
/// [`nyl_core::resources::schema::schema_artifacts`] plus the `nyl.toml`
/// schema, keyed by path within the published schema directory.
pub fn published_schema_artifacts() -> BTreeMap<String, serde_json::Value> {
    let mut artifacts = nyl_core::resources::schema::schema_artifacts();
    artifacts.insert(PROJECT_CONFIG_SCHEMA_PATH.into(), generate_project_config_schema());
    artifacts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_schema_has_project_section() {
        let schema = generate_project_config_schema();
        let properties = schema.get("properties").unwrap().as_object().unwrap();
        assert!(properties.contains_key("project"));
    }

    #[test]
    fn test_published_schema_set_matches_docs_artifacts() {
        let schema_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../nyl/book")
            .join("public")
            .join("reference")
            .join("schemas");
        let artifacts = published_schema_artifacts();
        assert!(artifacts.contains_key(PROJECT_CONFIG_SCHEMA_PATH));
        for (path, expected) in artifacts {
            let published = fs::read_to_string(schema_directory.join(&path))
                .unwrap_or_else(|error| panic!("published schema {path} must exist: {error}"));
            let published: serde_json::Value =
                serde_json::from_str(&published).expect("published schema file must be valid JSON");
            assert_eq!(
                published, expected,
                "Published schema {path} is out of date. Regenerate with: nyl schema all --output-dir nyl/book/public/reference/schemas"
            );
        }
    }
}
