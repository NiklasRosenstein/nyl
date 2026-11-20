//! Substituting source clauses at declared fields, and the fail-closed check.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! "Implementation shape". Unlike [`super::resolve_document`], which resolves
//! reference objects at any depth of a free-form map, [`resolve_declared`]
//! substitutes clauses only at the pointers a schema marks resolvable
//! ([`super::resolvable_fields`]), from the one clause kind its caller
//! supplies. Afterwards [`reject_surviving_clauses`] makes sure no clause is
//! left anywhere, so an unresolved clause is an error and never data.

use serde::Deserialize;
use serde_json::Value;

use super::{
    BindingIssue, Blocked, Origin, Provided, Provision, ResolvableField, ResolvedDocument, SourceClause, SourceKind,
};

/// Reads the value of an `S` clause.
pub trait SourceProvider<S: SourceKind> {
    type Origin: Origin;

    /// The value `clause` at `field` reads. [`Provision::Unbound`] and a `null`
    /// value both mean the source holds none, so the clause's missing-value
    /// rule applies.
    fn provide(&self, field: &str, clause: &S::Clause) -> Provided<Self::Origin>;

    /// The issue for a required clause whose source holds no value, when the
    /// provider can say more than the generic message, such as the keys that
    /// do exist.
    fn missing(&self, _field: &str, _clause: &S::Clause) -> Option<BindingIssue> {
        None
    }
}

/// Substitute the `S` clauses at the `fields` a schema marks resolvable.
///
/// `field` names `document` in messages, such as `spec`; provenance and
/// blocked values are keyed by each marked pointer. A marked pointer that
/// `document` does not hold, or that holds a literal, is left alone. When the
/// clause's source holds no value, its `default` applies, `optional` removes
/// the field, and otherwise it is an issue. A marker naming another clause
/// kind than `S` is a configuration issue: the struct is used in a document
/// that cannot resolve it.
pub fn resolve_declared<S: SourceKind, P: SourceProvider<S>>(
    field: &str,
    document: &Value,
    fields: &[ResolvableField],
    provider: &P,
    issues: &mut Vec<BindingIssue>,
) -> ResolvedDocument<P::Origin> {
    let mut resolved = ResolvedDocument {
        value: document.clone(),
        provenance: std::collections::BTreeMap::default(),
        blocked: Vec::new(),
    };
    for marked in fields {
        let location = field_name(field, &marked.pointer);
        if marked.kind != S::FIELD {
            issues.push(BindingIssue::new(format!(
                "{location} is declared resolvable from {}, but this document resolves only {}; the schema of this field does not fit where it is used",
                marked.kind,
                S::FIELD
            )));
            continue;
        }
        let Some(Value::Object(object)) = resolved.value.pointer(&marked.pointer) else {
            continue;
        };
        if !object.contains_key(S::FIELD) {
            continue;
        }
        if object.len() != 1 {
            issues.push(BindingIssue::new(format!(
                "{location} mixes {} with other fields; a clause holds only its source, and replaces the whole value",
                S::FIELD
            )));
            continue;
        }
        let clause_field = format!("{location}.{}", S::FIELD);
        let clause = match parse_clause::<S>(&clause_field, &object[S::FIELD]) {
            Ok(clause) => clause,
            Err(issue) => {
                issues.push(BindingIssue::new(issue));
                continue;
            }
        };
        match provider.provide(&location, &clause) {
            Ok(Provision::Value(input)) if !input.value.is_null() => {
                replace(&mut resolved.value, &marked.pointer, Some(input.value));
                resolved.provenance.insert(marked.pointer.clone(), input.origin);
            }
            Ok(Provision::Value(_) | Provision::Unbound) => {
                if let Some(default) = clause.default_value() {
                    replace(&mut resolved.value, &marked.pointer, Some(default.clone()));
                    resolved
                        .provenance
                        .insert(marked.pointer.clone(), P::Origin::from_default());
                } else if clause.optional() && !marked.pointer.is_empty() {
                    replace(&mut resolved.value, &marked.pointer, None);
                } else {
                    issues.push(provider.missing(&location, &clause).unwrap_or_else(|| {
                        BindingIssue::new(format!(
                            "{location} needs a value, but its {} source holds none; set a default, or optional: true to omit the field",
                            S::FIELD
                        ))
                    }));
                }
            }
            Ok(Provision::Blocked(reason)) => resolved.blocked.push(Blocked {
                field: location,
                pointer: marked.pointer.clone(),
                reason,
            }),
            Err(issue) => issues.push(issue),
        }
    }
    resolved
}

