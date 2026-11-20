//! Value sources and JSON Schemas: the reserved `$` namespace of a typed
//! field, read from a schema, and the source variants editor schemas offer.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! "The `$` namespace" and "Editor schemas". Both read only a JSON Schema and
//! the [`Context`] table, never a kind's name, so a unit kind's driver keeps
//! its schema and every kind gets the same rules:
//!
//! - [`reserved_key_issues`] walks a document along its schema. Where the
//!   schema has no place for a `$` key that is not a known source, the key is
//!   in a typed field, so it is an unknown source and an error; where the
//!   schema leaves the object open (free-form data, such as a Terraform
//!   unit's `variables`), the key is data.
//! - [`source_variants`] lists the sources a context accepts as JSON Schema
//!   alternatives, and [`offer_sources`] adds them next to the top-level
//!   `spec` fields of a resource schema.

use schemars::{Schema, SchemaGenerator};
use serde_json::{json, Value};

use super::{
    clause_key, suggest, Context, FileInputSource, GitInputSource, InputSource, ParameterSource, PromotionInputSource,
    PublicationInputSource, Source, Treatment, UnitInputSource, ValuesSource, VALUE_SOURCES_DOCS,
};

/// What one step of a schema walk found for a member or an item.
#[derive(Debug, Clone)]
pub enum SchemaStep {
    /// The schema that describes the member.
    Node(Value),
    /// The schema says nothing about the member: any value may be there.
    Free,
    /// The schema has no place for the member.
    Missing,
}

/// The alternatives of a schema node: itself, or its `anyOf`, `oneOf`, and
/// `allOf` branches, with `$ref`s inside `root` followed.
pub fn schema_alternatives(root: &Value, node: &Value) -> Vec<Value> {
    let node = schema_resolve(root, node);
    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(items)) = node.get(key) {
            for item in items {
                branches.extend(schema_alternatives(root, item));
            }
        }
    }
    if branches.is_empty() {
        vec![node]
    } else {
        branches
    }
}

/// `node` with its `$ref` inside `root` followed, if it has one.
pub fn schema_resolve(root: &Value, node: &Value) -> Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node.clone();
    };
    reference
        .strip_prefix('#')
        .and_then(|pointer| root.pointer(pointer))
        .map_or_else(|| node.clone(), |found| schema_resolve(root, found))
}

