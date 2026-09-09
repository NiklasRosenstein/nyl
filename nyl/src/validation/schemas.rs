//! Deterministic conversion of structural CRD schemas to JSON Schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{NylError, Result};

/// A CRD's complete served-version contract. Empty versions still shadow older definitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrdSchemas {
    pub group: String,
    pub kind: String,
    pub versions: BTreeMap<String, SchemaVariants>,
}

/// Both modes are captured independently of the project's current strictness policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaVariants {
    pub strict: Value,
    pub permissive: Value,
}

/// Extract only schema-relevant CRD fields; do not retain status or Kubernetes metadata.
pub fn extract_crds(resources: &[Value]) -> Result<BTreeMap<String, CrdSchemas>> {
    let mut definitions = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for resource in resources {
        if resource.get("kind").and_then(Value::as_str) != Some("CustomResourceDefinition")
            || resource.get("apiVersion").and_then(Value::as_str) != Some("apiextensions.k8s.io/v1")
        {
            continue;
        }
        let name = required(resource, "/metadata/name")?;
        let group = required(resource, "/spec/group")?;
        let kind = required(resource, "/spec/names/kind")?;
        if let Some(other) = identities.insert((group.clone(), kind.clone()), name.clone()) {
            if other != name {
                return Err(NylError::validation(format!(
                    "Conflicting desired CRDs for {group}/{kind}: {other}, {name}"
                )));
            }
        }
        let mut versions = BTreeMap::new();
        let declared = resource
            .pointer("/spec/versions")
            .and_then(Value::as_array)
            .ok_or_else(|| NylError::validation(format!("CRD {name} requires spec.versions")))?;
        let mut seen = std::collections::BTreeSet::new();
        for version in declared {
            let version_name = required(version, "/name")?;
            if !seen.insert(version_name.clone()) {
                return Err(NylError::validation(format!(
                    "CRD {name} repeats version {version_name}"
                )));
            }
            if version.get("served").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let schema = version
                .pointer("/schema/openAPIV3Schema")
                .filter(|value| value.is_object())
                .ok_or_else(|| {
                    NylError::validation(format!(
                        "CRD {name} served version {version_name} has no structural schema"
                    ))
                })?;
            let api_version = format!("{group}/{version_name}");
            versions.insert(
                version_name,
                SchemaVariants {
                    strict: convert_root(schema.clone(), &api_version, &kind, true)?,
                    permissive: convert_root(schema.clone(), &api_version, &kind, false)?,
                },
            );
        }
        let definition = CrdSchemas { group, kind, versions };
        if let Some(previous) = definitions.insert(name.clone(), definition.clone()) {
            if previous != definition {
                return Err(NylError::validation(format!(
                    "Conflicting desired CRD definitions: {name}"
                )));
            }
        }
    }
    Ok(definitions)
}

fn required(value: &Value, path: &str) -> Result<String> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| NylError::validation(format!("CRD field {path} must be a non-empty string")))
}

fn convert_root(mut schema: Value, api_version: &str, kind: &str, strict: bool) -> Result<Value> {
    add_envelope(&mut schema, Some((api_version, kind)));
    convert_node(&mut schema, strict, true)?;
    schema["$schema"] = json!("http://json-schema.org/draft-07/schema#");
    Ok(schema)
}

