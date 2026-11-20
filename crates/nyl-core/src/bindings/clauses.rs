//! The `$` value sources: what each one reads, how it is written, and where
//! it is valid.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md).
//! An object whose only key is a known `$` source is a reference to the value
//! it names, at any depth of a document the user wrote. Every source shares
//! parsing ([`Clause::parse`]), the missing-value rules, and the type checks;
//! only the [`Context`] decides which sources resolve there, which are carried
//! to where the document lands, and which are an error that teaches the fix.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{FileInputSource, GitInputSource, PromotionInputSource, PublicationInputSource, UnitInputSource};
use crate::{CoreError, Result};

/// The documentation of the value sources, linked from errors.
pub const VALUE_SOURCES_DOCS: &str = "https://niklasrosenstein.github.io/nyl/reference/value-sources/";

/// A `$` value source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// `$environment`: the environment's effective values.
    Environment,
    /// `$input`: a Release input.
    Input,
    /// `$parameter`: an EnvironmentTemplate instance's parameter.
    Parameter,
    /// `$unit`: a unit's output, artifact, or observed value.
    Unit,
    /// `$promotion`: a value recorded through a PromotionPath.
    Promotion,
    /// `$file`: a YAML or JSON file of this repository.
    File,
    /// `$git`: a file of a Git repository at a locked commit.
    Git,
    /// `$publication`: a state file in the target's publication branch.
    Publication,
    /// `$literal`: its content, verbatim and never walked.
    Literal,
}

impl Source {
    /// Every source.
    pub const ALL: [Source; 9] = [
        Self::Environment,
        Self::Input,
        Self::Parameter,
        Self::Unit,
        Self::Promotion,
        Self::File,
        Self::Git,
        Self::Publication,
        Self::Literal,
    ];

    /// The key as authored, such as `$environment`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Environment => "$environment",
            Self::Input => "$input",
            Self::Parameter => "$parameter",
            Self::Unit => "$unit",
            Self::Promotion => "$promotion",
            Self::File => "$file",
            Self::Git => "$git",
            Self::Publication => "$publication",
            Self::Literal => "$literal",
        }
    }

    /// The source a key names, if it is a known one.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.key() == key)
    }

    /// Whether only orchestrated execution resolves this source.
    pub fn requires_orchestration(self) -> bool {
        matches!(self, Self::Unit | Self::Promotion)
    }

    /// Where the source is valid, for errors about one written elsewhere.
    fn valid_where(self) -> &'static str {
        match self {
            Self::Environment => "reads the environment's values and resolves only in unit documents",
            Self::Input => {
                "reads a Release input and resolves only in the fields of a HelmChart in a Release bundle"
            }
            Self::Parameter => "reads a template parameter and resolves only in an EnvironmentTemplate's spec.values",
            Self::Unit | Self::Promotion => {
                "is resolved by orchestration and is valid only in unit documents and in a DeploymentTarget's releaseInputs"
            }
            Self::File | Self::Git | Self::Publication => {
                "fills a Release input and is valid only in a DeploymentTarget's releaseInputs"
            }
            Self::Literal => "is valid wherever value sources are",
        }
    }
}

/// `$` keys that look like a source but are none: a likely slip, with what
/// was probably meant. In free-form data any other `$` key, such as `$ref`
/// or `$schema`, is data.
pub const SLIPS: &[(&str, &str)] = &[
    (
        "$value",
        "$literal for a literal value, or $environment for the environment's values",
    ),
    ("$values", "$environment"),
    ("$env", "$environment"),
    ("$inputs", "$input"),
    ("$param", "$parameter"),
    ("$params", "$parameter"),
    ("$parameters", "$parameter"),
    ("$units", "$unit"),
    ("$output", "$unit"),
    ("$artifact", "$unit"),
];

/// `$environment`: a value of the environment's effective `values`.
///
/// When nothing is at `pointer`, or the value there is `null`, `default`
/// applies; with `optional: true` the field is removed instead, as if it were
/// omitted; otherwise it is an error. Shorthand: `{$environment: /pointer}`.
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

impl ValuesSource {
    fn validate(&self, field: &str) -> Result<()> {
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)?;
        if self.default.is_some() && self.optional {
            return Err(CoreError::config(format!(
                "{field} sets both default and optional; set default to use a value when none is found, or optional: true to omit the field"
            )));
        }
        Ok(())
    }
}

/// `$input`: a Release input. Its declared default applies when the target
/// binds none. Shorthand: `{$input: name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InputSource {
    /// Name of an input the Release declares.
    pub name: String,
    /// JSON Pointer inside the input's value; the whole value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// `$parameter`: a parameter an EnvironmentTemplate instance binds. The
