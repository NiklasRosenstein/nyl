//! One resolver for value sources, typed bindings, and references.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! [Bindings](../../../../design/release-inputs.md#bindings) and the
//! effective-value rule, and the orchestration core contract's References.
//! Unit documents, EnvironmentTemplate values, Release bundles, Release-input
//! bindings, PromotionPath selectors, and `nyl get` value forms share it, so
//! they cannot disagree on parsing, precedence, missing values, type checks,
//! selection, or digests.
//!
//! - A value source is an object whose only key is a known `$` source
//!   ([`Source`], parsed by [`Clause::parse`]). Its [`Context`] decides
//!   whether it resolves, is carried to where the document lands, or is an
//!   error that teaches the fix.
//! - [`resolve_document`] replaces the sources of a document at any depth and
//!   records provenance per JSON Pointer; [`find_clauses`] lists them without
//!   reading any, such as the unit references of a stored spec.
//! - A declaration is typed ([`InputDeclaration`]); [`resolve`] fills declared
//!   slots: the effective value is the override, otherwise the binding,
//!   otherwise the declared default. A source that holds no value yet leaves
//!   the slot unbound, so the default applies.
//! - A provider may report a source as [`Provision::Blocked`], such as a
//!   producer without a current receipt; blocked values are listed with their
//!   location instead of failing resolution.
//! - Every problem is collected as a [`BindingIssue`], so a caller reports
//!   them together; a provider's code and fix reach the caller unchanged.
//!
//! Each source is one [`Providers`] method. The resolver itself performs no
//! effects: providers read what their caller gathered, whether files and Git
//! for rendering, the environment's values, or a state snapshot for
//! orchestration.

mod binding;
mod clauses;
mod declaration;
mod issue;
mod references;
mod schema;
mod walk;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::orchestration::blocked::Blocker;

pub use binding::{
    FileInputSource, GitInputSource, InputBinding, PromotionInputSource, PublicationInputSource, UnitInputSource,
};
pub use clauses::{
    clause_key, holds_dollar_key, literal_field_issues, slip, sources_in, suggest, Clause, Context, InputSource,
    Namespace, ParameterSource, Source, Treatment, ValuesSource, LITERAL_ONLY_FIELDS, VALUE_SOURCES_DOCS,
};
pub use declaration::{json_type_name, validate_declarations_at, validate_input_name, InputDeclaration, InputType};
pub use issue::{BindingIssue, Coded};
pub use references::{unit_references, UnitReference};
pub use schema::{
    offer_sources, reserved_key_issues, schema_alternatives, schema_resolve, schema_step, source_variants, SchemaStep,
};
pub use walk::{find_clauses, provide, resolve_document, FoundClause, Scope};

/// An effective value and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved<O> {
    pub value: Value,
    pub origin: O,
}

/// What a provider found for one binding.
#[derive(Debug, Clone, PartialEq)]
pub enum Provision<O> {
    /// The binding's value.
    Value(Resolved<O>),
    /// The source exists but holds no value yet; a declared default applies.
    Unbound,
    /// The value cannot be read yet, for the given reason, such as a producer
    /// without a current receipt. Resolution continues around it.
    Blocked(Blocker),
}

/// A provider's answer, or the issue that makes the binding invalid. The issue
/// carries the provider's own stable code and fix, which resolution passes on
/// unchanged.
pub type Provided<O> = std::result::Result<Provision<O>, BindingIssue>;

/// A value that could not be read yet, and where it is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Location in messages, such as `spec.variables.vpc_id`.
    pub field: String,
    /// JSON Pointer of the value: for [`resolve_document`], below the pointer
    /// the caller passes, such as `/variables/vpc_id`; for [`resolve`], the
    /// slot name, such as `/image`.
    pub pointer: String,
    pub reason: Blocker,
}

/// The provenance a caller records for each value.
pub trait Origin {
    /// The declared default.
    fn from_default() -> Self;
    /// A caller-supplied override, such as a direct command's `--input`.
    fn from_override() -> Self;
    /// A literal binding, or a `$literal` source.
    fn from_value() -> Self;
    /// Names the value's source in messages about `field`.
    fn describe(&self, field: &str) -> String;
}

/// Reads each value source. A source a caller does not read keeps the
/// default method, which rejects it; the walker rejects sources the context
/// does not resolve before any provider is asked.
pub trait Providers {
    type Origin: Origin;