/// The schema of member (or item) `segment` of a value that `node` describes.
pub fn schema_step(root: &Value, node: &Value, segment: &str) -> SchemaStep {
    let mut free = false;
    for branch in schema_alternatives(root, node) {
        let branch = &branch;
        if let Some(property) = branch.get("properties").and_then(|properties| properties.get(segment)) {
            return SchemaStep::Node(property.clone());
        }
        match branch.get("additionalProperties") {
            Some(Value::Object(_)) => return SchemaStep::Node(branch["additionalProperties"].clone()),
            Some(Value::Bool(false)) => continue,
            _ => {}
        }
        if let Some(items) = branch.get("items") {
            if segment.parse::<usize>().is_ok() {
                return SchemaStep::Node(items.clone());
            }
            continue;
        }
        let kinds = match branch.get("type") {
            Some(Value::String(kind)) => vec![kind.as_str()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let scalar = !kinds.is_empty() && kinds.iter().all(|kind| !matches!(*kind, "object" | "array"));
        // An object that does not close itself accepts other members.
        if !scalar {
            free = true;
        }
    }
    if free {
        SchemaStep::Free
    } else {
        SchemaStep::Missing
    }
}

/// Report every `$` key of `document` that is not a known source, where the
/// JSON Schema `root` has no place for it: inside a typed field the whole `$`
/// namespace is reserved, so such a key is an unknown source. `field` names
/// `document` in messages, such as `spec`. A known source is left to the
/// walker of its context, and a subtree the schema leaves open is data.
pub fn reserved_key_issues(field: &str, document: &Value, root: &Value) -> Vec<String> {
    let mut issues = Vec::new();
    reserved(field, document, std::slice::from_ref(root), root, &mut issues);
    issues
}

fn reserved(field: &str, value: &Value, nodes: &[Value], root: &Value, issues: &mut Vec<String>) {
    let children: Vec<(String, String, &Value)> = match value {
        Value::Object(fields) => {
            if !matches!(clause_key(fields), Ok(None)) {
                return;
            }
            fields
                .iter()
                .map(|(key, child)| (key.clone(), format!("{field}.{key}"), child))
                .collect()
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, child)| (index.to_string(), format!("{field}[{index}]"), child))
            .collect(),
        _ => return,
    };
    for (segment, child_field, child) in children {
        let mut next = Vec::new();
        let mut free = false;
        for node in nodes {
            match schema_step(root, node, &segment) {
                SchemaStep::Node(found) => next.push(found),
                SchemaStep::Free => free = true,
                SchemaStep::Missing => {}
            }
        }
        if next.is_empty() {
            let dollar = segment.starts_with('$') && value.is_object();
            if dollar && !(free && nodes.iter().any(|node| admits_foreign_keys(root, node))) {
                issues.push(unknown_source(&child_field, &segment));
            }
            continue;
        }
        reserved(&child_field, child, &next, root, issues);
    }
}

/// Whether an object `node` describes has room for a `$` key it does not
/// name: one of its alternatives declares no members, such as free-form data
/// (`{"type": "object"}` or no schema at all), or admits others explicitly
/// (`additionalProperties` true or a schema). An object that names its
/// members (`properties` or `required`) and says nothing more is a typed
/// field, whose `$` namespace is reserved even when its schema does not close
/// it, so a slip there is reported as an unknown source instead of reaching
/// the kind's own validation as a missing or unknown field.
fn admits_foreign_keys(root: &Value, node: &Value) -> bool {
    schema_alternatives(root, node)
        .iter()
        .any(|branch| match branch.get("additionalProperties") {
            Some(Value::Bool(false)) => false,
            Some(_) => true,
            None => {
                let names_members = branch.get("properties").is_some() || branch.get("required").is_some();
                let scalar = match branch.get("type") {
                    Some(Value::String(kind)) => kind != "object",
                    Some(Value::Array(kinds)) => !kinds.iter().any(|kind| kind == "object"),
                    _ => false,
                };
                !names_members && !scalar && branch.get("items").is_none()
            }
        })
}

/// The error for `key`, an unknown `$` key at `field` of a typed field.
fn unknown_source(field: &str, key: &str) -> String {
    format!(
        "{field} is not a value source; `$` keys are reserved for value sources in the typed fields of a resource{} (see {VALUE_SOURCES_DOCS})",
        suggest(key).map(|meant| format!("; did you mean {meant}?")).unwrap_or_default()
    )
}

impl Source {
    /// What the source reads, for editor schemas.
    fn schema_description(self) -> &'static str {
        match self {
            Self::Environment => {
                "Reads the environment's effective values at a JSON Pointer; resolved when the unit renders."
            }
            Self::Input => "Reads an input the Release declares.",
            Self::Parameter => "Reads a parameter of the EnvironmentTemplate instance.",
            Self::Unit => "Reads a unit's output, artifact, or observed value; resolved by orchestration.",
            Self::Promotion => "Reads a value recorded through a PromotionPath; resolved by orchestration.",
            Self::File => "Reads the value from a YAML or JSON file of this repository.",
            Self::Git => "Reads the value from a file of a Git repository at a locked commit.",
            Self::Publication => "Reads the value from a state file in this target's publication branch.",
            Self::Literal => "The content, verbatim, even when it looks like a value source.",
        }
    }

    /// The schema of the source's body: its object form, and its shorthand.
    fn body_schema(self, generator: &mut SchemaGenerator) -> Schema {
        let with_shorthand =
            |shorthand: Value, object: Schema| -> Schema { schemars::json_schema!({"anyOf": [shorthand, object]}) };
        match self {
            Self::Environment => with_shorthand(
                json!({"type": "string", "description": "Shorthand: a JSON Pointer into the values, such as /target."}),
                generator.subschema_for::<ValuesSource>(),
            ),
            Self::Input => with_shorthand(
                json!({"type": "string", "description": "Shorthand: the input's name."}),
                generator.subschema_for::<InputSource>(),
            ),
            Self::Parameter => with_shorthand(
                json!({"type": "string", "description": "Shorthand: the parameter's name."}),
                generator.subschema_for::<ParameterSource>(),
            ),
            Self::Unit => generator.subschema_for::<UnitInputSource>(),
            Self::Promotion => generator.subschema_for::<PromotionInputSource>(),
            Self::File => generator.subschema_for::<FileInputSource>(),
            Self::Git => generator.subschema_for::<GitInputSource>(),
            Self::Publication => generator.subschema_for::<PublicationInputSource>(),
            Self::Literal => schemars::json_schema!(true),
        }
    }
}