/// declared default applies when the instance binds none. Shorthand:
/// `{$parameter: name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ParameterSource {
    /// Name of a parameter the template declares.
    pub name: String,
    /// JSON Pointer inside the parameter's value; the whole value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
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

/// A parsed value source.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Environment(ValuesSource),
    Input(InputSource),
    Parameter(ParameterSource),
    Unit(UnitInputSource),
    Promotion(PromotionInputSource),
    File(FileInputSource),
    Git(GitInputSource),
    Publication(PublicationInputSource),
    Literal(Value),
}

impl Clause {
    /// The source of this clause.
    pub fn source(&self) -> Source {
        match self {
            Self::Environment(_) => Source::Environment,
            Self::Input(_) => Source::Input,
            Self::Parameter(_) => Source::Parameter,
            Self::Unit(_) => Source::Unit,
            Self::Promotion(_) => Source::Promotion,
            Self::File(_) => Source::File,
            Self::Git(_) => Source::Git,
            Self::Publication(_) => Source::Publication,
            Self::Literal(_) => Source::Literal,
        }
    }

    /// The value used when the source holds none. Never `null`.
    pub fn default_value(&self) -> Option<&Value> {
        match self {
            Self::Environment(source) => source.default.as_ref(),
            _ => None,
        }
    }

    /// Whether a missing value removes the field, as if it were omitted.
    pub fn optional(&self) -> bool {
        matches!(self, Self::Environment(source) if source.optional)
    }

    /// The clause object, as it is written in its object form.
    pub fn to_value(&self) -> Value {
        let body = match self {
            Self::Environment(source) => serde_json::to_value(source),
            Self::Input(source) => serde_json::to_value(source),
            Self::Parameter(source) => serde_json::to_value(source),
            Self::Unit(source) => serde_json::to_value(source),
            Self::Promotion(source) => serde_json::to_value(source),
            Self::File(source) => serde_json::to_value(source),
            Self::Git(source) => serde_json::to_value(source),
            Self::Publication(source) => serde_json::to_value(source),
            Self::Literal(value) => Ok(value.clone()),
        }
        .expect("clauses serialize");
        serde_json::json!({ self.source().key(): body })
    }

    /// Parse the body of a `source` clause written at `field`, the object that
    /// holds it, such as `spec.target`: its shorthand or its object form, then
    /// the static checks of its fields.
    pub fn parse(field: &str, source: Source, body: &Value) -> std::result::Result<Self, String> {
        let at = format!("{field}.{}", source.key());
        let key = source.key();
        match (source, body) {
            (Source::Literal, body) => Ok(Self::Literal(body.clone())),
            (Source::Environment, Value::String(pointer)) => {
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(format!("{at} takes a JSON Pointer; did you mean /{pointer}?"));
                }
                Ok(Self::Environment(ValuesSource {
                    pointer: pointer.clone(),
                    default: None,
                    optional: false,
                }))
            }
            (Source::Input | Source::Parameter, Value::String(name)) => {
                if let Some(pointer) = name.strip_prefix('/') {
                    let (first, rest) = pointer.split_once('/').unwrap_or((pointer, ""));
                    let suggestion = if rest.is_empty() {
                        first.to_owned()
                    } else {
                        format!("{{name: {first}, pointer: /{rest}}}")
                    };
                    return Err(format!("{at} takes a name; did you mean {suggestion}?"));
                }
                if name.is_empty() {
                    return Err(format!(
                        "{at} must name an {}",
                        if source == Source::Input { "input" } else { "parameter" }
                    ));
                }
                Ok(if source == Source::Input {
                    Self::Input(InputSource {
                        name: name.clone(),
                        pointer: None,
                    })
                } else {
                    Self::Parameter(ParameterSource {
                        name: name.clone(),
                        pointer: None,
                    })
                })
            }
            (_, Value::Object(_)) => Self::parse_object(&at, source, body),
            (Source::Environment, other) => Err(format!(
                "{at} must be a JSON Pointer or an object {{pointer, default?, optional?}}, not {}",
                super::json_type_name(other)
            )),
            (Source::Input | Source::Parameter, other) => Err(format!(
                "{at} must be a name or an object {{name, pointer?}}, not {}",
                super::json_type_name(other)
            )),
            (_, other) => Err(format!(
                "{at} must be an object, not {}; {key} has no shorthand",
                super::json_type_name(other)
            )),
        }
    }

    /// The object form of a `source` clause at `at`, such as `spec.target.$unit`.
    fn parse_object(at: &str, source: Source, body: &Value) -> std::result::Result<Self, String> {
        let invalid = |error: serde_json::Error| format!("{at} is not valid: {error}");
        let checked = |result: Result<()>| result.map_err(|error| error.to_string());
        match source {
            Source::Environment => {
                let clause: ValuesSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(clause.validate(at))?;
                Ok(Self::Environment(clause))
            }
            Source::Input => {
                let clause: InputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(validate_named(at, &clause.name, clause.pointer.as_deref()))?;
                Ok(Self::Input(clause))
            }
            Source::Parameter => {
                let clause: ParameterSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(validate_named(at, &clause.name, clause.pointer.as_deref()))?;
                Ok(Self::Parameter(clause))
            }
            Source::Unit => {
                let clause: UnitInputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(clause.validate(at))?;
                Ok(Self::Unit(clause))
            }
            Source::Promotion => {
                let clause: PromotionInputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                Ok(Self::Promotion(clause))
            }
            Source::File => {
                let clause: FileInputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(clause.validate(at))?;
                Ok(Self::File(clause))
            }
            Source::Git => {
                let clause: GitInputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(clause.validate(at))?;
                Ok(Self::Git(clause))
            }
            Source::Publication => {
                let clause: PublicationInputSource = serde_json::from_value(body.clone()).map_err(invalid)?;
                checked(clause.validate(at))?;
                Ok(Self::Publication(clause))
            }
            Source::Literal => unreachable!("handled above"),
        }
    }
}