/// Substitute the `S` clauses at `fields` inside every `reference` object at
/// any depth of `document`, such as `fromValues` at `/environment` of each
/// `fromUnit`.
///
/// A reference object is an object whose only field is `reference`; its
/// value is resolved like a document with [`resolve_declared`], and every
/// other object is data that the walk descends into. `field` and `pointer`
/// locate `document` itself, such as `spec` and `""`; provenance and blocked
/// values are keyed by the clause's pointer in `document`.
pub fn resolve_in_references<S: SourceKind, P: SourceProvider<S>>(
    field: &str,
    pointer: &str,
    document: &Value,
    reference: &str,
    fields: &[ResolvableField],
    provider: &P,
    issues: &mut Vec<BindingIssue>,
) -> ResolvedDocument<P::Origin> {
    let mut resolved = ResolvedDocument {
        value: document.clone(),
        provenance: std::collections::BTreeMap::default(),
        blocked: Vec::new(),
    };
    walk_references(field, pointer, &mut resolved, None, reference, fields, provider, issues);
    resolved
}

/// Walk `resolved.value` at `at` (the whole value when `None`), which sits at
/// `field` and `pointer` of the document.
#[allow(clippy::too_many_arguments)] // The walk carries its location, inputs, and outputs.
fn walk_references<S: SourceKind, P: SourceProvider<S>>(
    field: &str,
    pointer: &str,
    resolved: &mut ResolvedDocument<P::Origin>,
    at: Option<&str>,
    reference: &str,
    fields: &[ResolvableField],
    provider: &P,
    issues: &mut Vec<BindingIssue>,
) {
    let local = at.unwrap_or("");
    let Some(value) = resolved.value.pointer(local) else {
        return;
    };
    match value {
        Value::Object(members) if members.len() == 1 && members.contains_key(reference) => {
            let inner_local = crate::json_pointer::child(local, reference);
            let inner_pointer = crate::json_pointer::child(pointer, reference);
            let inner_field = format!("{field}.{reference}");
            let inner = resolved.value.pointer(&inner_local).cloned().unwrap_or(Value::Null);
            if !inner.is_object() {
                return;
            }
            let found = resolve_declared::<S, P>(&inner_field, &inner, fields, provider, issues);
            if let Some(slot) = resolved.value.pointer_mut(&inner_local) {
                *slot = found.value;
            }
            for (clause, origin) in found.provenance {
                resolved.provenance.insert(format!("{inner_pointer}{clause}"), origin);
            }
            resolved.blocked.extend(found.blocked.into_iter().map(|mut blocked| {
                blocked.pointer = format!("{inner_pointer}{}", blocked.pointer);
                blocked
            }));
        }
        Value::Object(members) => {
            let keys = members.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                walk_references(
                    &format!("{field}.{key}"),
                    &crate::json_pointer::child(pointer, &key),
                    resolved,
                    Some(&crate::json_pointer::child(local, &key)),
                    reference,
                    fields,
                    provider,
                    issues,
                );
            }
        }
        Value::Array(items) => {
            for index in 0..items.len() {
                walk_references(
                    &format!("{field}[{index}]"),
                    &format!("{pointer}/{index}"),
                    resolved,
                    Some(&format!("{local}/{index}")),
                    reference,
                    fields,
                    provider,
                    issues,
                );
            }
        }
        _ => {}
    }
}

