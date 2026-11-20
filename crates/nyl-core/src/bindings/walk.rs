//! The one walker of `$` value sources.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md).
//! [`resolve_document`] replaces every value source of a document that its
//! [`Context`] resolves, at any depth of its objects and arrays, checks the
//! ones it carries, and reports the ones it rejects. [`find_clauses`] walks
//! the same way without reading any source, for callers that need the
//! references of a document before any of them has a value, such as the
//! dependency graph. Both apply the same object rules:
//!
//! - An object whose only key is a known source is a value source.
//! - A known source next to other keys is an error: the source would replace
//!   them.
//! - Any other `$` key is an error in Nyl's typed resource fields
//!   ([`Namespace::Reserved`]); in free-form data it is data, except a likely
//!   slip ([`super::SLIPS`]).
//! - `$literal` yields its content verbatim, which is never walked.
//!
//! A source the context carries keeps its place, and its body is walked for
//! the sources that resolve here, such as `$environment` in the `environment`
//! of a `$unit` when its unit renders. The value an `$environment` clause
//! reads comes from the user's configuration, so it is walked too, with the
//! references it carries; every other source reads data, which is never
//! walked.

use serde_json::Value;

use super::{
    clause_key, holds_dollar_key, BindingIssue, Blocked, Clause, Context, Namespace, Origin, Provided, Providers,
    Provision, ResolvedDocument, Source, Treatment, SLIPS,
};

/// A document's context, and the contexts of parts of it, by JSON Pointer
/// relative to the document; `*` matches any one token. A part takes its
/// context where its pointer matches, such as an inline target's
/// `releaseInputs` inside a unit document.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub context: Context,
    pub within: &'a [(&'a str, Context)],
}

impl From<Context> for Scope<'static> {
    fn from(context: Context) -> Self {
        Self { context, within: &[] }
    }
}

impl Scope<'_> {
    /// The context at `relative`, the pointer of a node inside the document.
    fn context_at(&self, relative: &str, current: Context) -> Context {
        self.within
            .iter()
            .find(|(pattern, _)| matches_pattern(pattern, relative))
            .map_or(current, |(_, context)| *context)
    }
}

fn matches_pattern(pattern: &str, pointer: &str) -> bool {
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let pointer = pointer.split('/').collect::<Vec<_>>();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(pattern, token)| *pattern == "*" || pattern == token)
}

/// Where a node of the walk sits.
#[derive(Clone)]
struct At<'s> {
    /// The node in messages, such as `spec.variables.vpc_id`.
    field: String,
    /// The node's JSON Pointer, below the pointer the caller passes.
    pointer: String,
    /// The node's JSON Pointer inside the document, for [`Scope::within`].
    relative: String,
    context: Context,
    /// Inside a value an `$environment` clause read.
    environment_value: bool,
    scope: Scope<'s>,
}

impl<'s> At<'s> {
    fn root(field: &str, pointer: &str, scope: Scope<'s>) -> Self {
        Self {
            field: field.to_owned(),
            pointer: pointer.to_owned(),
            relative: String::new(),
            context: scope.context_at("", scope.context),
            environment_value: false,
            scope,
        }
    }

    fn key(&self, key: &str) -> Self {
        let relative = crate::json_pointer::child(&self.relative, key);
        Self {
            field: if self.field.is_empty() {
                key.to_owned()
            } else {
                format!("{}.{key}", self.field)
            },
            pointer: crate::json_pointer::child(&self.pointer, key),
            context: self.scope.context_at(&relative, self.context),
            relative,
            environment_value: self.environment_value,
            scope: self.scope,
        }
    }

    fn index(&self, index: usize) -> Self {
        let relative = format!("{}/{index}", self.relative);
        Self {
            field: format!("{}[{index}]", self.field),
            pointer: format!("{}/{index}", self.pointer),
            context: self.scope.context_at(&relative, self.context),
            relative,
            environment_value: self.environment_value,
            scope: self.scope,
        }
    }

