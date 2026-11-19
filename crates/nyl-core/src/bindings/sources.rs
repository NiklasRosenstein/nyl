//! Typed source clauses for declared fields, and the schema that declares them.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! "Implementation shape". A field accepts a source clause only where its
//! owner's schema says so: [`Resolvable<T, S>`] is a schema-only type, applied
//! with `#[schemars(with = …)]`, that marks the field with
//! `"x-nyl-resolvable": <clause field>`. The runtime type of the field stays
//! the resolved type, because clauses are substituted on the document's
//! [`Value`] before it is deserialized ([`super::resolve_declared`]), and
//! [`resolvable_fields`] reads the markers back from the schema.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::marker::PhantomData;

use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{CoreError, Result};

/// The schema extension that marks a resolvable field. Its value names the
/// clause field the field accepts, such as `"fromValues"`.
pub const RESOLVABLE_EXTENSION: &str = "x-nyl-resolvable";

/// A kind of source clause: the field that names it, and the clause object
/// it takes. Each kind is one uninhabited marker type, such as [`FromValues`].
pub trait SourceKind {
    /// The clause's only field, as authored, such as `fromValues`.
    const FIELD: &'static str;
    /// The object the clause field holds.
    type Clause: SourceClause;
}

/// The object a source clause holds.
pub trait SourceClause: JsonSchema + DeserializeOwned {
    /// Validate the static form of the clause at `field`, such as
    /// `spec.target.fromValues`.
    fn validate(&self, field: &str) -> Result<()>;

    /// The value used when the source holds none. Never `null`.
    fn default_value(&self) -> Option<&Value> {
        None
    }

    /// Whether a missing value removes the field, as if it were omitted.
    fn optional(&self) -> bool {
        false
    }
}

/// `fromValues`: a unit field read from the environment's effective values.
pub enum FromValues {}

impl SourceKind for FromValues {
    const FIELD: &'static str = "fromValues";
    type Clause = ValuesSource;
}

/// `fromInput`: a Nyl resource field read from a Release input.
pub enum FromInput {}

impl SourceKind for FromInput {
    const FIELD: &'static str = "fromInput";
    type Clause = InputSource;
}

/// `fromParameter`: a value of an EnvironmentTemplate's `spec.values` read
/// from the instance's bound parameter. It resolves at any depth of those
/// values ([`super::resolve_document`] with [`super::TEMPLATE_VALUE_KINDS`]),
/// not at fields a schema marks, so no struct names it.
pub enum FromParameter {}

impl SourceKind for FromParameter {
    const FIELD: &'static str = "fromParameter";
    type Clause = ParameterSource;
}

/// Reads a value from the environment's effective `values`.
///
/// When nothing is at `pointer`, or the value there is `null`, `default`
/// applies; with `optional: true` the field is removed instead, as if it were
/// omitted; otherwise it is an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ValuesSource {
    /// JSON Pointer into the environment's effective values, such as `/target`.
    pub pointer: String,
    /// Value used when nothing is at `pointer` or the value there is `null`. Must not be `null`; use `optional` to leave the field out instead. Exclusive with `optional`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_non_null"
    )]
    pub default: Option<Value>,
    /// Remove the field when nothing is at `pointer` or the value there is `null`, as if it were omitted. Exclusive with `default`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl SourceClause for ValuesSource {
    fn validate(&self, field: &str) -> Result<()> {
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)?;
        if self.default.is_some() && self.optional {
            return Err(CoreError::config(format!(
                "{field} sets both default and optional; set default to use a value when none is found, or optional: true to omit the field"
            )));
        }
        Ok(())
    }

    fn default_value(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    fn optional(&self) -> bool {
        self.optional
    }
}

/// Reads a value from a Release input. When the target binds none, the
/// input's declared default applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InputSource {
    /// Name of an input the Release declares.
    pub name: String,
    /// JSON Pointer inside the input's value; the whole value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

impl SourceClause for InputSource {
    fn validate(&self, field: &str) -> Result<()> {
        validate_named(field, &self.name, self.pointer.as_deref())
    }
}

/// Reads an EnvironmentTemplate parameter bound for the instance. The
/// parameter's declared default applies when the instance binds none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ParameterSource {
    /// Name of a parameter the template declares.
    pub name: String,
    /// JSON Pointer inside the parameter's value; the whole value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

impl SourceClause for ParameterSource {
    fn validate(&self, field: &str) -> Result<()> {
        validate_named(field, &self.name, self.pointer.as_deref())
    }
}

fn validate_named(field: &str, name: &str, pointer: Option<&str>) -> Result<()> {
    if name.is_empty() {
        return Err(CoreError::config(format!("{field}.name must not be empty")));
    }
    pointer.map_or(Ok(()), |pointer| {
        crate::json_pointer::validate(&format!("{field}.pointer"), pointer)
    })
}

/// Deserialize a present field as `Some`, and reject `null` instead of
/// reading it as an absent field.
fn present_non_null<'de, D>(deserializer: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Null => Err(serde::de::Error::custom(
            "default must not be null; use optional: true to omit the field when no value is found",
        )),
        value => Ok(Some(value)),
    }
}