fn add_envelope(schema: &mut Value, identity: Option<(&str, &str)>) {
    let object = schema.as_object_mut().expect("structural schema is an object");
    let properties = object.entry("properties").or_insert_with(|| json!({}));
    if let Some(properties) = properties.as_object_mut() {
        properties
            .entry("apiVersion")
            .or_insert_with(|| json!({"type": "string"}));
        properties.entry("kind").or_insert_with(|| json!({"type": "string"}));
        // Kubernetes owns ObjectMeta; CRD schemas may only constrain name/generateName.
        let metadata = properties
            .entry("metadata")
            .or_insert_with(|| json!({"type": "object"}));
        if let Some(metadata) = metadata.as_object_mut() {
            metadata.entry("type").or_insert_with(|| json!("object"));
            metadata.insert("additionalProperties".into(), json!(true));
            let fields = metadata.entry("properties").or_insert_with(|| json!({}));
            if let Some(fields) = fields.as_object_mut() {
                for (name, constraint) in metadata_properties().as_object().unwrap() {
                    fields.entry(name.clone()).or_insert_with(|| constraint.clone());
                }
            }
        }
        if let Some((api_version, kind)) = identity {
            properties.insert("apiVersion".into(), json!({"type": "string", "const": api_version}));
            properties.insert("kind".into(), json!({"type": "string", "const": kind}));
        }
    }
    let required = object.entry("required").or_insert_with(|| json!([]));
    if let Some(required) = required.as_array_mut() {
        for field in if identity.is_some() {
            &["apiVersion", "kind", "metadata"][..]
        } else {
            &["apiVersion", "kind"][..]
        } {
            if !required.iter().any(|value| value == *field) {
                required.push(json!(field));
            }
        }
    }
}

// ObjectMeta accepts server-owned fields while checking the types of its known fields.
fn metadata_properties() -> Value {
    json!({
        "name":{"type":"string"}, "generateName":{"type":"string"},
        "namespace":{"type":"string"}, "uid":{"type":"string"},
        "resourceVersion":{"type":"string"}, "generation":{"type":"integer"},
        "creationTimestamp":{"type":["string","null"]},
        "deletionTimestamp":{"type":["string","null"]},
        "deletionGracePeriodSeconds":{"type":["integer","null"]},
        "labels":{"type":["object","null"],"additionalProperties":{"type":"string"}},
        "annotations":{"type":["object","null"],"additionalProperties":{"type":"string"}},
        "finalizers":{"type":["array","null"],"items":{"type":"string"}},
        "ownerReferences":{"type":["array","null"],"items":{"type":"object","properties":{
            "apiVersion":{"type":"string"},"kind":{"type":"string"},
            "name":{"type":"string"},"uid":{"type":"string"},
            "controller":{"type":["boolean","null"]},"blockOwnerDeletion":{"type":["boolean","null"]}
        },"required":["apiVersion","kind","name","uid"]}},
        "managedFields":{"type":["array","null"],"items":{"type":"object","additionalProperties":true}}
    })
}