    /// What this node's context does with `source`.
    fn treats(&self, source: Source) -> Treatment {
        if self.environment_value && source == Source::Environment {
            return Treatment::Reject;
        }
        self.context.treats(source)
    }

    fn misplaced(&self, source: Source) -> String {
        if self.environment_value && source == Source::Environment {
            return format!(
                "{} holds a $environment source inside a value an $environment source read; the environment's values cannot read its values, so write the value there",
                self.field
            );
        }
        self.context.misplaced(&self.field, source)
    }
}

/// What an object is to the walk.
enum Node {
    /// Data: walk its members.
    Data,
    /// A value source.
    Clause(Source),
    /// An error, already reported; leave it alone.
    Invalid,
}

/// Classify `fields` at `at`, reporting a source with siblings, an unknown
/// `$` key in a reserved namespace, and a slip.
fn classify(at: &At<'_>, fields: &serde_json::Map<String, Value>, issues: &mut Vec<String>) -> Node {
    match clause_key(fields) {
        Ok(Some(source)) => Node::Clause(source),
        Err(known) => {
            let others = fields
                .keys()
                .filter(|key| !known.contains(&key.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            issues.push(format!(
                "{} holds {} next to {}; a value source replaces the whole object, so it must be the object's only key (wrap the object in $literal to keep it as data)",
                at.field,
                known.join(" and "),
                if others.is_empty() {
                    "another source".to_owned()
                } else {
                    format!("other keys ({})", others.join(", "))
                }
            ));
            Node::Invalid
        }
        Ok(None) => {
            let mut valid = true;
            for key in fields.keys().filter(|key| key.starts_with('$')) {
                let field = at.key(key).field;
                if at.context.namespace() == Namespace::Reserved {
                    issues.push(format!(
                        "{field} is not a value source; `$` keys are reserved for value sources in Nyl's resource fields (write {{$literal: …}} to use the key as data)"
                    ));
                    valid = false;
                } else if let Some((_, meant)) = SLIPS.iter().find(|(slip, _)| slip == key) {
                    issues.push(format!(
                        "{field} is not a value source; did you mean {meant}? (write {{$literal: …}} to keep the key as data)"
                    ));
                    valid = false;
                }
            }
            if valid {
                Node::Data
            } else {
                Node::Invalid
            }
        }
    }
}

/// What to do with a node once it is walked.
enum After {
    Keep,
    /// Remove the node's key from its parent, as an `optional` clause asks.
    Remove,
}

struct Walk<'p, P: Providers> {
    providers: &'p P,
    provenance: std::collections::BTreeMap<String, P::Origin>,
    blocked: Vec<Blocked>,
    issues: Vec<BindingIssue>,
}

impl<P: Providers> Walk<'_, P> {
    fn issue(&mut self, message: String) {
        self.issues.push(BindingIssue::new(message));
    }

    fn walk(&mut self, at: &At<'_>, value: &mut Value) -> After {
        match value {
            Value::Object(fields) => {
                let mut messages = Vec::new();
                let node = classify(at, fields, &mut messages);
                for message in messages {
                    self.issue(message);
                }
                match node {
                    Node::Invalid => After::Keep,
                    Node::Data => {
                        let keys = fields.keys().cloned().collect::<Vec<_>>();
                        for key in keys {
                            let child = fields.get_mut(&key).expect("key of the object");
                            if let After::Remove = self.walk(&at.key(&key), child) {
                                fields.remove(&key);
                            }
                        }
                        After::Keep
                    }
                    Node::Clause(source) => self.clause(at, source, value),
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    if let After::Remove = self.walk(&at.index(index), item) {
                        self.issue(format!(
                            "{}[{index}]: optional removes a field, but a list item is no field; give the clause a default",
                            at.field
                        ));
                    }
                }
                After::Keep
            }
            _ => After::Keep,
        }
    }

    fn clause(&mut self, at: &At<'_>, source: Source, value: &mut Value) -> After {
        let key = source.key();
        let treatment = at.treats(source);
        if treatment == Treatment::Reject {
            let message = at.misplaced(source);
            self.issue(message);
            return After::Keep;
        }
        if source == Source::Literal {
            let content = value[key].clone();
            let unwrap = treatment == Treatment::Resolve
                || (matches!(at.context, Context::UnitRender | Context::UnitTargetInputs)
                    && !holds_dollar_key(&content));
            if unwrap {
                *value = content;
            }
            return After::Keep;
        }
        // The body's own sources first, such as `$environment` in a `$unit`.
        let before = self.issues.len();
        let body_at = at.key(key);
        if let Some(Value::Object(fields)) = value.get_mut(key) {
            let keys = fields.keys().cloned().collect::<Vec<_>>();
            for member in keys {
                let child = fields.get_mut(&member).expect("key of the object");
                if let After::Remove = self.walk(&body_at.key(&member), child) {
                    fields.remove(&member);
                }
            }
        }
        if self.issues.len() > before {
            return After::Keep;
        }
        let clause = match Clause::parse(&at.field, source, &value[key]) {
            Ok(clause) => clause,
            Err(message) => {
                self.issue(message);
                return After::Keep;
            }
        };
        if treatment == Treatment::Carry {
            return After::Keep;
        }
        let provided = provide(&at.field, &clause, self.providers);
        match provided {
            Ok(Provision::Value(resolved)) if !(resolved.value.is_null() && source == Source::Environment) => {
                let mut resolved_value = resolved.value;
                if source == Source::Environment {
                    self.walk_configuration(at, &mut resolved_value);
                }
                self.provenance.insert(at.pointer.clone(), resolved.origin);
                *value = resolved_value;
                After::Keep
            }
            Ok(Provision::Value(_) | Provision::Unbound) => self.missing(at, &clause, value),
            Ok(Provision::Blocked(reason)) => {
                self.blocked.push(Blocked {
                    field: at.field.clone(),
                    pointer: at.pointer.clone(),
                    reason,
                });
                After::Keep
            }
            Err(issue) => {
                self.issues.push(issue);
                After::Keep
            }
        }
    }

    /// Walk a value read from the user's configuration, which carries its
    /// references, where it lands.
    fn walk_configuration(&mut self, at: &At<'_>, value: &mut Value) {
        let mut inner = at.clone();
        inner.environment_value = true;
        if let After::Remove = self.walk(&inner, value) {
            *value = Value::Null;
        }
    }

    /// The missing-value rules: `default`, then `optional`, then an error.
    fn missing(&mut self, at: &At<'_>, clause: &Clause, value: &mut Value) -> After {
        if let Some(default) = clause.default_value() {
            let mut default = default.clone();
            self.walk_configuration(at, &mut default);
            self.provenance.insert(at.pointer.clone(), P::Origin::from_default());
            *value = default;
            return After::Keep;
        }
        if clause.optional() {
            if at.pointer.is_empty() || at.relative.is_empty() {
                self.issue(format!(
                    "{}: optional removes a field, but the whole document is no field; give the clause a default",
                    at.field
                ));
                return After::Keep;
            }
            return After::Remove;
        }
        let issue = self.providers.missing(&at.field, clause).unwrap_or_else(|| {
            BindingIssue::new(format!(
                "{} needs a value, but its {} source holds none{}",
                at.field,
                clause.source().key(),
                if clause.source() == Source::Environment {
                    "; set a default, or optional: true to omit the field"
                } else {
                    ""
                }
            ))
        });
        self.issues.push(issue);
        After::Keep
    }
}