/// The `$` key of `fields` that names a known source, if `fields` is a value
/// source: its only key. `Err` holds the known keys of an object that mixes a
/// source with other keys.
pub fn clause_key(fields: &serde_json::Map<String, Value>) -> std::result::Result<Option<Source>, Vec<&str>> {
    let known = fields
        .keys()
        .filter(|key| Source::from_key(key).is_some())
        .map(String::as_str)
        .collect::<Vec<_>>();
    match known.as_slice() {
        [] => Ok(None),
        [key] if fields.len() == 1 => Ok(Source::from_key(key)),
        _ => Err(known),
    }
}

/// Whether `value` holds a `$` key anywhere, so it cannot be unwrapped from a
/// `$literal` without later reading as a source or a slip.
pub fn holds_dollar_key(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(key, child)| key.starts_with('$') || holds_dollar_key(child)),
        Value::Array(items) => items.iter().any(holds_dollar_key),
        _ => false,
    }
}

/// Where a document sits, which decides what each source does there.
///
/// Contract: the context table of
/// [Declarative value sources](../../../../design/declarative-value-sources.md#contexts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// A unit document as its unit renders: `$environment` resolves; `$unit`
    /// and `$promotion` are carried to the run, which resolves them.
    UnitRender,
    /// The part of a unit document that is a DeploymentTarget's
    /// `releaseInputs`, such as an inline target's: also carries `$file`,
    /// `$git`, and `$publication` to the target's rendering.
    UnitTargetInputs,
    /// A rendered unit spec as a run resolves it: `$unit`, `$promotion`, and
    /// `$literal` resolve; `$file`, `$git`, and `$publication` were placed in
    /// a target's bindings when the unit rendered, and are carried.
    UnitExecution,
    /// The Nyl resources of a Release bundle: `$input` resolves.
    ReleaseBundle,
    /// An EnvironmentTemplate's `spec.values`: `$parameter` resolves; the
    /// sources of the documents the values land in are carried.
    TemplateValues,
    /// A declared Environment's or EnvironmentGroup's `values`: nothing
    /// resolves; the sources of the documents the values land in are carried.
    EnvironmentValues,
    /// A DeploymentTarget's Release-input bindings, declared or inline.
    ReleaseInputs,
    /// A field that is read before anything resolves, or that is never a
    /// document the user's sources reach; every source is an error.
    Literal,
}

/// What a context does with a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    /// The source resolves here.
    Resolve,
    /// The source is left in place, checked, and resolved where the document
    /// lands; its body is walked for the sources that resolve here.
    Carry,
    /// The source is an error here.
    Reject,
}

/// Whether unknown `$` keys are reserved, or data apart from [`SLIPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Nyl's typed resource fields: every `$` key is a source or an error.
    Reserved,
    /// Free-form data: foreign `$` keys are data, the slips an error.
    FreeForm,
}