fn parse_clause<S: SourceKind>(field: &str, clause: &Value) -> Result<S::Clause, String> {
    if !clause.is_object() {
        return Err(format!("{field} must be an object"));
    }
    let clause = S::Clause::deserialize(clause).map_err(|error| format!("{field} is not valid: {error}"))?;
    clause.validate(field).map_err(|error| error.to_string())?;
    Ok(clause)
}

/// Set the value at `pointer`, or remove its key with `None`.
fn replace(document: &mut Value, pointer: &str, value: Option<Value>) {
    let Some((parent, token)) = pointer.rsplit_once('/') else {
        if let Some(value) = value {
            *document = value;
        }
        return;
    };
    let key = token.replace("~1", "/").replace("~0", "~");
    if let Some(Value::Object(parent)) = document.pointer_mut(parent) {
        match value {
            Some(value) => {
                parent.insert(key, value);
            }
            None => {
                parent.remove(&key);
            }
        }
    }
}

/// `spec` and `/teardownWait` name `spec.teardownWait`.
fn field_name(field: &str, pointer: &str) -> String {
    pointer.split('/').skip(1).fold(field.to_owned(), |name, token| {
        format!("{name}.{}", token.replace("~1", "/").replace("~0", "~"))
    })
}

/// Report every clause left in `document`, at any depth of its objects and
/// arrays: an object whose only field is one of `kinds`.
///
/// Run after substitution, so a clause written where none resolves, or one a
/// typo kept from resolving, is an error instead of reaching a reader as data.
/// `field` names `document` in messages, such as `spec`.
pub fn reject_surviving_clauses(field: &str, document: &Value, kinds: &[&str], issues: &mut Vec<BindingIssue>) {
    match document {
        Value::Object(fields) => {
            if let (1, Some(kind)) = (fields.len(), fields.keys().find(|key| kinds.contains(&key.as_str()))) {
                issues.push(BindingIssue::new(format!(
                    "{field} holds a {kind} clause where none resolves; {}",
                    clause_scope(kind)
                )));
                return;
            }
            for (key, child) in fields {
                reject_surviving_clauses(&format!("{field}.{key}"), child, kinds, issues);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                reject_surviving_clauses(&format!("{field}[{index}]"), child, kinds, issues);
            }
        }
        _ => {}
    }
}