/// Read one clause through its provider; `$literal` needs none.
pub fn provide<P: Providers>(field: &str, clause: &Clause, providers: &P) -> Provided<P::Origin> {
    match clause {
        Clause::Environment(source) => providers.environment(field, source),
        Clause::Input(source) => providers.input(field, source),
        Clause::Parameter(source) => providers.parameter(field, source),
        Clause::Unit(source) => providers.unit(field, source),
        Clause::Promotion(source) => providers.promotion(field, source),
        Clause::File(source) => providers.file(field, source),
        Clause::Git(source) => providers.git(field, source),
        Clause::Publication(source) => providers.publication(field, source),
        Clause::Literal(value) => Ok(Provision::Value(super::Resolved {
            value: value.clone(),
            origin: P::Origin::from_value(),
        })),
    }
}

/// Replace every value source in `document` that its context resolves, at
/// any depth of its objects and arrays, by the value it names.
///
/// `field` and `pointer` locate `document` itself, such as `spec` and `""`.
/// A blocked or failed source keeps its place; a carried one keeps its place
/// once checked. A source whose value is missing follows the missing-value
/// rules: `default`, `optional`, otherwise an issue. Problems are appended to
/// `issues`.
pub fn resolve_document<P: Providers>(
    field: &str,
    pointer: &str,
    document: &Value,
    scope: Scope<'_>,
    providers: &P,
    issues: &mut Vec<BindingIssue>,
) -> ResolvedDocument<P::Origin> {
    let mut value = document.clone();
    let mut walk = Walk {
        providers,
        provenance: std::collections::BTreeMap::new(),
        blocked: Vec::new(),
        issues: Vec::new(),
    };
    let at = At::root(field, pointer, scope);
    if let After::Remove = walk.walk(&at, &mut value) {
        walk.issue(format!(
            "{field}: optional removes a field, but the whole document is no field; give the clause a default"
        ));
    }
    issues.extend(walk.issues);
    ResolvedDocument {
        value,
        provenance: walk.provenance,
        blocked: walk.blocked,
    }
}