/// Schema-only: a field that holds a literal `T` or one `S` clause.
///
/// Applied with `#[schemars(with = "Resolvable<T, S>")]`; the field's runtime
/// type stays `T`. Its schema is `anyOf: [T, {<S::FIELD>: S::Clause}]` with
/// `"x-nyl-resolvable": <S::FIELD>`, which [`resolvable_fields`] reads.
pub struct Resolvable<T, S: SourceKind>(PhantomData<(T, S)>);

impl<T: JsonSchema, S: SourceKind> JsonSchema for Resolvable<T, S> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        format!("{}_{}", T::schema_name(), S::FIELD).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let literal = generator.subschema_for::<T>();
        resolvable_schema::<S>(generator, literal)
    }

    // An `Option` field stays optional: the derive asks the `with` type, not
    // the field's own type, whether a field without `#[serde(default)]` is
    // required.
    fn _schemars_private_is_option() -> bool {
        T::_schemars_private_is_option()
    }
}

/// The schema of a field that holds `literal` or one `S` clause, for a schema
/// written by hand instead of derived: what [`Resolvable<T, S>`] generates
/// for a `T` whose schema is `literal`. A `description` on `literal` moves to
/// the field, where editors show it.
pub fn resolvable_schema<S: SourceKind>(generator: &mut SchemaGenerator, literal: Schema) -> Schema {
    let mut literal = literal;
    let description = literal.as_object_mut().and_then(|object| object.remove("description"));
    let clause = generator.subschema_for::<S::Clause>();
    let mut schema = json_schema!({
        "anyOf": [
            literal,
            {
                "type": "object",
                "properties": {S::FIELD: clause},
                "required": [S::FIELD],
                "additionalProperties": false,
            },
        ],
        RESOLVABLE_EXTENSION: S::FIELD,
    });
    if let Some(description) = description {
        schema.insert("description".to_owned(), description);
    }
    schema
}

/// Replace every field `schema` marks resolvable from `kind` by its literal
/// form, at any depth and in its `$defs`: for a document whose fields are
/// shared with documents that resolve `kind`, but that does not, such as a
/// declared DeploymentTarget, whose bindings a unit document's inline target
/// shares. A field keeps its description.
pub fn without_clauses(schema: &mut Value, kind: &str) {
    match schema {
        Value::Object(object) => {
            if object.get(RESOLVABLE_EXTENSION).and_then(Value::as_str) == Some(kind) {
                let literal = object
                    .get("anyOf")
                    .and_then(Value::as_array)
                    .and_then(|branches| branches.first())
                    .cloned();
                if let Some(Value::Object(mut literal)) = literal {
                    if let Some(description) = object.get("description") {
                        literal.insert("description".to_owned(), description.clone());
                    }
                    *object = literal;
                }
            }
            for child in object.values_mut() {
                without_clauses(child, kind);
            }
        }
        Value::Array(items) => {
            for item in items {
                without_clauses(item, kind);
            }
        }
        _ => {}
    }
}

/// A field a schema marks resolvable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvableField {
    /// JSON Pointer of the field inside a document of the schema, such as `/target`.
    pub pointer: String,
    /// The clause field it accepts, such as `fromValues`.
    pub kind: String,
}

/// Every field `schema` marks resolvable, in pointer order.
///
/// The walk follows object `properties`, `$ref` into the schema's `$defs`,
/// and the `anyOf`/`oneOf`/`allOf` branches of optional and flattened fields.
/// It stops at a marked field and does not descend into arrays or maps, so a
/// resolvable field is always a fixed path of named properties.
pub fn resolvable_fields(schema: &Schema) -> Vec<ResolvableField> {
    let root = schema.as_value();
    let mut found = BTreeSet::new();
    collect(root, root, "", &mut Vec::new(), &mut found);
    found.into_iter().collect()
}

