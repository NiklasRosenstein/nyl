//! Finding references without resolving them.
//!
//! Contract: [References](../../../../design/orchestration-core.md#references):
//! "References add edges implicitly", and the edge is known after rendering.
//! The dependency graph needs every `$unit` reference of a rendered or stored
//! spec before any producer has a value, so this walk shares the walker of
//! [`super::resolve_document`] ([`super::find_clauses`]) but reads no
//! provider.

use serde_json::Value;

use super::{find_clauses, Clause, Context, UnitInputSource};

/// One `$unit` reference of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReference {
    /// JSON Pointer of the reference object, below the pointer the caller
    /// passes, such as `/variables/vpc_id`.
    pub pointer: String,
    pub source: UnitInputSource,
}

/// Every `$unit` reference in a rendered unit spec `document`, at any depth
/// of its objects and arrays, with `pointer` locating `document` itself. A
/// malformed or misplaced source (unknown fields, both `output` and
/// `artifact`) is an issue naming its location, never a dropped reference.
pub fn unit_references(pointer: &str, document: &Value) -> Result<Vec<UnitReference>, Vec<String>> {
    Ok(find_clauses("spec", pointer, document, Context::UnitExecution.into())?
        .into_iter()
        .filter_map(|found| match found.clause {
            Clause::Unit(source) => Some(UnitReference {
                pointer: found.pointer,
                source,
            }),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source(unit: &str, output: Option<&str>, artifact: Option<&str>) -> UnitInputSource {
        UnitInputSource {
            environment: None,
            unit: unit.to_owned(),
            output: output.map(str::to_owned),
            artifact: artifact.map(str::to_owned),
            observation: None,
            kind: None,
            pointer: None,
            evidence: None,
            attestations: Vec::new(),
        }
    }

    #[test]
    fn test_unit_references_finds_nested_and_array_references() {
        let document = json!({
            "variables": {"vpc_id": {"$unit": {"unit": "network", "output": "vpcId"}}},
            "hosts": ["static", {"$unit": {"unit": "database", "artifact": "endpoint"}}],
            "promoted": {"$promotion": {"value": "image"}},
            "literal": {"$literal": {"$unit": {"unit": "data"}}},
        });
        let references = unit_references("", &document).unwrap();
        assert_eq!(
            references,
            [
                UnitReference {
                    pointer: "/hosts/1".to_owned(),
                    source: source("database", None, Some("endpoint")),
                },
                UnitReference {
                    pointer: "/variables/vpc_id".to_owned(),
                    source: source("network", Some("vpcId"), None),
                },
            ]
        );
    }

    #[test]
    fn test_unit_references_escapes_tokens_below_the_given_pointer() {
        let document = json!({"platform/web": {"image": {"$unit": {"unit": "web-image", "artifact": "image"}}}});
        let references = unit_references("/releases", &document).unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].pointer, "/releases/platform~1web/image");
    }

    #[test]
    fn test_unit_references_names_the_location_of_malformed_references() {
        let document = json!({
            "both": {"$unit": {"unit": "a", "output": "x", "artifact": "y"}},
            "neither": {"$unit": {"unit": "a"}},
            "unknown": {"$unit": {"unit": "a", "output": "x", "extra": 1}},
            "mixed": {"$unit": {"unit": "a", "output": "x"}, "other": 1},
            "scalar": {"$unit": "a"},
        });
        let issues = unit_references("", &document).unwrap_err();
        assert_eq!(issues.len(), 5, "{issues:?}");
        for (issue, field) in
            issues
                .iter()
                .zip(["spec.both", "spec.mixed", "spec.neither", "spec.scalar", "spec.unknown"])
        {
            assert!(issue.starts_with(field), "{issue} names {field}");
        }
        assert!(
            issues[0].ends_with("must set exactly one of output, artifact, and observation"),
            "{issues:?}"
        );
    }
}
