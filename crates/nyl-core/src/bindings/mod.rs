//! One resolver for typed value bindings and references.
//!
//! Contract: [Bindings](../../../../design/release-inputs.md#bindings) and the
//! effective-value rule, and the orchestration core contract's References.
//! Release inputs are its first user; unit `values` and `variables`,
//! PromotionPath selectors, and `nyl get` value forms reuse it, so they cannot
//! disagree on precedence, type checks, selection, or digests.
//!
//! - A declaration is typed ([`InputDeclaration`]); a binding sets exactly one
//!   source ([`InputBinding::kind`]).
//! - [`resolve`] fills declared slots: the effective value is the override,
//!   otherwise the binding, otherwise the declared default. A source that holds
//!   no value yet leaves the slot unbound, so the default applies.
//! - [`resolve_document`] replaces reference objects at any depth of a
//!   free-form document and records provenance per JSON Pointer.
//! - A provider may report a reference as [`Provision::Blocked`], such as a
//!   producer without a current receipt; blocked values are listed with their
//!   location instead of failing resolution.
//! - Every problem is collected, so a caller reports them together.
//!
//! Each binding kind is one [`Providers`] method. The resolver itself performs
//! no effects: providers read what their caller gathered, whether files and
//! Git for rendering or a state snapshot for orchestration.

mod binding;
mod declaration;

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

pub use binding::{
    BindingKind, FileInputSource, GitInputSource, InputBinding, PromotionInputSource, PublicationInputSource,
    UnitInputSource,
};
pub use declaration::{json_type_name, validate_declarations_at, validate_input_name, InputDeclaration, InputType};

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
    Blocked(String),
}

/// A provider's answer, or the reason the binding is invalid.
pub type Provided<O> = std::result::Result<Provision<O>, String>;

/// A value that could not be read yet, and where it is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// Location in messages, such as `spec.variables.vpc_id`.
    pub field: String,
    /// JSON Pointer of the value: for [`resolve_document`], below the pointer
    /// the caller passes, such as `/variables/vpc_id`; for [`resolve`], the
    /// slot name, such as `/image`.
    pub pointer: String,
    pub reason: String,
}

/// The provenance a caller records for each value.
pub trait Origin {
    /// The declared default.
    fn from_default() -> Self;
    /// A caller-supplied override, such as a direct command's `--input`.
    fn from_override() -> Self;
    /// An inline `value` binding.
    fn from_value() -> Self;
    /// Names the value's source in messages about `field`.
    fn describe(&self, field: &str) -> String;
}

/// Reads the source of each binding kind. A kind a caller does not accept keeps
/// the default method, which rejects it.
pub trait Providers {
    type Origin: Origin;

    fn file(&self, field: &str, _source: &FileInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, BindingKind::FromFile))
    }

    fn git(&self, field: &str, _source: &GitInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, BindingKind::FromGit))
    }

    fn publication(&self, field: &str, _source: &PublicationInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, BindingKind::FromPublication))
    }

    fn unit(&self, field: &str, _source: &UnitInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, BindingKind::FromUnit))
    }

    fn promotion(&self, field: &str, _source: &PromotionInputSource) -> Provided<Self::Origin> {
        Err(unsupported(field, BindingKind::FromPromotion))
    }

    /// Why a required slot whose binding left it unbound is missing, when the
    /// provider knows more than the generic message. `requirement` reads like
    /// "Release platform/web requires input \"image\"".
    fn unbound(&self, _field: &str, _binding: &InputBinding, _requirement: &str) -> Option<String> {
        None
    }
}

fn unsupported(field: &str, kind: BindingKind) -> String {
    format!("{field}.{} is not supported here", kind.field())
}