fn collect<'a>(
    root: &'a Value,
    node: &'a Value,
    pointer: &str,
    refs: &mut Vec<&'a str>,
    found: &mut BTreeSet<ResolvableField>,
) {
    let Some(object) = node.as_object() else {
        return;
    };
    if let Some(kind) = object.get(RESOLVABLE_EXTENSION).and_then(Value::as_str) {
        found.insert(ResolvableField {
            pointer: pointer.to_owned(),
            kind: kind.to_owned(),
        });
        return;
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        // A recursive type refers to itself; each definition is walked once per path.
        if !refs.contains(&reference) {
            if let Some(target) = reference.strip_prefix('#').and_then(|path| root.pointer(path)) {
                refs.push(reference);
                collect(root, target, pointer, refs, found);
                refs.pop();
            }
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, property) in properties {
            collect(root, property, &crate::json_pointer::child(pointer, key), refs, found);
        }
    }
    for branches in ["anyOf", "oneOf", "allOf"] {
        for branch in object.get(branches).and_then(Value::as_array).into_iter().flatten() {
            collect(root, branch, pointer, refs, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_values_source_rejects_null_default_and_unknown_fields() {
        let source: ValuesSource = serde_json::from_value(json!({"pointer": "/a", "default": 0})).unwrap();
        assert_eq!(source.default_value(), Some(&json!(0)));
        let error = serde_json::from_value::<ValuesSource>(json!({"pointer": "/a", "default": null})).unwrap_err();
        assert!(error.to_string().contains("default must not be null"), "{error}");
        assert!(serde_json::from_value::<ValuesSource>(json!({"pointer": "/a", "key": "x"})).is_err());
        assert!(serde_json::from_value::<InputSource>(json!({"name": "a", "default": 1})).is_err());
        assert!(serde_json::from_value::<ParameterSource>(json!({"name": "a", "optional": true})).is_err());
    }

    #[test]
    fn test_clause_validation() {
        let values = |value| serde_json::from_value::<ValuesSource>(value).unwrap();
        values(json!({"pointer": "/a", "optional": true}))
            .validate("f")
            .unwrap();
        let error = values(json!({"pointer": "/a", "default": 1, "optional": true}))
            .validate("f")
            .unwrap_err();
        assert!(
            error.to_string().contains("f sets both default and optional"),
            "{error}"
        );
        let error = values(json!({"pointer": "a"})).validate("f").unwrap_err();
        assert!(
            error.to_string().contains("f.pointer must be a JSON Pointer"),
            "{error}"
        );

        let input = |value| serde_json::from_value::<InputSource>(value).unwrap();
        input(json!({"name": "crds", "pointer": "/x"})).validate("f").unwrap();
        assert!(input(json!({"name": "crds", "pointer": "x"})).validate("f").is_err());
        assert!(input(json!({"name": ""})).validate("f").is_err());
        let parameter = serde_json::from_value::<ParameterSource>(json!({"name": "pr"})).unwrap();
        parameter.validate("f").unwrap();
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Nested {
        #[schemars(with = "Resolvable<bool, FromInput>")]
        flag: bool,
        plain: String,
    }

    #[derive(JsonSchema)]
    #[serde(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct Spec {
        /// The target.
        #[schemars(with = "Resolvable<String, FromValues>")]
        target: String,
        #[schemars(with = "Resolvable<Option<u32>, FromValues>")]
        teardown_wait: Option<u32>,
        nested: Option<Nested>,
        #[serde(flatten)]
        flattened: Flattened,
        items: Vec<Nested>,
        recursive: Option<Box<Recursive>>,
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Flattened {
        #[schemars(with = "Resolvable<String, FromValues>")]
        approval: String,
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Recursive {
        next: Option<Box<Recursive>>,
        #[schemars(with = "Resolvable<String, FromValues>")]
        leaf: String,
    }

    #[test]
    fn test_resolvable_schema_offers_the_literal_or_one_clause() {
        let schema = schemars::schema_for!(Spec);
        let target = &schema.as_value()["properties"]["target"];
        assert_eq!(target[RESOLVABLE_EXTENSION], json!("fromValues"));
        assert_eq!(target["description"], json!("The target."));
        assert_eq!(target["anyOf"][0], json!({"type": "string"}));
        assert_eq!(target["anyOf"][1]["required"], json!(["fromValues"]));
        assert_eq!(target["anyOf"][1]["additionalProperties"], json!(false));
        // An `Option` field stays optional without `#[serde(default)]`.
        assert_eq!(schema.as_value()["required"], json!(["target", "approval", "items"]));
    }

    #[test]
    fn test_resolvable_schema_wraps_a_hand_written_literal() {
        let mut generator = SchemaGenerator::default();
        let schema = resolvable_schema::<FromValues>(
            &mut generator,
            json_schema!({"type": "object", "description": "When phase 1 ends."}),
        );
        let schema = schema.as_value();
        assert_eq!(schema["description"], json!("When phase 1 ends."));
        assert_eq!(schema["anyOf"][0], json!({"type": "object"}));
        assert_eq!(schema[RESOLVABLE_EXTENSION], json!("fromValues"));
    }

    #[test]
    fn test_without_clauses_keeps_the_literal_and_the_description() {
        let mut schema = schemars::schema_for!(Spec).to_value();
        without_clauses(&mut schema, "fromValues");
        assert_eq!(
            schema["properties"]["target"],
            json!({"type": "string", "description": "The target."})
        );
        assert!(resolvable_fields(&serde_json::from_value(schema).unwrap())
            .iter()
            .all(|field| field.kind == "fromInput"));
    }

    #[test]
    fn test_resolvable_fields_follow_refs_optional_and_flattened_fields() {
        let fields = resolvable_fields(&schemars::schema_for!(Spec));
        let field = |pointer: &str, kind: &str| ResolvableField {
            pointer: pointer.to_owned(),
            kind: kind.to_owned(),
        };
        // Arrays are not walked: `/items/*/flag` is no fixed path.
        assert_eq!(
            fields,
            [
                field("/approval", "fromValues"),
                field("/nested/flag", "fromInput"),
                field("/recursive/leaf", "fromValues"),
                field("/target", "fromValues"),
                field("/teardownWait", "fromValues"),
            ]
        );
    }
}
