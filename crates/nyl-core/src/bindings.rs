//! One resolver for typed value bindings.
//!
//! Contract: [Bindings](../../../design/release-inputs.md#bindings) and the
//! effective-value rule. Release inputs are its first user; unit `values` and
//! `variables`, PromotionPath selectors, and `nyl get` value forms reuse it, so
//! they cannot disagree on precedence, type checks, selection, or digests.
//!
//! - A declaration is typed ([`InputDeclaration`]); a binding sets exactly one
//!   source ([`InputBinding::kind`]).
//! - The effective value is the override, otherwise the binding, otherwise the
//!   declared default. A binding whose source holds no value yet leaves the
//!   slot unbound, so the default applies.
//! - Every effective value is checked against its declaration, and every
//!   problem is collected so a caller reports them together.
//! - Each binding kind is served by one [`Providers`] method. The resolver
//!   performs no effects; providers read files, Git, or recorded state.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::resources::release_inputs::{
    BindingKind, FileInputSource, GitInputSource, InputBinding, InputDeclaration, PromotionInputSource,
    PublicationInputSource, UnitInputSource,
};

/// An effective value and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved<O> {
    pub value: Value,
    pub origin: O,
}

/// What a provider returns: a value, `None` for a source that holds no value
/// yet, or the reason the binding is invalid.
pub type Provided<O> = std::result::Result<Option<Resolved<O>>, String>;

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

/// Reads the source of each binding kind.
pub trait Providers {
    type Origin: Origin;

    fn file(&self, field: &str, source: &FileInputSource) -> Provided<Self::Origin>;
    fn git(&self, field: &str, source: &GitInputSource) -> Provided<Self::Origin>;
    fn publication(&self, field: &str, source: &PublicationInputSource) -> Provided<Self::Origin>;
    fn unit(&self, field: &str, source: &UnitInputSource) -> Provided<Self::Origin>;
    fn promotion(&self, field: &str, source: &PromotionInputSource) -> Provided<Self::Origin>;

    /// Why a required slot whose binding resolved to no value is missing.
    /// `requirement` reads like "Release platform/web requires input \"image\"".
    fn unbound(&self, field: &str, binding: &InputBinding, requirement: &str) -> String;
}