/// Resolve one binding through the provider for its kind.
pub fn resolve_binding<P: Providers>(field: &str, binding: &InputBinding, providers: &P) -> Provided<P::Origin> {
    let set = "kind agrees with the set field";
    match binding.kind(field).map_err(|error| error.to_string())? {
        BindingKind::Value => Ok(Provision::Value(Resolved {
            value: binding.value.clone().expect(set),
            origin: P::Origin::from_value(),
        })),
        BindingKind::FromFile => providers.file(field, binding.from_file.as_ref().expect(set)),
        BindingKind::FromGit => providers.git(field, binding.from_git.as_ref().expect(set)),
        BindingKind::FromPublication => providers.publication(field, binding.from_publication.as_ref().expect(set)),
        BindingKind::FromUnit => providers.unit(field, binding.from_unit.as_ref().expect(set)),
        BindingKind::FromPromotion => providers.promotion(field, binding.from_promotion.as_ref().expect(set)),
    }
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
    issues: &mut Vec<String>,
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
                    issues.push(
                        binding
                            .and_then(|binding| providers.unbound(&field, binding, &requirement))
                            .unwrap_or_else(|| {
                                format!(
                                    "{requirement}, but it has no binding and no default; bind it in {field_prefix}"
                                )
                            }),
                    );
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
            Err(error) => {
                issues.push(error);
                continue;
            }
        };
        match declaration.check(&input.value) {
            Ok(()) => {
                slots.values.insert(name.clone(), input);
            }
            Err(reason) => issues.push(format!(
                "{} for input {name:?} of {subject} is invalid: {reason}",
                input.origin.describe(&field)
            )),
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

/// Replace every reference object in `document`, at any depth of its objects
/// and arrays, by the value it stands for.
///
/// Contract: orchestration core, References. A reference object has exactly
/// one field, `fromUnit` or `fromPromotion`, and replaces the whole value it
/// stands for; every other object is data. `field` and `pointer` locate
/// `document` itself, such as `spec.variables` and `/variables`. A reference
/// that resolves to no value is an issue, because a document declares no
/// defaults.
pub fn resolve_document<P: Providers>(
    field: &str,
    pointer: &str,
    document: &Value,
    providers: &P,
    issues: &mut Vec<String>,
) -> ResolvedDocument<P::Origin> {
    let mut value = document.clone();
    let mut found = Found {
        provenance: BTreeMap::new(),
        blocked: Vec::new(),
    };
    walk(field, pointer, &mut value, providers, &mut found, issues);
    ResolvedDocument {
        value,
        provenance: found.provenance,
        blocked: found.blocked,
    }
}

struct Found<O> {
    provenance: BTreeMap<String, O>,
    blocked: Vec<Blocked>,
}

/// Fields that make an object a reference.
const REFERENCE_KINDS: [&str; 2] = ["fromUnit", "fromPromotion"];

fn walk<P: Providers>(
    field: &str,
    pointer: &str,
    value: &mut Value,
    providers: &P,
    found: &mut Found<P::Origin>,
    issues: &mut Vec<String>,
) {
    match value {
        Value::Object(fields) => {
            let references = fields
                .keys()
                .filter(|key| REFERENCE_KINDS.contains(&key.as_str()))
                .collect::<Vec<_>>();
            match references.as_slice() {
                [] => {
                    for (key, child) in fields.iter_mut() {
                        walk(
                            &format!("{field}.{key}"),
                            &crate::json_pointer::child(pointer, key),
                            child,
                            providers,
                            found,
                            issues,
                        );
                    }
                }
                [kind] if fields.len() == 1 => {
                    let kind = (*kind).clone();
                    if let Some(input) = resolve_reference(field, pointer, &kind, value, providers, found, issues) {
                        *value = input;
                    }
                }
                _ => issues.push(format!(
                    "{field} mixes {} with other fields; a reference object holds only its reference, and replaces the whole value",
                    references.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(" and ")
                )),
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                walk(
                    &format!("{field}[{index}]"),
                    &format!("{pointer}/{index}"),
                    child,
                    providers,
                    found,
                    issues,
                );
            }
        }
        _ => {}
    }
}

/// Resolve one reference object; `Some` is the value that replaces it.
fn resolve_reference<P: Providers>(
    field: &str,
    pointer: &str,
    kind: &str,
    reference: &Value,
    providers: &P,
    found: &mut Found<P::Origin>,
    issues: &mut Vec<String>,
) -> Option<Value> {
    if !reference[kind].is_object() {
        issues.push(format!("{field}.{kind} must be an object"));
        return None;
    }
    let binding = match InputBinding::deserialize(reference) {
        Ok(binding) => binding,
        Err(error) => {
            issues.push(format!("{field} is not a valid reference: {error}"));
            return None;
        }
    };
    if let Err(error) = binding.validate(field) {
        issues.push(error.to_string());
        return None;
    }
    match resolve_binding(field, &binding, providers) {
        Ok(Provision::Value(input)) => {
            found.provenance.insert(pointer.to_owned(), input.origin);
            Some(input.value)
        }
        Ok(Provision::Unbound) => {
            let requirement = format!("{field} needs a value");
            issues.push(
                providers
                    .unbound(field, &binding, &requirement)
                    .unwrap_or_else(|| format!("{requirement}, but its {kind} reference has none yet")),
            );
            None
        }
        Ok(Provision::Blocked(reason)) => {
            found.blocked.push(Blocked {
                field: field.to_owned(),
                pointer: pointer.to_owned(),
                reason,
            });
            None
        }
        Err(error) => {
            issues.push(error);
            None
        }
    }
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

    #[derive(Debug, Clone, PartialEq)]
    enum TestOrigin {
        Default,
        Override,
        Value,
        File(String),
        Unit(String),
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
    #[derive(Default)]
    struct TestProviders {
        files: BTreeMap<&'static str, Value>,
        units: BTreeMap<&'static str, Value>,
    }

    impl Providers for TestProviders {
        type Origin = TestOrigin;

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
            Ok(match self.units.get(source.unit.as_str()) {
                Some(output) => Provision::Value(Resolved {
                    value: output.clone(),
                    origin: TestOrigin::Unit(source.unit.clone()),
                }),
                None => Provision::Blocked(format!("{} has no current receipt", source.unit)),
            })
        }

        fn unbound(&self, field: &str, binding: &InputBinding, requirement: &str) -> Option<String> {
            binding
                .from_file
                .as_ref()
                .map(|source| format!("{requirement}, but {field} file {} does not exist yet", source.path))
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
        (slots, issues)
    }

    #[test]
    fn test_resolve_prefers_override_then_binding_then_default() {
        let declared = declarations(json!({
            "a": {"type": "integer", "default": 1},
            "b": {"type": "integer", "default": 1},
            "c": {"type": "integer", "default": 1},
        }));
        let bound = bindings(json!({"a": {"value": 2}, "b": {"value": 2}}));
        let overrides = BTreeMap::from([("a".to_owned(), json!(3))]);
        let (slots, issues) = resolve_all(&declared, Some(&bound), &overrides, &TestProviders::default());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(slots.values["a"].value, json!(3));
        assert_eq!(slots.values["a"].origin, TestOrigin::Override);
        assert_eq!(slots.values["b"].origin, TestOrigin::Value);
        assert_eq!(slots.values["c"].origin, TestOrigin::Default);
    }

    #[test]
    fn test_resolve_falls_back_to_the_default_when_a_source_holds_no_value() {
        let declared = declarations(json!({
            "tag": {"type": "string", "default": "none"},
            "image": {"type": "string"},
        }));
        let bound = bindings(json!({
            "tag": {"fromFile": {"path": "missing.json"}},
            "image": {"fromFile": {"path": "missing.json"}},
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
            "port": {"fromFile": {"path": "ports.json", "pointer": "/web"}},
            "sha": {"fromGit": {"repository": {"repoURL": "https://git.example.com/x.git"}, "revision": "main", "commit": "a".repeat(40), "path": "x.json"}},
            "tier": {"value": "large"},
            "vpc": {"fromUnit": {"unit": "network", "output": "vpcId"}},
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
                "bindings.sha.fromGit is not supported here",
                r#"bindings.tier (Value) for input "tier" of Subject s is invalid: "large" is not one of the allowed values ["small"]"#,
            ]
        );
        assert_eq!(
            slots.blocked,
            [Blocked {
                field: "bindings.vpc".to_owned(),
                pointer: "/vpc".to_owned(),
                reason: "network has no current receipt".to_owned(),
            }]
        );
    }

    #[test]
    fn test_undeclared_binding_issues_name_the_declared_slots() {
        let declared = declarations(json!({"image": {"type": "string"}}));
        let bound = bindings(json!({"imgae": {"value": "x"}}));
        assert_eq!(
            undeclared_binding_issues("bindings", "Subject s", &bound, &declared),
            ["bindings.imgae binds an input Subject s does not declare; declared inputs: image"]
        );
    }

    #[test]
    fn test_resolve_document_replaces_references_at_any_depth() {
        let providers = TestProviders {
            units: BTreeMap::from([("network", json!("vpc-0abc123"))]),
            ..TestProviders::default()
        };
        let document = json!({
            "vpc_id": {"fromUnit": {"unit": "network", "output": "vpcId"}},
            "db": {"hosts": [{"fromUnit": {"unit": "database", "output": "host"}}, "static"]},
            "literal": {"fromGit": "main"},
        });
        let mut issues = Vec::new();
        let resolved = resolve_document("spec.variables", "/variables", &document, &providers, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved.value["vpc_id"], json!("vpc-0abc123"));
        // Only fromUnit and fromPromotion are references in a document.
        assert_eq!(resolved.value["literal"], json!({"fromGit": "main"}));
        assert_eq!(
            resolved.provenance,
            BTreeMap::from([("/variables/vpc_id".to_owned(), TestOrigin::Unit("network".to_owned()))])
        );
        // The blocked reference stays in place, and is named by its location.
        assert_eq!(resolved.value["db"]["hosts"][0], document["db"]["hosts"][0]);
        assert_eq!(
            resolved.blocked,
            [Blocked {
                field: "spec.variables.db.hosts[0]".to_owned(),
                pointer: "/variables/db/hosts/0".to_owned(),
                reason: "database has no current receipt".to_owned(),
            }]
        );
    }

    #[test]
    fn test_resolve_document_reports_invalid_references() {
        let document = json!({
            "a": {"fromUnit": {"unit": "network"}},
            "b": {"fromUnit": {"unit": "network", "output": "vpcId"}, "pointer": "/id"},
            "c": {"fromUnit": {"unit": "network", "output": "vpcId", "extra": 1}},
            "d": {"fromUnit": null},
            "e": {"fromPromotion": {"value": "image"}},
        });
        let mut issues = Vec::new();
        resolve_document("values", "", &document, &TestProviders::default(), &mut issues);
        assert_eq!(issues.len(), 5, "{issues:?}");
        assert_eq!(
            issues[0],
            "values.a.fromUnit must set exactly one of output and artifact"
        );
        assert!(
            issues[1].starts_with("values.b mixes fromUnit with other fields"),
            "{issues:?}"
        );
        assert!(issues[2].starts_with("values.c is not a valid reference"), "{issues:?}");
        assert_eq!(issues[3], "values.d.fromUnit must be an object");
        assert_eq!(issues[4], "values.e.fromPromotion is not supported here");
    }
}