/// The sources `context` accepts, resolved there or carried, each as a JSON
/// Schema alternative `{$source: body}`, in [`Source::ALL`] order. The order
/// of a Release-input binding's sources keeps the files first.
pub fn source_variants(context: Context, generator: &mut SchemaGenerator) -> Vec<Schema> {
    let mut sources = Source::ALL
        .into_iter()
        .filter(|source| context.treats(*source) != Treatment::Reject)
        .collect::<Vec<_>>();
    if context == Context::ReleaseInputs {
        sources.sort_by_key(|source| match source {
            Source::File => 0,
            Source::Git => 1,
            Source::Publication => 2,
            Source::Unit => 3,
            Source::Promotion => 4,
            _ => 5,
        });
    }
    sources
        .into_iter()
        .map(|source| {
            let key = source.key();
            schemars::json_schema!({
                "type": "object",
                "description": source.schema_description(),
                "properties": {key: source.body_schema(generator)},
                "required": [key],
                "additionalProperties": false,
            })
        })
        .collect()
}

/// The `$defs` name of the value sources `context` accepts.
fn definition_name(context: Context) -> &'static str {
    match context {
        Context::UnitRender => "UnitDocumentValueSource",
        Context::ReleaseBundle | Context::ReleaseData => "ReleaseBundleValueSource",
        Context::ReleaseInputs => "ReleaseInputValueSource",
        Context::TemplateValues => "TemplateValueSource",
        Context::UnitTargetInputs => "UnitTargetInputValueSource",
        Context::UnitExecution => "UnitExecutionValueSource",
        Context::EnvironmentValues => "EnvironmentValueSource",
        Context::Literal => "LiteralValueSource",
    }
}