/// Resolve one binding through the provider for its kind.
pub fn resolve_binding<P: Providers>(field: &str, binding: &InputBinding, providers: &P) -> Provided<P::Origin> {
    let set = "kind agrees with the set field";
    match binding.kind(field).map_err(|error| error.to_string())? {
        BindingKind::Value => Ok(Some(Resolved {
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

/// Resolve every declared slot of `subject`, such as `"Release platform/web"`.
///
/// `field_prefix` names the bindings' location, such as
/// `spec.releaseInputs."platform/web"`. Problems are appended to `issues`; the
/// result omits the slots that failed.
pub fn resolve<P: Providers>(
    field_prefix: &str,
    subject: &str,
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
    overrides: &BTreeMap<String, Value>,
    providers: &P,
    issues: &mut Vec<String>,
) -> BTreeMap<String, Resolved<P::Origin>> {
    let mut resolved = BTreeMap::new();
    for (name, declaration) in declarations {
        let field = format!("{field_prefix}.{name}");
        let binding = bindings.and_then(|bindings| bindings.get(name));
        let candidate = if let Some(value) = overrides.get(name) {
            Ok(Some(Resolved {
                value: value.clone(),
                origin: P::Origin::from_override(),
            }))
        } else {
            binding
                .map(|binding| resolve_binding(&field, binding, providers))
                .transpose()
                .map(Option::flatten)
                .map(|input| {
                    input.or_else(|| {
                        declaration.default.clone().map(|value| Resolved {
                            value,
                            origin: P::Origin::from_default(),
                        })
                    })
                })
        };
        match candidate {
            Ok(Some(input)) => match declaration.check(&input.value) {
                Ok(()) => {
                    resolved.insert(name.clone(), input);
                }
                Err(reason) => issues.push(format!(
                    "{} for input {name:?} of {subject} is invalid: {reason}",
                    input.origin.describe(&field)
                )),
            },
            Ok(None) => {
                let description = declaration
                    .description
                    .as_deref()
                    .map(|description| format!(" ({description})"))
                    .unwrap_or_default();
                let requirement = format!("{subject} requires input {name:?}{description}");
                issues.push(match binding {
                    Some(binding) => providers.unbound(&field, binding, &requirement),
                    None => format!("{requirement}, but it has no binding and no default; bind it in {field_prefix}"),
                });
            }
            Err(error) => issues.push(error),
        }
    }
    resolved
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

    /// Files map a path to its value; a missing path holds no value yet.
    struct TestProviders(BTreeMap<&'static str, Value>);

    impl Providers for TestProviders {
        type Origin = TestOrigin;

        fn file(&self, _field: &str, source: &FileInputSource) -> Provided<TestOrigin> {
            Ok(self.0.get(source.path.as_str()).map(|document| Resolved {
                value: select(document, &source.pointer).unwrap(),
                origin: TestOrigin::File(source.path.clone()),
            }))
        }
        fn git(&self, field: &str, _source: &GitInputSource) -> Provided<TestOrigin> {
            Err(format!("{field}.fromGit is not provided"))
        }
        fn publication(&self, field: &str, _source: &PublicationInputSource) -> Provided<TestOrigin> {
            Err(format!("{field}.fromPublication is not provided"))
        }
        fn unit(&self, field: &str, _source: &UnitInputSource) -> Provided<TestOrigin> {
            Err(format!("{field}.fromUnit is not provided"))
        }
        fn promotion(&self, field: &str, _source: &PromotionInputSource) -> Provided<TestOrigin> {
            Err(format!("{field}.fromPromotion is not provided"))
        }
        fn unbound(&self, field: &str, _binding: &InputBinding, requirement: &str) -> String {
            format!("{requirement}, but {field} holds no value yet")
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
    ) -> (BTreeMap<String, Resolved<TestOrigin>>, Vec<String>) {
        let mut issues = Vec::new();
        let resolved = resolve(
            "bindings",
            "Subject s",
            declared,
            bound,
            overrides,
            providers,
            &mut issues,
        );
        (resolved, issues)
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
        let (resolved, issues) = resolve_all(&declared, Some(&bound), &overrides, &TestProviders(BTreeMap::new()));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(resolved["a"].value, json!(3));
        assert_eq!(resolved["a"].origin, TestOrigin::Override);
        assert_eq!(resolved["b"].origin, TestOrigin::Value);
        assert_eq!(resolved["c"].origin, TestOrigin::Default);
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
        let (resolved, issues) = resolve_all(
            &declared,
            Some(&bound),
            &BTreeMap::new(),
            &TestProviders(BTreeMap::new()),
        );
        assert_eq!(resolved["tag"].origin, TestOrigin::Default);
        assert_eq!(
            issues,
            [r#"Subject s requires input "image", but bindings.image holds no value yet"#]
        );
    }

    #[test]
    fn test_resolve_collects_every_problem() {
        let declared = declarations(json!({
            "port": {"type": "integer"},
            "tier": {"type": "string", "enum": ["small"]},
            "name": {"type": "string", "description": "Service name"},
            "sha": {"type": "string"},
        }));
        let bound = bindings(json!({
            "port": {"fromFile": {"path": "ports.json", "pointer": "/web"}},
            "tier": {"value": "large"},
            "sha": {"fromGit": {"repository": {"repoURL": "https://git.example.com/x.git"}, "revision": "main", "commit": "a".repeat(40), "path": "x.json"}},
        }));
        let providers = TestProviders(BTreeMap::from([("ports.json", json!({"web": "8080"}))]));
        let (resolved, issues) = resolve_all(&declared, Some(&bound), &BTreeMap::new(), &providers);
        assert!(resolved.is_empty());
        assert_eq!(
            issues,
            [
                r#"Subject s requires input "name" (Service name), but it has no binding and no default; bind it in bindings"#
                    .to_owned(),
                r#"bindings.port (File("ports.json")) for input "port" of Subject s is invalid: expected integer, got string"#.to_owned(),
                "bindings.sha.fromGit is not provided".to_owned(),
                r#"bindings.tier (Value) for input "tier" of Subject s is invalid: "large" is not one of the allowed values ["small"]"#.to_owned(),
            ]
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
}
