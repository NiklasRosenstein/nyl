use schemars::schema_for;

use super::ProjectFile;

/// Generate JSON Schema for `nyl.toml`.
pub fn generate_project_config_schema() -> serde_json::Value {
    let schema = schema_for!(ProjectFile);
    serde_json::to_value(schema).expect("schema serialization should never fail")
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
    fn test_schema_matches_published_docs_artifact() {
        let schema = generate_project_config_schema();
        let schema_text = serde_json::to_string_pretty(&schema).unwrap();

        let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("book")
            .join("src")
            .join("reference")
            .join("schemas")
            .join("nyl.schema.json");
        let published = fs::read_to_string(&schema_path).expect("published schema file must exist");

        assert_eq!(
            schema_text.trim(),
            published.trim(),
            "Published schema is out of date. Regenerate with: cargo run -- generate schema config > book/src/reference/schemas/nyl.schema.json"
        );
    }
}