/// Offer the value sources `context` accepts next to each top-level field of
/// the `spec` of the resource schema `schema`, for editors: each field
/// becomes `anyOf: [<its schema>, <a source>]`. Sources at any depth below
/// those fields are valid as well, but editors are not offered them there.
/// A schema without `spec` properties is left as it is.
pub fn offer_sources(schema: &mut Value, context: Context) {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let variants = source_variants(context, &mut generator);
    let name = definition_name(context);
    let definitions = generator.take_definitions(true);
    let Some(spec) = schema.pointer("/properties/spec").cloned() else {
        return;
    };
    // The spec's properties live where its `$ref` points, if it has one.
    let spec_pointer = match spec.get("$ref").and_then(Value::as_str) {
        Some(reference) => match reference.strip_prefix('#') {
            Some(pointer) => pointer.to_owned(),
            None => return,
        },
        None => "/properties/spec".to_owned(),
    };
    let Some(Value::Object(properties)) = schema.pointer_mut(&format!("{spec_pointer}/properties")) else {
        return;
    };
    let reference = json!({"$ref": format!("#/$defs/{name}")});
    for property in properties.values_mut() {
        let mut original = std::mem::take(property);
        let mut wrapped = serde_json::Map::new();
        for annotation in ["description", "default"] {
            if let Some(value) = original.as_object_mut().and_then(|fields| fields.remove(annotation)) {
                wrapped.insert(annotation.to_owned(), value);
            }
        }
        if original.as_object().is_some_and(serde_json::Map::is_empty) {
            original = Value::Bool(true);
        }
        wrapped.insert("anyOf".to_owned(), json!([original, reference]));
        *property = Value::Object(wrapped);
    }
    let defs = schema
        .as_object_mut()
        .expect("a resource schema is an object")
        .entry("$defs")
        .or_insert_with(|| json!({}));
    let Some(defs) = defs.as_object_mut() else {
        return;
    };
    for (key, definition) in definitions {
        defs.entry(key).or_insert(definition);
    }
    defs.insert(
        name.to_owned(),
        json!({
            "description": format!("A value source this field accepts instead of a literal. See {VALUE_SOURCES_DOCS}."),
            "anyOf": variants,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "source": {"$ref": "#/$defs/Source"},
                "name": {"type": ["string", "null"]},
                "variables": {"type": "object", "additionalProperties": true},
                "tags": {"type": "array", "items": {"$ref": "#/$defs/Source"}},
            },
            "$defs": {"Source": {"type": "object", "additionalProperties": false, "properties": {"path": {"type": "string"}}}},
        })
    }

    /// Catches an unknown `$` key in a typed field left to a generic
    /// deserialization error, and a foreign key in free-form data reported.
    #[test]
    fn test_unknown_dollar_keys_in_typed_fields_are_unknown_sources() {
        let document = json!({
            "source": {"$fromEnviroment": "/source"},
            "name": {"$fromEnv": "/name"},
            "variables": {"schema": {"$ref": "#/x"}},
            "tags": [{"path": "a", "$fromEnvironments": "/x"}],
            "carried": {"$fromUnit": {"unit": "a", "output": "b"}},
        });
        let issues = reserved_key_issues("spec", &document, &schema());
        assert_eq!(issues.len(), 3, "{issues:?}");
        assert!(
            issues[0].starts_with("spec.name.$fromEnv is not a value source; `$` keys are reserved for value sources in the typed fields of a resource; did you mean $fromEnvironment?"),
            "{issues:?}"
        );
        let issues = &issues[1..];
        assert!(
            issues[0].contains("spec.source.$fromEnviroment is not a value source; `$` keys are reserved for value sources in the typed fields of a resource; did you mean $fromEnvironment?"),
            "{issues:?}"
        );
        assert!(
            issues[1].starts_with("spec.tags[0].$fromEnvironments is not a value source;")
                && issues[1].ends_with(
                    "did you mean $fromEnvironment? (see https://niklasrosenstein.github.io/nyl/reference/value-sources/)"
                ),
            "{issues:?}"
        );
    }

    /// Catches an unknown `$` key in a typed field whose schema names its
    /// members without closing the object (a struct without
    /// `deny_unknown_fields`, or a kind's hand-written schema) read as data,
    /// so the kind's own validation reports a missing field instead of the
    /// unknown source. An object left free-form or opened explicitly keeps
    /// such a key as data.
    #[test]
    fn test_an_object_that_names_its_members_reserves_dollar_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "context": {"type": "object", "properties": {"path": {"type": "string"}}},
                "required": {"type": "object", "required": ["path"]},
                "free": {"type": "object"},
                "open": {"type": "object", "properties": {"path": {"type": "string"}}, "additionalProperties": true},
                "any": true,
            },
        });
        let document = json!({
            "context": {"$fromEnviroment": "/context"},
            "required": {"$fromEnviroment": "/context"},
            "free": {"$ref": "#/x"},
            "open": {"$ref": "#/x"},
            "any": {"$ref": "#/x"},
        });
        let issues = reserved_key_issues("spec", &document, &schema);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues[0].starts_with("spec.context.$fromEnviroment is not a value source; `$` keys are reserved for value sources in the typed fields of a resource; did you mean $fromEnvironment?"),
            "{issues:?}"
        );
        assert!(
            issues[1].starts_with("spec.required.$fromEnviroment is not a value source;"),
            "{issues:?}"
        );
    }

    #[test]
    fn test_offer_sources_wraps_top_level_spec_fields() {
        let mut resource = json!({
            "properties": {"spec": {"$ref": "#/$defs/Spec"}},
            "$defs": {"Spec": {"type": "object", "properties": {"image": {"type": "string", "description": "The image."}}}},
        });
        offer_sources(&mut resource, Context::ReleaseBundle);
        assert_eq!(
            resource["$defs"]["Spec"]["properties"]["image"],
            json!({"description": "The image.", "anyOf": [{"type": "string"}, {"$ref": "#/$defs/ReleaseBundleValueSource"}]})
        );
        let keys = resource["$defs"]["ReleaseBundleValueSource"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| variant["required"][0].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["$fromInput", "$literal"]);
        assert!(resource["$defs"].get("InputSource").is_some());
    }

    #[test]
    fn test_unit_documents_offer_render_and_run_sources() {
        let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
        let keys = source_variants(Context::UnitRender, &mut generator)
            .into_iter()
            .map(|variant| variant.as_value()["required"][0].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["$fromEnvironment", "$fromUnit", "$fromPromotion", "$literal"]);
    }
}