/// One value source of a document, found without reading it.
#[derive(Debug, Clone, PartialEq)]
pub struct FoundClause {
    /// Location in messages, such as `spec.variables.vpc_id`.
    pub field: String,
    /// JSON Pointer of the source object, below the pointer the caller passes.
    pub pointer: String,
    pub clause: Clause,
}

/// Every value source of `document` that its context resolves or carries,
/// parsed, without reading any; `$literal` content is never entered. A
/// malformed or misplaced source is an issue naming its location, so a
/// reference is never silently dropped.
pub fn find_clauses(
    field: &str,
    pointer: &str,
    document: &Value,
    scope: Scope<'_>,
) -> std::result::Result<Vec<FoundClause>, Vec<String>> {
    let mut found = Vec::new();
    let mut issues = Vec::new();
    find(&At::root(field, pointer, scope), document, &mut found, &mut issues);
    if issues.is_empty() {
        Ok(found)
    } else {
        Err(issues)
    }
}

fn find(at: &At<'_>, value: &Value, found: &mut Vec<FoundClause>, issues: &mut Vec<String>) {
    match value {
        Value::Object(fields) => match classify(at, fields, issues) {
            Node::Invalid => {}
            Node::Data => {
                for (key, child) in fields {
                    find(&at.key(key), child, found, issues);
                }
            }
            Node::Clause(source) => {
                if at.treats(source) == Treatment::Reject {
                    issues.push(at.misplaced(source));
                    return;
                }
                if source == Source::Literal {
                    return;
                }
                let before = issues.len();
                if let Some(Value::Object(body)) = fields.get(source.key()) {
                    let body_at = at.key(source.key());
                    for (key, child) in body {
                        find(&body_at.key(key), child, found, issues);
                    }
                }
                if issues.len() > before {
                    return;
                }
                match Clause::parse(&at.field, source, &fields[source.key()]) {
                    Ok(clause) => found.push(FoundClause {
                        field: at.field.clone(),
                        pointer: at.pointer.clone(),
                        clause,
                    }),
                    Err(message) => issues.push(message),
                }
            }
        },
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                find(&at.index(index), item, found, issues);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::bindings::tests::{TestOrigin, TestProviders};

    fn resolve(
        document: Value,
        scope: Scope<'_>,
        providers: &TestProviders,
    ) -> (ResolvedDocument<TestOrigin>, Vec<String>) {
        let mut issues = Vec::new();
        let resolved = resolve_document("spec", "", &document, scope, providers, &mut issues);
        (resolved, issues.into_iter().map(|issue| issue.message).collect())
    }

    fn values(values: Value) -> TestProviders {
        TestProviders {
            values,
            ..TestProviders::default()
        }
    }

    #[test]
    fn test_unit_render_resolves_environment_and_carries_run_sources() {
        let document = json!({
            "target": {"$environment": "/target"},
            "variables": {
                "vpc": {"$unit": {"unit": "network", "output": "vpcId", "environment": {"$environment": "/networkFrom"}}},
                "local": {"$unit": {"unit": "db", "output": "url", "environment": {"$environment": {"pointer": "/missing", "optional": true}}}},
                "image": {"$promotion": {"value": "webImage"}},
                "schema": {"$ref": "#/defs/x"},
            },
        });
        let (resolved, issues) = resolve(
            document,
            Context::UnitRender.into(),
            &values(json!({"target": "prod", "networkFrom": "infra"})),
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            resolved.value,
            json!({
                "target": "prod",
                "variables": {
                    "vpc": {"$unit": {"unit": "network", "output": "vpcId", "environment": "infra"}},
                    "local": {"$unit": {"unit": "db", "output": "url"}},
                    "image": {"$promotion": {"value": "webImage"}},
                    "schema": {"$ref": "#/defs/x"},
                },
            })
        );
        assert_eq!(
            resolved.provenance.keys().collect::<Vec<_>>(),
            ["/target", "/variables/vpc/$unit/environment"]
        );
    }

    #[test]
    fn test_missing_values_take_the_default_or_remove_the_field() {
        let document = json!({
            "defaulted": {"$environment": {"pointer": "/missing", "default": {"strategy": "manual"}}},
            "nullDefaulted": {"$environment": {"pointer": "/nothing", "default": 5}},
            "optional": {"$environment": {"pointer": "/missing", "optional": true}},
            "nullOptional": {"$environment": {"pointer": "/nothing", "optional": true}},
            "required": {"$environment": "/missing"},
            "list": [{"$environment": {"pointer": "/missing", "optional": true}}],
        });
        let (resolved, issues) = resolve(
            document.clone(),
            Context::UnitRender.into(),
            &values(json!({"nothing": null})),
        );
        assert_eq!(
            resolved.value,
            json!({
                "defaulted": {"strategy": "manual"},
                "nullDefaulted": 5,
                "required": document["required"],
                "list": document["list"],
            })
        );
        assert_eq!(resolved.provenance["/defaulted"], TestOrigin::Default);
        assert_eq!(
            issues,
            [
                "spec.list[0]: optional removes a field, but a list item is no field; give the clause a default",
                "spec.required needs a value, but its $environment source holds none; set a default, or optional: true to omit the field",
            ]
        );
    }

    /// Catches a source written where it cannot resolve reaching a reader as
    /// data: each context rejects the sources it neither resolves nor carries,
    /// with where they work.
    #[test]
    fn test_sources_outside_their_context_are_errors_that_teach_the_fix() {
        let (_, issues) = resolve(
            json!({"includeCrds": {"$unit": {"unit": "crds", "output": "enabled"}}, "lookup": {"$input": "lookup"}}),
            Context::ReleaseBundle.into(),
            &TestProviders::default(),
        );
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues[0].ends_with("declare a Release input and bind it on the target with $unit (see https://niklasrosenstein.github.io/nyl/reference/value-sources/)"),
            "{issues:?}"
        );
        assert_eq!(issues[1], "spec.lookup.$input is not supported here");

        let (_, issues) = resolve(
            json!({"image": {"$input": "image"}, "path": {"$file": {"path": "x.yaml"}}}),
            Context::UnitRender.into(),
            &TestProviders::default(),
        );
        assert!(issues[0].starts_with("spec.image holds a $input source"), "{issues:?}");
        assert!(
            issues[0].contains("read the environment's values with $environment"),
            "{issues:?}"
        );
        assert!(issues[1].contains("bind it to a Release input"), "{issues:?}");
    }

    #[test]
    fn test_an_inline_targets_bindings_carry_render_sources() {
        let document = json!({"target": {"inline": {"releaseInputs": {"g/r": {
            "config": {"$file": {"path": "config.yaml"}},
            "image": {"$unit": {"unit": "build", "artifact": "image"}},
        }}}}});
        let within = [("/target/inline/releaseInputs", Context::UnitTargetInputs)];
        let scope = Scope {
            context: Context::UnitRender,
            within: &within,
        };
        let (resolved, issues) = resolve(document.clone(), scope, &TestProviders::default());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved.value, document);
    }

    #[test]
    fn test_literal_keeps_its_content_and_slips_are_errors() {
        let document = json!({
            "plain": {"$literal": {"a": 1}},
            "shaped": {"$literal": {"$unit": "data"}},
            "slip": {"$values": "/x"},
            "foreign": {"$schema": "x"},
        });
        let (resolved, issues) = resolve(document, Context::UnitRender.into(), &TestProviders::default());
        assert_eq!(resolved.value["plain"], json!({"a": 1}));
        // Unwrapped when the run resolves the unit, so it never reads as a source.
        assert_eq!(resolved.value["shaped"], json!({"$literal": {"$unit": "data"}}));
        assert_eq!(resolved.value["foreign"], json!({"$schema": "x"}));
        assert_eq!(
            issues,
            ["spec.slip.$values is not a value source; did you mean $environment? (write {$literal: …} to keep the key as data)"]
        );

        let (resolved, issues) = resolve(
            json!({"shaped": {"$literal": {"$unit": "data"}}}),
            Context::UnitExecution.into(),
            &TestProviders::default(),
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved.value["shaped"], json!({"$unit": "data"}));
    }

    #[test]
    fn test_reserved_namespace_rejects_unknown_dollar_keys() {
        let (_, issues) = resolve(
            json!({"chart": {"$ref": "x"}}),
            Context::ReleaseBundle.into(),
            &TestProviders::default(),
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].starts_with("spec.chart.$ref is not a value source; `$` keys are reserved"));
    }

    /// The value `$environment` reads comes from the user's configuration, so
    /// the references it carries land with it; a source that reads data does
    /// not.
    #[test]
    fn test_environment_values_carry_their_references() {
        let providers = TestProviders {
            values: json!({"target": {"inline": {"releaseInputs": {"g/r": {"image": {"$unit": {"unit": "build", "artifact": "image"}}}}}}, "self": {"$environment": "/target"}}),
            units: BTreeMap::from([("build", json!({"$unit": "data"}))]),
            ..TestProviders::default()
        };
        let (resolved, issues) = resolve(
            json!({"target": {"$environment": "/target"}}),
            Context::UnitRender.into(),
            &providers,
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved.value, json!({"target": providers.values["target"]}));

        let (_, issues) = resolve(
            json!({"x": {"$environment": "/self"}}),
            Context::UnitRender.into(),
            &providers,
        );
        assert!(
            issues[0].contains("the environment's values cannot read its values"),
            "{issues:?}"
        );

        let (resolved, issues) = resolve(
            json!({"image": {"$unit": {"unit": "build", "artifact": "image"}}}),
            Context::UnitExecution.into(),
            &providers,
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved.value["image"], json!({"$unit": "data"}));
    }

    #[test]
    fn test_find_clauses_skips_literals_and_reports_misplaced_sources() {
        let document = json!({
            "a": {"$unit": {"unit": "network", "output": "id"}},
            "b": [{"$promotion": {"value": "image"}}],
            "c": {"$literal": {"$unit": {"unit": "x", "output": "y"}}},
        });
        let found = find_clauses("spec", "", &document, Context::UnitExecution.into()).unwrap();
        assert_eq!(
            found.iter().map(|found| found.pointer.as_str()).collect::<Vec<_>>(),
            ["/a", "/b/0"]
        );
        let issues = find_clauses(
            "spec",
            "",
            &json!({"a": {"$input": "x"}}),
            Context::UnitExecution.into(),
        )
        .unwrap_err();
        assert!(issues[0].starts_with("spec.a holds a $input source"), "{issues:?}");
    }
}