impl Context {
    /// What this context does with `source`.
    pub fn treats(self, source: Source) -> Treatment {
        use Source::{Environment, File, Git, Input, Literal, Parameter, Promotion, Publication, Unit};
        let resolves = match self {
            Self::UnitRender | Self::UnitTargetInputs => matches!(source, Environment),
            Self::UnitExecution => matches!(source, Unit | Promotion | Literal),
            Self::ReleaseBundle => matches!(source, Input | Literal),
            Self::TemplateValues => matches!(source, Parameter),
            Self::ReleaseInputs => matches!(source, Unit | Promotion | File | Git | Publication | Literal),
            Self::EnvironmentValues | Self::Literal => false,
        };
        let carries = match self {
            Self::UnitRender => matches!(source, Unit | Promotion | Literal),
            Self::UnitTargetInputs => matches!(source, Unit | Promotion | Literal | File | Git | Publication),
            Self::UnitExecution => matches!(source, File | Git | Publication),
            Self::TemplateValues | Self::EnvironmentValues => {
                matches!(source, Unit | Promotion | File | Git | Publication | Literal)
            }
            Self::ReleaseBundle | Self::ReleaseInputs | Self::Literal => false,
        };
        if resolves {
            Treatment::Resolve
        } else if carries {
            Treatment::Carry
        } else {
            Treatment::Reject
        }
    }

    /// Whether unknown `$` keys are reserved here.
    pub fn namespace(self) -> Namespace {
        match self {
            Self::ReleaseBundle => Namespace::Reserved,
            _ => Namespace::FreeForm,
        }
    }

    /// The error for a `source` clause at `field`, which this context rejects:
    /// where the source is valid, and how to get the value here instead.
    pub fn misplaced(self, field: &str, source: Source) -> String {
        let key = source.key();
        let fix = match (self, source) {
            (Self::ReleaseBundle, _) => {
                format!("declare a Release input and bind it on the target{}", match source {
                    Source::Unit | Source::Promotion | Source::File | Source::Git | Source::Publication => {
                        format!(" with {key}")
                    }
                    _ => String::new(),
                })
            }
            (Self::UnitRender | Self::UnitTargetInputs | Self::UnitExecution, Source::Input) => {
                "a unit has no Release inputs; read the environment's values with $environment".to_owned()
            }
            (Self::UnitRender | Self::UnitTargetInputs | Self::UnitExecution, Source::Parameter) => {
                "set the value in the EnvironmentTemplate's spec.values with $parameter, and read it here with $environment"
                    .to_owned()
            }
            (Self::UnitRender | Self::UnitExecution, Source::File | Source::Git | Source::Publication) => {
                "bind it to a Release input in the DeploymentTarget's releaseInputs".to_owned()
            }
            (Self::UnitExecution, Source::Environment) => {
                "it resolves when the unit renders; render the unit again".to_owned()
            }
            (Self::TemplateValues | Self::EnvironmentValues, Source::Environment) => {
                "the environment's values cannot read its values; write the value here".to_owned()
            }
            (Self::EnvironmentValues, Source::Parameter) => {
                "only an EnvironmentTemplate's values read parameters".to_owned()
            }
            (Self::TemplateValues | Self::EnvironmentValues, Source::Input) => {
                "values do not reach a Release's inputs; bind the input on the target".to_owned()
            }
            (Self::ReleaseInputs, Source::Environment) => {
                "a declared DeploymentTarget is no unit document; write the value here, or declare the target inline in a KubernetesPublication unit"
                    .to_owned()
            }
            (Self::ReleaseInputs, Source::Parameter) => {
                "declare the target inline in the EnvironmentTemplate's values, where $parameter resolves".to_owned()
            }
            (Self::ReleaseInputs, Source::Input) => "a binding fills a Release input; it cannot read one".to_owned(),
            (Self::Literal, _) => "write a literal value".to_owned(),
            _ => "move it to where it resolves".to_owned(),
        };
        format!(
            "{field} holds a {key} source, which {}; {fix} (see {VALUE_SOURCES_DOCS})",
            source.valid_where()
        )
    }
}

/// The fields of a resource that are read before anything renders or
/// resolves: discovery and selection read them. `metadata.namespace` is not
/// one of them.
pub const LITERAL_ONLY_FIELDS: &[&str] = &["/apiVersion", "/kind", "/metadata/name", "/metadata/labels"];

/// Report every value source in the literal-only fields of `document`
/// ([`LITERAL_ONLY_FIELDS`]), such as a unit document or a resource of a
/// Release bundle: discovery and selection read them before anything resolves.
pub fn literal_field_issues(document: &Value) -> Vec<String> {
    let mut issues = Vec::new();
    for pointer in LITERAL_ONLY_FIELDS {
        if let Some(value) = document.pointer(pointer) {
            let field = pointer.trim_start_matches('/').replace('/', ".");
            find_any(&field, value, &mut issues);
        }
    }
    issues
}

