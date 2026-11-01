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
pub fn published_schema_artifacts(
    descriptors: &[nyl_core::resources::schema::ResourceDescriptor],
) -> BTreeMap<String, serde_json::Value> {
    let mut artifacts = nyl_core::resources::schema::schema_artifacts(descriptors);
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
    fn test_project_schema_matches_published_artifact() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../nyl/book/public/reference/schemas")
            .join(PROJECT_CONFIG_SCHEMA_PATH);
        let published = fs::read_to_string(path).expect("published project schema must exist");
        let published: serde_json::Value =
            serde_json::from_str(&published).expect("published schema must be valid JSON");
        assert_eq!(
            published,
            generate_project_config_schema(),
            "Regenerate with nyl schema all"
        );
    }
}