/// Where a clause of `kind` resolves, for the error about one that did not.
fn clause_scope(kind: &str) -> String {
    if kind == <super::FromParameter as SourceKind>::FIELD {
        format!(
            "{kind} resolves only in an EnvironmentTemplate's spec.values, see https://niklasrosenstein.github.io/nyl/reference/value-sources/#template-parameters-fromparameter"
        )
    } else {
        format!("{kind} is accepted only by the fields that declare it")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::bindings::{FromInput, FromValues, Resolved, ValuesSource};
    use crate::orchestration::blocked::{Blocker, ProducerRef};
    use crate::orchestration::codes::Code;
    use crate::orchestration::ids::UnitName;

    #[derive(Debug, Clone, PartialEq)]
    enum TestOrigin {
        Default,
        Values(String),
    }

    impl Origin for TestOrigin {
        fn from_default() -> Self {
            Self::Default
        }
        fn from_override() -> Self {
            unreachable!("declared fields have no overrides")
        }
        fn from_value() -> Self {
            unreachable!("declared fields have no inline bindings")
        }
        fn describe(&self, field: &str) -> String {
            format!("{field} ({self:?})")
        }
    }

    /// Reads pointers from `values`; `/blocked` is not readable yet and
    /// `/rejected` is refused with the provider's own code and fix.
    struct Values(Value);

    impl SourceProvider<FromValues> for Values {
        type Origin = TestOrigin;

        fn provide(&self, _field: &str, clause: &ValuesSource) -> Provided<TestOrigin> {
            match clause.pointer.as_str() {
                "/blocked" => Ok(Provision::Blocked(Blocker::no_current_receipt(ProducerRef::local(
                    UnitName::new("network").unwrap(),
                )))),
                "/rejected" => Err(BindingIssue::coded(Code::UnitInvalid, "rejected", "fix it")),
                pointer => Ok(match self.0.pointer(pointer) {
                    Some(value) => Provision::Value(Resolved {
                        value: value.clone(),
                        origin: TestOrigin::Values(pointer.to_owned()),
                    }),
                    None => Provision::Unbound,
                }),
            }
        }
    }

    fn marked(pointers: &[&str]) -> Vec<ResolvableField> {
        pointers
            .iter()
            .map(|pointer| ResolvableField {
                pointer: (*pointer).to_owned(),
                kind: "fromValues".to_owned(),
            })
            .collect()
    }

    fn resolve(document: Value, pointers: &[&str]) -> (ResolvedDocument<TestOrigin>, Vec<BindingIssue>) {
        let provider = Values(json!({"target": "prod", "nothing": null, "approval": {"required": true}}));
        let mut issues = Vec::new();
        let resolved = resolve_declared("spec", &document, &marked(pointers), &provider, &mut issues);
        (resolved, issues)
    }

    #[test]
    fn test_resolve_declared_substitutes_only_at_marked_pointers() {
        let document = json!({
            "target": {"fromValues": {"pointer": "/target"}},
            "nested": {"approval": {"fromValues": {"pointer": "/approval"}}},
            "unmarked": {"fromValues": {"pointer": "/target"}},
            "literal": "kept",
        });
        let (resolved, issues) = resolve(
            document.clone(),
            &["/target", "/nested/approval", "/literal", "/absent"],
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            resolved.value,
            json!({
                "target": "prod",
                "nested": {"approval": {"required": true}},
                "unmarked": document["unmarked"],
                "literal": "kept",
            })
        );
        assert_eq!(
            resolved.provenance,
            BTreeMap::from([
                (
                    "/nested/approval".to_owned(),
                    TestOrigin::Values("/approval".to_owned())
                ),
                ("/target".to_owned(), TestOrigin::Values("/target".to_owned())),
            ])
        );
    }

    #[test]
    fn test_resolve_declared_applies_the_missing_value_rules() {
        let document = json!({
            "defaulted": {"fromValues": {"pointer": "/missing", "default": {"strategy": "manual"}}},
            "nullDefaulted": {"fromValues": {"pointer": "/nothing", "default": 5}},
            "optional": {"fromValues": {"pointer": "/missing", "optional": true}},
            "nullOptional": {"fromValues": {"pointer": "/nothing", "optional": true}},
            "required": {"fromValues": {"pointer": "/missing"}},
        });
        let pointers = [
            "/defaulted",
            "/nullDefaulted",
            "/optional",
            "/nullOptional",
            "/required",
        ];
        let (resolved, issues) = resolve(document.clone(), &pointers);
        assert_eq!(
            resolved.value,
            json!({
                "defaulted": {"strategy": "manual"},
                "nullDefaulted": 5,
                "required": document["required"],
            })
        );
        assert_eq!(resolved.provenance["/defaulted"], TestOrigin::Default);
        assert_eq!(
            issues,
            [BindingIssue::new(
                "spec.required needs a value, but its fromValues source holds none; set a default, or optional: true to omit the field"
            )]
        );
    }

    #[test]
    fn test_resolve_declared_reports_invalid_clauses_and_passes_provider_issues_on() {
        let document = json!({
            "mixed": {"fromValues": {"pointer": "/target"}, "other": 1},
            "scalar": {"fromValues": "/target"},
            "nullDefault": {"fromValues": {"pointer": "/target", "default": null}},
            "both": {"fromValues": {"pointer": "/target", "default": 1, "optional": true}},
            "rejected": {"fromValues": {"pointer": "/rejected"}},
            "blocked": {"fromValues": {"pointer": "/blocked"}},
        });
        let pointers = ["/mixed", "/scalar", "/nullDefault", "/both", "/rejected", "/blocked"];
        let (resolved, issues) = resolve(document.clone(), &pointers);
        let messages = issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>();
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert!(messages[0].starts_with("spec.mixed mixes fromValues with other fields"));
        assert_eq!(messages[1], "spec.scalar.fromValues must be an object");
        assert!(
            messages[2].starts_with("spec.nullDefault.fromValues is not valid: default must not be null"),
            "{messages:?}"
        );
        assert!(messages[3].starts_with("spec.both.fromValues sets both default and optional"));
        assert_eq!(issues[4], BindingIssue::coded(Code::UnitInvalid, "rejected", "fix it"));
        // A failed or blocked clause stays in place, for the fail-closed check.
        assert_eq!(resolved.value, document);
        assert_eq!(resolved.blocked.len(), 1);
        assert_eq!(resolved.blocked[0].field, "spec.blocked");
        assert_eq!(resolved.blocked[0].pointer, "/blocked");
    }

    #[test]
    fn test_resolve_declared_refuses_a_marker_of_another_kind() {
        let document = json!({"includeCrds": {"fromInput": {"name": "crds"}}});
        let fields = [ResolvableField {
            pointer: "/includeCrds".to_owned(),
            kind: <FromInput as SourceKind>::FIELD.to_owned(),
        }];
        let mut issues = Vec::new();
        let resolved = resolve_declared::<FromValues, _>("spec", &document, &fields, &Values(json!({})), &mut issues);
        assert_eq!(resolved.value, document);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.starts_with(
                "spec.includeCrds is declared resolvable from fromInput, but this document resolves only fromValues"
            ),
            "{issues:?}"
        );
    }

    #[test]
    fn test_resolve_in_references_resolves_inside_each_reference_at_any_depth() {
        let document = json!({
            "variables": {
                "vpc": {"fromUnit": {"unit": "network", "output": "vpcId", "environment": {"fromValues": {"pointer": "/target"}}}},
                "list": [{"fromUnit": {"unit": "db", "output": "url", "environment": {"fromValues": {"pointer": "/missing", "optional": true}}}}],
                "local": {"fromUnit": {"unit": "db", "output": "url"}},
            },
            "data": {"fromUnit": {"unit": "db"}, "other": {"fromValues": {"pointer": "/target"}}},
        });
        let fields = marked(&["/environment"]);
        let provider = Values(json!({"target": "prod"}));
        let mut issues = Vec::new();
        let resolved =
            resolve_in_references::<FromValues, _>("spec", "", &document, "fromUnit", &fields, &provider, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            resolved.value["variables"]["vpc"]["fromUnit"]["environment"],
            json!("prod")
        );
        assert_eq!(
            resolved.value["variables"]["list"][0]["fromUnit"],
            json!({"unit": "db", "output": "url"})
        );
        // An object with other fields is data, not a reference.
        assert_eq!(resolved.value["data"], document["data"]);
        assert_eq!(
            resolved.provenance.keys().collect::<Vec<_>>(),
            ["/variables/vpc/fromUnit/environment"]
        );
    }

    #[test]
    fn test_reject_surviving_clauses_finds_clauses_at_any_depth() {
        let document = json!({
            "values": {"a": [{"fromValues": {"pointer": "/x"}}]},
            "target": {"fromValues": {"pointer": "/target"}},
            "data": {"fromValues": 1, "other": 2},
            "reference": {"fromUnit": {"unit": "network", "output": "vpcId"}},
        });
        let mut issues = Vec::new();
        reject_surviving_clauses("spec", &document, &["fromValues"], &mut issues);
        let messages = issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>();
        assert_eq!(
            messages,
            [
                "spec.target holds a fromValues clause where none resolves; fromValues is accepted only by the fields that declare it",
                "spec.values.a[0] holds a fromValues clause where none resolves; fromValues is accepted only by the fields that declare it",
            ]
        );
    }
}