fn find_any(field: &str, value: &Value, issues: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(key) = fields.keys().find(|key| Source::from_key(key).is_some()) {
                issues.push(format!(
                    "{field} holds a {key} source, but {field} must be literal, because discovery and selection read it before anything resolves (see {VALUE_SOURCES_DOCS})"
                ));
                return;
            }
            for (key, child) in fields {
                find_any(&format!("{field}.{key}"), child, issues);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                find_any(&format!("{field}[{index}]"), child, issues);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(source: Source, body: Value) -> std::result::Result<Clause, String> {
        Clause::parse("spec.x", source, &body)
    }

    #[test]
    fn test_shorthands_read_the_main_field() {
        assert_eq!(
            parse(Source::Environment, json!("/target")).unwrap(),
            Clause::Environment(ValuesSource {
                pointer: "/target".to_owned(),
                default: None,
                optional: false
            })
        );
        assert_eq!(
            parse(Source::Input, json!("image")).unwrap(),
            Clause::Input(InputSource {
                name: "image".to_owned(),
                pointer: None
            })
        );
        assert_eq!(
            parse(Source::Parameter, json!({"name": "sizes", "pointer": "/web"})).unwrap(),
            Clause::Parameter(ParameterSource {
                name: "sizes".to_owned(),
                pointer: Some("/web".to_owned())
            })
        );
    }

    /// Catches the two shorthand slips the UX review found: a pointer where a
    /// name belongs, and a name where a pointer belongs.
    #[test]
    fn test_shorthand_misuse_suggests_the_fix() {
        assert_eq!(
            parse(Source::Input, json!("/image")).unwrap_err(),
            "spec.x.$input takes a name; did you mean image?"
        );
        assert_eq!(
            parse(Source::Parameter, json!("/sizes/web")).unwrap_err(),
            "spec.x.$parameter takes a name; did you mean {name: sizes, pointer: /web}?"
        );
        assert_eq!(
            parse(Source::Environment, json!("target")).unwrap_err(),
            "spec.x.$environment takes a JSON Pointer; did you mean /target?"
        );
        assert_eq!(
            parse(Source::Unit, json!("network")).unwrap_err(),
            "spec.x.$unit must be an object, not string; $unit has no shorthand"
        );
    }

    #[test]
    fn test_missing_value_options_are_checked() {
        let error = parse(Source::Environment, json!({"pointer": "/a", "default": null})).unwrap_err();
        assert!(error.contains("default must not be null"), "{error}");
        let error = parse(
            Source::Environment,
            json!({"pointer": "/a", "default": 1, "optional": true}),
        )
        .unwrap_err();
        assert!(error.contains("sets both default and optional"), "{error}");
        assert!(parse(Source::Input, json!({"name": "a", "default": 1})).is_err());
        let clause = parse(Source::Environment, json!({"pointer": "/a", "optional": true})).unwrap();
        assert!(clause.optional());
    }

    #[test]
    fn test_clause_key_finds_a_lone_source_and_reports_siblings() {
        let object = |value: Value| value.as_object().unwrap().clone();
        assert_eq!(clause_key(&object(json!({"$unit": {}}))), Ok(Some(Source::Unit)));
        assert_eq!(clause_key(&object(json!({"$ref": "x", "a": 1}))), Ok(None));
        assert_eq!(
            clause_key(&object(json!({"$unit": {}, "pointer": "/x"}))),
            Err(vec!["$unit"])
        );
    }

    #[test]
    fn test_literal_fields_hold_no_clause() {
        let document = json!({
            "apiVersion": "v1",
            "kind": "X",
            "metadata": {"name": {"$environment": "/name"}, "namespace": {"$input": "ns"}, "labels": {"a": {"$literal": "b"}}},
        });
        let issues = literal_field_issues(&document);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues[0].starts_with("metadata.name holds a $environment source"));
        assert!(issues[1].starts_with("metadata.labels.a holds a $literal source"));
    }

    #[test]
    fn test_misplaced_teaches_the_fix() {
        let message = Context::ReleaseBundle.misplaced("spec.image", Source::Unit);
        assert!(
            message.starts_with("spec.image holds a $unit source, which is resolved by orchestration"),
            "{message}"
        );
        assert!(
            message.contains("declare a Release input and bind it on the target with $unit"),
            "{message}"
        );
    }
}