/// Walk schema positions only: defaults/examples can contain arbitrary application JSON.
fn convert_node(schema: &mut Value, strict: bool, structural: bool) -> Result<()> {
    if schema.get("x-kubernetes-embedded-resource").and_then(Value::as_bool) == Some(true) {
        add_envelope(schema, None);
    }
    let Some(object) = schema.as_object_mut() else {
        return Ok(());
    };
    if object.contains_key("$ref") {
        return Err(NylError::validation("Structural CRD schemas cannot contain $ref"));
    }
    let int_or_string = object.get("x-kubernetes-int-or-string").and_then(Value::as_bool) == Some(true)
        || object.get("format").and_then(Value::as_str) == Some("int-or-string");
    if int_or_string {
        object.remove("format");
        object.insert("type".into(), json!(["integer", "string"]));
    }
    // CRDs use boolean OpenAPI bounds; Draft 7 puts the bound in the exclusive keyword.
    for (exclusive, inclusive) in [("exclusiveMinimum", "minimum"), ("exclusiveMaximum", "maximum")] {
        if let Some(flag) = object.remove(exclusive) {
            match flag {
                Value::Bool(true) => {
                    let bound = object
                        .remove(inclusive)
                        .ok_or_else(|| NylError::validation(format!("CRD schema {exclusive} requires {inclusive}")))?;
                    object.insert(exclusive.into(), bound);
                }
                Value::Bool(false) => {}
                _ => {
                    return Err(NylError::validation(format!(
                        "CRD schema {exclusive} must be a boolean"
                    )))
                }
            }
        }
    }
    let nullable = object.remove("nullable").and_then(|value| value.as_bool()) == Some(true);
    for key in ["properties", "patternProperties", "definitions", "$defs"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                convert_node(child, strict, structural)?;
            }
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(child) = object.get_mut(key) {
            convert_node(child, strict, structural)?;
        }
    }
    for key in ["not", "contains"] {
        if let Some(child) = object.get_mut(key) {
            convert_node(child, strict, false)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                convert_node(child, strict, false)?;
            }
        }
    }
    let preserves = object
        .get("x-kubernetes-preserve-unknown-fields")
        .and_then(Value::as_bool)
        == Some(true);
    if preserves && !object.contains_key("additionalProperties") {
        object.insert("additionalProperties".into(), json!(true));
    } else if strict
        && structural
        && (object.contains_key("properties") || object.get("type").and_then(Value::as_str) == Some("object"))
        && !object.contains_key("additionalProperties")
    {
        object.insert("additionalProperties".into(), json!(false));
    }
    if nullable {
        // Kubernetes skips composition validators for null, but still checks enum.
        let enumeration = object.remove("enum");
        let non_null = std::mem::take(schema);
        *schema = json!({"anyOf": [non_null, {"type": "null"}]});
        if let Some(enumeration) = enumeration {
            schema["enum"] = enumeration;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crd(schema: Value) -> Value {
        json!({"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com"}, "spec":{"group":"example.com","names":{"kind":"Widget"},
            "versions":[{"name":"v1","served":true,"schema":{"openAPIV3Schema":schema}}]}})
    }

    #[test]
    fn test_conversion_preserves_structural_semantics() {
        let result = extract_crds(&[crd(json!({"type":"object","properties":{
            "spec":{"type":"object","properties":{
                "port":{"x-kubernetes-int-or-string":true},
                "optional":{"type":"string","nullable":true,"enum":["yes"]},
                "free":{"type":"object","x-kubernetes-preserve-unknown-fields":true,"properties":{"known":{"type":"object","properties":{"a":{"type":"string"}}}}},
                "map":{"type":"object","additionalProperties":{"type":"integer"}},
                "embedded":{"type":"object","x-kubernetes-embedded-resource":true,"x-kubernetes-preserve-unknown-fields":true}
            }}}}))]).unwrap();
        let variants = &result["widgets.example.com"].versions["v1"];
        let spec = &variants.strict["properties"]["spec"];
        assert_eq!(spec["additionalProperties"], false);
        assert_eq!(spec["properties"]["port"]["type"], json!(["integer", "string"]));
        assert_eq!(spec["properties"]["optional"]["anyOf"][1], json!({"type":"null"}));
        assert_eq!(spec["properties"]["free"]["additionalProperties"], true);
        assert_eq!(
            spec["properties"]["free"]["properties"]["known"]["additionalProperties"],
            false
        );
        assert_eq!(spec["properties"]["map"]["additionalProperties"]["type"], "integer");
        assert_eq!(
            spec["properties"]["embedded"]["properties"]["apiVersion"]["type"],
            "string"
        );
        assert_eq!(variants.strict["properties"]["metadata"]["additionalProperties"], true);
        assert!(variants.permissive["properties"]["spec"]
            .get("additionalProperties")
            .is_none());
    }

    #[test]
    fn test_conversion_keeps_complete_version_inventory_and_rejects_conflicts() {
        let mut resource = crd(json!({"type":"object"}));
        resource["spec"]["versions"][0]["served"] = json!(false);
        assert!(extract_crds(&[resource]).unwrap()["widgets.example.com"]
            .versions
            .is_empty());
        assert!(extract_crds(&[
            crd(json!({"type":"object"})),
            crd(json!({"type":"object","required":["spec"]}))
        ])
        .is_err());
    }
}