    fn environment(&self, field: &str, _source: &ValuesSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Environment))
    }

    fn input(&self, field: &str, _source: &InputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Input))
    }

    fn parameter(&self, field: &str, _source: &ParameterSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Parameter))
    }

    fn file(&self, field: &str, _source: &FileInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::File))
    }

    fn git(&self, field: &str, _source: &GitInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Git))
    }

    fn publication(&self, field: &str, _source: &PublicationInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Publication))
    }

    fn unit(&self, field: &str, _source: &UnitInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Unit))
    }

    fn promotion(&self, field: &str, _source: &PromotionInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, Source::Promotion))
    }

    /// The issue for a source at `field` that holds no value and has no
    /// `default` or `optional`, when the provider can say more than the
    /// generic message, such as the keys that do exist.
    fn missing(&self, _field: &str, _clause: &Clause) -> Option<BindingIssue> {
        None
    }

    /// Why a required slot whose binding left it unbound is missing, when the
    /// provider knows more than the generic message. `requirement` reads like
    /// "Release platform/web requires input \"image\"".
    fn unbound(&self, _field: &str, _clause: &Clause, _requirement: &str) -> Option<String> {
        None
    }
}

fn unsupported(field: &str, source: Source) -> BindingIssue {
    BindingIssue::new(format!("{field}.{} is not supported here", source.key()))
}

/// Resolve one Release-input binding at `field`. A binding that is one source
/// reads it through its provider, so a source that holds no value leaves the
/// slot unbound; a literal is the value, with any sources inside it resolved.
pub fn resolve_binding<P: Providers>(field: &str, binding: &InputBinding, providers: &P) -> Provided<P::Origin> {
    if let Some(fields) = binding.value().as_object() {
        if let Ok(Some(source)) = clause_key(fields) {
            let clause = Clause::parse(field, source, &fields[source.key()]).map_err(BindingIssue::new)?;
            return provide(field, &clause, providers);
        }
    }
    let mut issues = Vec::new();
    let resolved = resolve_document(
        field,
        "",
        binding.value(),
        Context::ReleaseInputs.into(),
        providers,
        &mut issues,
    );
    if let Some(first) = issues.first() {
        let mut issue = first.clone();
        issue.message = issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(issue);
    }
    if let Some(blocked) = resolved.blocked.into_iter().next() {
        return Ok(Provision::Blocked(blocked.reason));
    }
    Ok(Provision::Value(Resolved {
        value: resolved.value,
        origin: P::Origin::from_value(),
    }))
}

/// Resolved declared slots.
#[derive(Debug, Clone, PartialEq)]
pub struct Slots<O> {
    /// Slots with an effective value. Failed and blocked slots are absent.
    pub values: BTreeMap<String, Resolved<O>>,
    pub blocked: Vec<Blocked>,
}

impl<O> Default for Slots<O> {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            blocked: Vec::new(),
        }
    }
}

/// Resolve every declared slot of `subject`, such as `"Release platform/web"`.
///
/// `field_prefix` names the bindings' location, such as
/// `spec.releaseInputs."platform/web"`. Problems are appended to `issues`.
pub fn resolve<P: Providers>(
    field_prefix: &str,
    subject: &str,
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
    overrides: &BTreeMap<String, Value>,
    providers: &P,
    issues: &mut Vec<BindingIssue>,
) -> Slots<P::Origin> {
    let mut slots = Slots::default();
    for (name, declaration) in declarations {
        let field = format!("{field_prefix}.{name}");
        let binding = bindings.and_then(|bindings| bindings.get(name));
        let provision = if let Some(value) = overrides.get(name) {
            Ok(Provision::Value(Resolved {
                value: value.clone(),
                origin: P::Origin::from_override(),
            }))
        } else {
            binding.map_or(Ok(Provision::Unbound), |binding| {
                resolve_binding(&field, binding, providers)
            })
        };
        let input = match provision {
            Ok(Provision::Value(input)) => input,
            Ok(Provision::Unbound) => {
                if let Some(value) = &declaration.default {
                    Resolved {
                        value: value.clone(),
                        origin: P::Origin::from_default(),
                    }
                } else {
                    let description = declaration
                        .description
                        .as_deref()
                        .map(|description| format!(" ({description})"))
                        .unwrap_or_default();
                    let requirement = format!("{subject} requires input {name:?}{description}");
                    issues.push(BindingIssue::new(
                        binding
                            .and_then(InputBinding::clause)
                            .and_then(|clause| providers.unbound(&field, &clause, &requirement))
                            .unwrap_or_else(|| {
                                format!(
                                    "{requirement}, but it has no binding and no default; bind it in {field_prefix}"
                                )
                            }),
                    ));
                    continue;
                }
            }
            Ok(Provision::Blocked(reason)) => {
                slots.blocked.push(Blocked {
                    pointer: crate::json_pointer::child("", name),
                    field,
                    reason,
                });
                continue;
            }
            Err(issue) => {
                issues.push(issue);
                continue;
            }
        };
        match declaration.check(&input.value) {
            Ok(()) => {
                slots.values.insert(name.clone(), input);
            }
            Err(reason) => issues.push(BindingIssue::new(format!(
                "{} for input {name:?} of {subject} is invalid: {reason}",
                input.origin.describe(&field)
            ))),
        }
    }
    slots
}

/// A binding must name a slot its subject declares.
pub fn undeclared_binding_issues(
    field_prefix: &str,
    subject: &str,
    bindings: &BTreeMap<String, InputBinding>,
    declarations: &BTreeMap<String, InputDeclaration>,
) -> Vec<String> {
    bindings
        .keys()
        .filter(|name| !declarations.contains_key(*name))
        .map(|name| {
            format!(
                "{field_prefix}.{name} binds an input {subject} does not declare; declared inputs: {}",
                declarations.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

/// A document with its reference objects replaced by their values.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDocument<O> {
    /// The document; a blocked or failed reference keeps its reference object.
    pub value: Value,
    /// Where each replaced value came from, by JSON Pointer.
    pub provenance: BTreeMap<String, O>,
    pub blocked: Vec<Blocked>,
}

/// Select `pointer` inside `document`.
pub fn select(document: &Value, pointer: &str) -> std::result::Result<Value, String> {
    crate::json_pointer::resolve(document, pointer)
        .cloned()
        .ok_or_else(|| format!("has no value at JSON Pointer {pointer:?}"))
}

/// Digest of a value's canonical JSON form, recorded as its provenance.
pub fn value_digest(value: &Value) -> crate::Result<String> {
    Ok(crate::digest::sha256_hex(&crate::digest::canonical_json_bytes(value)?))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::orchestration::blocked::ProducerRef;
    use crate::orchestration::codes::Code;
    use crate::orchestration::ids::UnitName;

    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum TestOrigin {
        Default,
        Override,
        Value,
        File(String),
        Unit(String),
        Values(String),
    }

    impl Origin for TestOrigin {
        fn from_default() -> Self {
            Self::Default
        }
        fn from_override() -> Self {
            Self::Override
        }
        fn from_value() -> Self {
            Self::Value
        }
        fn describe(&self, field: &str) -> String {
            format!("{field} ({self:?})")
        }
    }

    /// Files map a path to its document; a missing path holds no value yet.
    /// Units map a name to its output; a missing unit has no current receipt.
    /// `values` are the environment's values.
    #[derive(Default)]
    pub(super) struct TestProviders {
        pub files: BTreeMap<&'static str, Value>,
        pub units: BTreeMap<&'static str, Value>,
        /// Units whose reference the provider rejects with its own code and fix.
        pub rejected: BTreeMap<&'static str, BindingIssue>,
        pub values: Value,
    }

    impl Providers for TestProviders {
        type Origin = TestOrigin;

        fn environment(&self, _field: &str, source: &ValuesSource) -> Provided<TestOrigin> {
            Ok(match self.values.pointer(&source.pointer) {
                Some(value) => Provision::Value(Resolved {
                    value: value.clone(),
                    origin: TestOrigin::Values(source.pointer.clone()),
                }),
                None => Provision::Unbound,
            })
        }

        fn file(&self, _field: &str, source: &FileInputSource) -> Provided<TestOrigin> {
            Ok(match self.files.get(source.path.as_str()) {
                Some(document) => Provision::Value(Resolved {
                    value: select(document, &source.pointer).unwrap(),
                    origin: TestOrigin::File(source.path.clone()),
                }),
                None => Provision::Unbound,
            })
        }

        fn unit(&self, _field: &str, source: &UnitInputSource) -> Provided<TestOrigin> {
            if let Some(issue) = self.rejected.get(source.unit.as_str()) {
                return Err(issue.clone());
            }
            Ok(match self.units.get(source.unit.as_str()) {
                Some(output) => Provision::Value(Resolved {
                    value: output.clone(),
                    origin: TestOrigin::Unit(source.unit.clone()),
                }),
                None => Provision::Blocked(Blocker::no_current_receipt(ProducerRef::local(
                    UnitName::new(&source.unit).unwrap(),
                ))),
            })
        }

        fn unbound(&self, field: &str, clause: &Clause, requirement: &str) -> Option<String> {
            match clause {
                Clause::File(source) => Some(format!(
                    "{requirement}, but {field} file {} does not exist yet",
                    source.path
                )),
                _ => None,
            }
        }
    }

    fn declarations(value: Value) -> BTreeMap<String, InputDeclaration> {
        serde_json::from_value(value).unwrap()
    }

    fn bindings(value: Value) -> BTreeMap<String, InputBinding> {
        serde_json::from_value(value).unwrap()
    }

    fn resolve_all(
        declared: &BTreeMap<String, InputDeclaration>,
        bound: Option<&BTreeMap<String, InputBinding>>,
        overrides: &BTreeMap<String, Value>,
        providers: &TestProviders,
    ) -> (Slots<TestOrigin>, Vec<String>) {
        let mut issues = Vec::new();
        let slots = resolve(
            "bindings",
            "Subject s",
            declared,
            bound,
            overrides,
            providers,
            &mut issues,
        );
        (slots, messages(&issues))
    }

    fn messages(issues: &[BindingIssue]) -> Vec<String> {
        issues.iter().map(|issue| issue.message.clone()).collect()
    }

    #[test]
    fn test_resolve_prefers_override_then_binding_then_default() {
        let declared = declarations(json!({
            "a": {"type": "integer", "default": 1},
            "b": {"type": "integer", "default": 1},
            "c": {"type": "integer", "default": 1},
        }));
        let bound = bindings(json!({"a": 2, "b": 2}));
        let overrides = BTreeMap::from([("a".to_owned(), json!(3))]);
        let (slots, issues) = resolve_all(&declared, Some(&bound), &overrides, &TestProviders::default());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(slots.values["a"].value, json!(3));
        assert_eq!(slots.values["a"].origin, TestOrigin::Override);
        assert_eq!(slots.values["b"].value, json!(2));
        assert_eq!(slots.values["b"].origin, TestOrigin::Value);
        assert_eq!(slots.values["c"].origin, TestOrigin::Default);
    }

    /// A bare literal is the value, sources inside it resolve, and `$literal`
    /// keeps a literal that looks like a source.
    #[test]
    fn test_resolve_reads_literals_and_sources_inside_them() {
        let declared = declarations(json!({
            "settings": {"type": "object"},
            "shaped": {"type": "object"},
        }));
        let bound = bindings(json!({
            "settings": {"port": {"$fromFile": {"path": "ports.json", "pointer": "/web"}}, "tls": true},
            "shaped": {"$literal": {"$fromFile": "not a source"}},
        }));
        let providers = TestProviders {
            files: BTreeMap::from([("ports.json", json!({"web": 8080}))]),
            ..TestProviders::default()
        };
        let (slots, issues) = resolve_all(&declared, Some(&bound), &BTreeMap::new(), &providers);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(slots.values["settings"].value, json!({"port": 8080, "tls": true}));
        assert_eq!(slots.values["shaped"].value, json!({"$fromFile": "not a source"}));
    }

    #[test]
    fn test_resolve_falls_back_to_the_default_when_a_source_holds_no_value() {
        let declared = declarations(json!({
            "tag": {"type": "string", "default": "none"},
            "image": {"type": "string"},
        }));
        let bound = bindings(json!({
            "tag": {"$fromFile": {"path": "missing.json"}},
            "image": {"$fromFile": {"path": "missing.json"}},
        }));
        let (slots, issues) = resolve_all(&declared, Some(&bound), &BTreeMap::new(), &TestProviders::default());
        assert_eq!(slots.values["tag"].origin, TestOrigin::Default);
        assert_eq!(
            issues,
            [r#"Subject s requires input "image", but bindings.image file missing.json does not exist yet"#]
        );
    }

    #[test]
    fn test_resolve_collects_every_problem_and_lists_blocked_slots() {
        let declared = declarations(json!({
            "name": {"type": "string", "description": "Service name"},
            "port": {"type": "integer"},
            "sha": {"type": "string"},
            "tier": {"type": "string", "enum": ["small"]},
            "vpc": {"type": "string"},
        }));
        let bound = bindings(json!({
            "port": {"$fromFile": {"path": "ports.json", "pointer": "/web"}},
            "sha": {"$fromGit": {"repository": {"repoURL": "https://git.example.com/x.git"}, "revision": "main", "commit": "a".repeat(40), "path": "x.json"}},
            "tier": "large",
            "vpc": {"$fromUnit": {"unit": "network", "output": "vpcId"}},
        }));
        let providers = TestProviders {
            files: BTreeMap::from([("ports.json", json!({"web": "8080"}))]),
            ..TestProviders::default()
        };
        let (slots, issues) = resolve_all(&declared, Some(&bound), &BTreeMap::new(), &providers);
        assert!(slots.values.is_empty());
        assert_eq!(
            issues,
            [
                r#"Subject s requires input "name" (Service name), but it has no binding and no default; bind it in bindings"#,
                r#"bindings.port (File("ports.json")) for input "port" of Subject s is invalid: expected integer, got string"#,
                "bindings.sha.$fromGit is not supported here",
                r#"bindings.tier (Value) for input "tier" of Subject s is invalid: "large" is not one of the allowed values ["small"]"#,
            ]
        );
        assert_eq!(
            slots.blocked,
            [Blocked {
                field: "bindings.vpc".to_owned(),
                pointer: "/vpc".to_owned(),
                reason: Blocker::no_current_receipt(ProducerRef::local(UnitName::new("network").unwrap())),
            }]
        );
    }

    #[test]
    fn test_undeclared_binding_issues_name_the_declared_slots() {
        let declared = declarations(json!({"image": {"type": "string"}}));
        let bound = bindings(json!({"imgae": "x"}));
        assert_eq!(
            undeclared_binding_issues("bindings", "Subject s", &bound, &declared),
            ["bindings.imgae binds an input Subject s does not declare; declared inputs: image"]
        );
    }

    /// Catches a provider's stable code and fix being dropped on the way to
    /// the caller, which would force callers to recover them from the message
    /// text: both resolution entry points hand each issue over unchanged, in
    /// the order found, beside issues the resolver raises itself, which carry
    /// neither.
    #[test]
    fn test_a_providers_code_and_fix_reach_the_caller_unchanged() {
        let rejected = BindingIssue::coded(
            Code::AttestationUndeclared,
            "bindings.vpc requires attestation \"smoke\" of unit network",
            "require an attestation the producer declares",
        );
        let providers = TestProviders {
            rejected: BTreeMap::from([("network", rejected.clone())]),
            ..TestProviders::default()
        };

        let declared = declarations(json!({"vpc": {"type": "string"}, "port": {"type": "integer"}}));
        let bound = bindings(json!({
            "vpc": {"$fromUnit": {"unit": "network", "output": "vpcId"}},
            "port": {"$fromGit": {"repository": {"repoURL": "https://git.example.com/x.git"}, "revision": "main", "commit": "a".repeat(40), "path": "x.json"}},
        }));
        let mut issues = Vec::new();
        resolve(
            "bindings",
            "Subject s",
            &declared,
            Some(&bound),
            &BTreeMap::new(),
            &providers,
            &mut issues,
        );
        assert_eq!(
            issues,
            [
                BindingIssue::new("bindings.port.$fromGit is not supported here"),
                rejected.clone()
            ]
        );

        let document = json!({
            "a": {"$fromUnit": {"unit": "network", "output": "vpcId"}},
            "b": {"$fromUnit": {"unit": "other", "output": "x"}, "pointer": "/id"},
        });
        let mut issues = Vec::new();
        resolve_document(
            "spec",
            "",
            &document,
            Context::UnitExecution.into(),
            &providers,
            &mut issues,
        );
        assert_eq!(
            issues,
            [
                rejected,
                BindingIssue::new(
                    "spec.b holds $fromUnit next to other keys (pointer); a value source replaces the whole object, so it must be the object's only key (wrap the object in $literal to keep it as data)"
                )
            ]
        );
    }
}
