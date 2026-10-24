//! Component value imports.
//!
//! A local component opts in to inheriting chart value defaults from the
//! effective Cluster and DeploymentTarget `values` by shipping a
//! `nyl-component.yaml` contract beside its `Chart.yaml`. Each import copies one
//! JSON Pointer selection of those values to one chart-value destination.
//!
//! Precedence, from lowest to highest: chart `values.yaml` defaults, imported
//! values, then the invocation's `spec`. Objects merge recursively; arrays,
//! scalars, and `null` replace, the same rule that merges Cluster and target
//! values. Helm validates `values.schema.json` against the merged result.
//!
//! Imports read only the `values` of the render session's target, so they never
//! widen what a session admits. A render without a DeploymentTarget has no
//! values to import: optional imports contribute nothing and required imports
//! fail. User documentation: `nyl/book/src/content/docs/kubernetes/components/value-imports.md`.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::resources::ComponentValueImportOptions;
use crate::{NylError, Result};

/// File name of the contract inside a component chart directory.
pub const CONTRACT_FILE: &str = "nyl-component.yaml";

/// The only supported contract `apiVersion`.
pub const CONTRACT_API_VERSION: &str = "components.nyl/v1alpha1";

/// A component's declared contract with the platform that renders it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ComponentContract {
    /// Must be [`CONTRACT_API_VERSION`].
    pub api_version: String,
    /// Selections of target values that become chart value defaults.
    #[serde(default)]
    pub value_imports: Vec<ValueImport>,
}

/// One selection of target values copied to one chart-value destination.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueImport {
    /// Identifies the import in diagnostics and invocation opt-outs.
    pub name: String,
    /// JSON Pointer into the effective target `values`.
    pub from: String,
    /// JSON Pointer into the chart values.
    pub to: String,
    /// Fail the render when `from` is absent, instead of skipping the import.
    #[serde(default)]
    pub required: bool,
}

/// The target values an import may read.
#[derive(Debug, Clone, Copy)]
pub enum ImportScope<'a> {
    /// The effective values of the named DeploymentTarget.
    Target { name: &'a str, values: &'a Value },
    /// A render without a DeploymentTarget.
    Standalone,
}

impl ComponentContract {
    /// Load the contract of a component chart directory, if it has one.
    pub fn load(chart_dir: &Path) -> Result<Option<Self>> {
        let path = chart_dir.join(CONTRACT_FILE);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(NylError::config(format!("Failed to read {}: {error}", path.display())));
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "Component contract {} must not be a symbolic link",
                    path.display()
                )));
            }
            Ok(_) => {}
        }
        let source = std::fs::read_to_string(&path)
            .map_err(|error| NylError::config(format!("Failed to read {}: {error}", path.display())))?;
        Self::parse(&source, &path.display().to_string()).map(Some)
    }

    /// Parse and validate contract source; `origin` names it in errors.
    pub fn parse(source: &str, origin: &str) -> Result<Self> {
        let contract: Self = serde_saphyr::from_str(source)
            .map_err(|error| NylError::config(format!("Invalid component contract {origin}: {error}")))?;
        contract.validate(origin)?;
        Ok(contract)
    }

    fn validate(&self, origin: &str) -> Result<()> {
        let invalid = |message: String| NylError::config(format!("Invalid component contract {origin}: {message}"));
        if self.api_version != CONTRACT_API_VERSION {
            return Err(invalid(format!(
                "apiVersion must be {CONTRACT_API_VERSION:?}, got {:?}",
                self.api_version
            )));
        }
        let mut names = BTreeSet::new();
        let mut destinations: Vec<(&str, Vec<String>)> = Vec::new();
        for (index, import) in self.value_imports.iter().enumerate() {
            let field = format!("valueImports[{index}]");
            if import.name.trim().is_empty() {
                return Err(invalid(format!("{field}.name must not be empty")));
            }
            if !names.insert(import.name.as_str()) {
                return Err(invalid(format!("{field}.name {:?} is declared twice", import.name)));
            }
            for (key, pointer) in [("from", &import.from), ("to", &import.to)] {
                nyl_core::json_pointer::validate(&format!("{field}.{key}"), pointer)
                    .map_err(|error| invalid(error.to_string()))?;
                if pointer.is_empty() {
                    return Err(invalid(format!(
                        "{field}.{key} must select a value beneath the document root, such as \"/backup\""
                    )));
                }
            }
            let tokens = pointer_tokens(&import.to);
            for (other, other_tokens) in &destinations {
                if tokens.starts_with(other_tokens) || other_tokens.starts_with(&tokens) {
                    return Err(invalid(format!(
                        "{field} ({:?}) writes {}, which overlaps the destination of import {other:?}",
                        import.name, import.to
                    )));
                }
            }
            destinations.push((&import.name, tokens));
        }
        Ok(())
    }
}

/// Compute an invocation's chart values from its component's value imports.
///
/// `identity` names the invocation in errors. Without a contract the explicit
/// values are returned unchanged, so components that do not opt in render
/// exactly as before.
pub fn apply_value_imports(
    contract: Option<&ComponentContract>,
    options: Option<&ComponentValueImportOptions>,
    identity: &str,
    scope: ImportScope<'_>,
    explicit: Value,
) -> Result<Value> {
    let imports = contract.map_or(&[][..], |contract| contract.value_imports.as_slice());
    let excluded = options.map_or(&[][..], |options| options.exclude.as_slice());
    for name in excluded {
        if !imports.iter().any(|import| &import.name == name) {
            let declared = if imports.is_empty() {
                "it declares no value imports".to_string()
            } else {
                let names = imports.iter().map(|import| import.name.as_str()).collect::<Vec<_>>();
                format!("it declares {}", names.join(", "))
            };
            return Err(NylError::config(format!(
                "{identity}: valueImports.exclude names unknown import {name:?}; {declared}"
            )));
        }
    }
    if imports.is_empty() || options.is_some_and(|options| !options.enabled) {
        return Ok(explicit);
    }

    let mut imported = Map::new();
    for import in imports.iter().filter(|import| !excluded.contains(&import.name)) {
        let selected = match scope {
            ImportScope::Target { values, .. } => values.pointer(&import.from),
            ImportScope::Standalone => None,
        };
        let Some(selected) = selected else {
            if import.required {
                return Err(missing_required_import(identity, import, scope));
            }
            tracing::debug!(
                component = identity,
                import = %import.name,
                from = %import.from,
                "Skipping optional value import: target values do not set the source"
            );
            continue;
        };
        let tokens = pointer_tokens(&import.to);
        check_explicit_destination(identity, import, &tokens, &explicit)?;
        tracing::debug!(
            component = identity,
            import = %import.name,
            from = %import.from,
            to = %import.to,
            overridden = explicit.pointer(&import.to).is_some(),
            "Imported target value as a chart value default; explicit invocation values take precedence"
        );
        insert_at(&mut imported, &tokens, selected.clone());
    }
    if imported.is_empty() {
        return Ok(explicit);
    }
    let explicit = match explicit {
        Value::Null => Value::Object(Map::new()),
        value @ Value::Object(_) => value,
        other => {
            return Err(NylError::config(format!(
                "{identity}: spec must be an object to receive value imports, got {}",
                json_type(&other)
            )));
        }
    };
    Ok(crate::util::deep_merge_value(Some(Value::Object(imported)), explicit))
}

fn missing_required_import(identity: &str, import: &ValueImport, scope: ImportScope<'_>) -> NylError {
    let ValueImport { name, from, to, .. } = import;
    match scope {
        ImportScope::Target { name: target, .. } => NylError::config(format!(
            "{identity}: required value import {name:?} ({from} -> {to}) is not set in the values of target \
             {target:?}. Set {from} in the Cluster or DeploymentTarget values, or skip the import with \
             valueImports.exclude on the invocation."
        )),
        ImportScope::Standalone => NylError::config(format!(
            "{identity}: required value import {name:?} ({from} -> {to}) reads DeploymentTarget values, but this \
             render has no target. Render with --target, or skip the import with valueImports.exclude on the \
             invocation."
        )),
    }
}

/// Reject explicit values that put a non-object where an import descends.
///
/// An explicit `null` ancestor is an explicit override and wins like any other.
fn check_explicit_destination(identity: &str, import: &ValueImport, tokens: &[String], explicit: &Value) -> Result<()> {
    let mut current = explicit;
    let mut pointer = String::new();
    for token in &tokens[..tokens.len() - 1] {
        pointer = nyl_core::json_pointer::child(&pointer, token);
        match current.get(token) {
            None | Some(Value::Null) => return Ok(()),
            Some(value @ Value::Object(_)) => current = value,
            Some(other) => {
                return Err(NylError::config(format!(
                    "{identity}: value import {:?} writes {}, but the invocation sets {pointer} to {}",
                    import.name,
                    import.to,
                    json_type(other)
                )));
            }
        }
    }
    Ok(())
}

/// Insert `value` at `tokens`, creating objects along the way.
///
/// Contract validation guarantees destinations do not overlap, so every
/// intermediate node is an object created by this function.
fn insert_at(target: &mut Map<String, Value>, tokens: &[String], value: Value) {
    let (last, parents) = tokens.split_last().expect("validated destinations are not the root");
    let mut current = target;
    for token in parents {
        current = current
            .entry(token.clone())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("non-overlapping destinations only traverse objects");
    }
    current.insert(last.clone(), value);
}

fn pointer_tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const IDENTITY: &str = "components.k8s.nyl/v1 example/v1/PostgresDb app/app-db";

    fn contract(source: &str) -> ComponentContract {
        ComponentContract::parse(source, "nyl-component.yaml").unwrap()
    }

    fn postgres_contract() -> ComponentContract {
        contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - name: scheduling
    from: /postgres/scheduling
    to: /scheduling
  - name: storageClass
    from: /postgres/storageClass
    to: /storage/storageClass
  - name: backup
    from: /postgres/backup
    to: /backup
",
        )
    }

    fn platform_values() -> Value {
        json!({
            "postgres": {
                "scheduling": {"nodeSelector": {"pool": "compute"}, "tolerations": [{"key": "pool"}]},
                "storageClass": "local-path-retain",
                "backup": {"destinationPath": "s3://backups/pg", "retentionPolicy": "30d"}
            },
            "unrelated": {"secretish": "not imported"}
        })
    }

    fn target(values: &Value) -> ImportScope<'_> {
        ImportScope::Target {
            name: "production",
            values,
        }
    }

    fn error(result: Result<Value>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn test_apply_value_imports_without_contract_returns_explicit_values_unchanged() {
        let explicit = json!({"storage": {"size": "20Gi"}});
        let values = platform_values();
        let result = apply_value_imports(None, None, IDENTITY, target(&values), explicit.clone()).unwrap();
        assert_eq!(result, explicit);
    }

    #[test]
    fn test_apply_value_imports_maps_only_declared_selections_beneath_explicit_values() {
        let values = platform_values();
        let explicit = json!({
            "storage": {"size": "20Gi"},
            "backup": {"retentionPolicy": "7d"}
        });
        let result =
            apply_value_imports(Some(&postgres_contract()), None, IDENTITY, target(&values), explicit).unwrap();
        assert_eq!(
            result,
            json!({
                "scheduling": {"nodeSelector": {"pool": "compute"}, "tolerations": [{"key": "pool"}]},
                "storage": {"storageClass": "local-path-retain", "size": "20Gi"},
                "backup": {"destinationPath": "s3://backups/pg", "retentionPolicy": "7d"}
            })
        );
    }

    #[test]
    fn test_apply_value_imports_keeps_explicit_false_zero_empty_array_and_null() {
        let contract = contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: enabled, from: /pg/enabled, to: /enabled}
  - {name: replicas, from: /pg/replicas, to: /replicas}
  - {name: tolerations, from: /pg/tolerations, to: /tolerations}
  - {name: backup, from: /pg/backup, to: /backup}
  - {name: labels, from: /pg/labels, to: /labels}
",
        );
        let values = json!({"pg": {
            "enabled": true,
            "replicas": 3,
            "tolerations": [{"key": "pool"}],
            "backup": {"retentionPolicy": "30d"},
            "labels": {"team": "platform"}
        }});
        let explicit = json!({
            "enabled": false,
            "replicas": 0,
            "tolerations": [],
            "backup": null,
            "labels": {}
        });
        let result = apply_value_imports(Some(&contract), None, IDENTITY, target(&values), explicit).unwrap();
        assert_eq!(
            result,
            json!({
                "enabled": false,
                "replicas": 0,
                "tolerations": [],
                "backup": null,
                "labels": {"team": "platform"}
            })
        );
    }

    #[test]
    fn test_apply_value_imports_imports_present_null_and_falsy_sources() {
        let contract = contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: backup, from: /pg/backup, to: /backup, required: true}
  - {name: enabled, from: /pg/enabled, to: /enabled, required: true}
  - {name: zones, from: /pg/zones, to: /zones, required: true}
",
        );
        let values = json!({"pg": {"backup": null, "enabled": false, "zones": []}});
        let result = apply_value_imports(Some(&contract), None, IDENTITY, target(&values), json!({})).unwrap();
        assert_eq!(result, json!({"backup": null, "enabled": false, "zones": []}));
    }

    #[test]
    fn test_apply_value_imports_skips_absent_optional_source() {
        let values = json!({"postgres": {"storageClass": "fast"}});
        let result =
            apply_value_imports(Some(&postgres_contract()), None, IDENTITY, target(&values), json!({})).unwrap();
        assert_eq!(result, json!({"storage": {"storageClass": "fast"}}));
    }

    #[test]
    fn test_apply_value_imports_reports_absent_required_source_even_when_explicitly_set() {
        let contract = contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: backup, from: /postgres/backup, to: /backup, required: true}
",
        );
        let values = json!({});
        let message = error(apply_value_imports(
            Some(&contract),
            None,
            IDENTITY,
            target(&values),
            json!({"backup": {"destinationPath": "s3://explicit"}}),
        ));
        assert!(message.contains(IDENTITY), "{message}");
        assert!(message.contains(r#"required value import "backup" (/postgres/backup -> /backup)"#));
        assert!(message.contains(r#"target "production""#), "{message}");
    }

    #[test]
    fn test_apply_value_imports_without_target_skips_optional_and_rejects_required() {
        let explicit = json!({"storage": {"size": "1Gi"}});
        let result = apply_value_imports(
            Some(&postgres_contract()),
            None,
            IDENTITY,
            ImportScope::Standalone,
            explicit.clone(),
        )
        .unwrap();
        assert_eq!(result, explicit);

        let required = contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: backup, from: /postgres/backup, to: /backup, required: true}
",
        );
        let message = error(apply_value_imports(
            Some(&required),
            None,
            IDENTITY,
            ImportScope::Standalone,
            json!({}),
        ));
        assert!(message.contains("this render has no target"), "{message}");
    }

    #[test]
    fn test_apply_value_imports_honors_invocation_opt_outs() {
        let values = platform_values();
        let all_off = ComponentValueImportOptions {
            enabled: false,
            exclude: Vec::new(),
        };
        let result = apply_value_imports(
            Some(&postgres_contract()),
            Some(&all_off),
            IDENTITY,
            target(&values),
            json!({}),
        )
        .unwrap();
        assert_eq!(result, json!({}));

        let required = contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: backup, from: /missing, to: /backup, required: true}
  - {name: storageClass, from: /postgres/storageClass, to: /storageClass}
",
        );
        let skip_backup = ComponentValueImportOptions {
            enabled: true,
            exclude: vec!["backup".to_string()],
        };
        let result = apply_value_imports(
            Some(&required),
            Some(&skip_backup),
            IDENTITY,
            target(&values),
            json!({}),
        )
        .unwrap();
        assert_eq!(result, json!({"storageClass": "local-path-retain"}));
    }

    #[test]
    fn test_apply_value_imports_rejects_unknown_excluded_import() {
        let values = platform_values();
        let options = ComponentValueImportOptions {
            enabled: true,
            exclude: vec!["backups".to_string()],
        };
        let message = error(apply_value_imports(
            Some(&postgres_contract()),
            Some(&options),
            IDENTITY,
            target(&values),
            json!({}),
        ));
        assert!(
            message.contains(r#"unknown import "backups"; it declares scheduling, storageClass, backup"#),
            "{message}"
        );
        let message = error(apply_value_imports(
            None,
            Some(&options),
            IDENTITY,
            target(&values),
            json!({}),
        ));
        assert!(message.contains("it declares no value imports"), "{message}");
    }

    #[test]
    fn test_apply_value_imports_rejects_explicit_non_object_on_destination_path() {
        let values = platform_values();
        let message = error(apply_value_imports(
            Some(&postgres_contract()),
            None,
            IDENTITY,
            target(&values),
            json!({"storage": "20Gi"}),
        ));
        assert!(
            message.contains(r#"value import "storageClass" writes /storage/storageClass, but the invocation sets /storage to a string"#),
            "{message}"
        );

        let result = apply_value_imports(
            Some(&postgres_contract()),
            None,
            IDENTITY,
            target(&values),
            json!({"storage": null}),
        )
        .unwrap();
        assert_eq!(result["storage"], Value::Null);
    }

    #[test]
    fn test_apply_value_imports_unescapes_pointer_tokens() {
        let contract = contract(
            r#"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: pool, from: "/scheduling.example.com~1pool", to: "/nodeSelector/scheduling.example.com~1pool"}
"#,
        );
        let values = json!({"scheduling.example.com/pool": "compute"});
        let result = apply_value_imports(Some(&contract), None, IDENTITY, target(&values), json!({})).unwrap();
        assert_eq!(
            result,
            json!({"nodeSelector": {"scheduling.example.com/pool": "compute"}})
        );
    }

    #[test]
    fn test_contract_parse_rejects_invalid_declarations() {
        for (source, expected) in [
            ("apiVersion: components.nyl/v1\n", "apiVersion must be"),
            (
                "apiVersion: components.nyl/v1alpha1\nvalueImports:\n  - {name: a, from: postgres, to: /a}\n",
                "valueImports[0].from must be a JSON Pointer",
            ),
            (
                "apiVersion: components.nyl/v1alpha1\nvalueImports:\n  - {name: a, from: /a, to: \"\"}\n",
                "valueImports[0].to must select a value beneath the document root",
            ),
            (
                "apiVersion: components.nyl/v1alpha1\nvalueImports:\n  - {name: a, from: /a, to: /a}\n  - {name: a, from: /b, to: /b}\n",
                r#"valueImports[1].name "a" is declared twice"#,
            ),
            (
                "apiVersion: components.nyl/v1alpha1\nvalueImports:\n  - {name: a, from: /a, to: /backup}\n  - {name: b, from: /b, to: /backup/retention}\n",
                r#"overlaps the destination of import "a""#,
            ),
            (
                "apiVersion: components.nyl/v1alpha1\nvalueImports:\n  - {name: a, from: /a, to: /a, default: 1}\n",
                "unknown field",
            ),
        ] {
            let message = ComponentContract::parse(source, "nyl-component.yaml")
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{source}: {message}");
        }
    }

    #[test]
    fn test_contract_parse_allows_sibling_destinations_with_shared_prefix_text() {
        contract(
            r"
apiVersion: components.nyl/v1alpha1
valueImports:
  - {name: a, from: /a, to: /backup}
  - {name: b, from: /b, to: /backupRetention}
",
        );
    }

    #[test]
    fn test_contract_load_ignores_charts_without_contract_and_rejects_symlinks() {
        let chart = tempfile::TempDir::new().unwrap();
        assert_eq!(ComponentContract::load(chart.path()).unwrap(), None);

        let elsewhere = chart.path().join("elsewhere.yaml");
        std::fs::write(&elsewhere, "apiVersion: components.nyl/v1alpha1\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&elsewhere, chart.path().join(CONTRACT_FILE)).unwrap();
            let message = ComponentContract::load(chart.path()).unwrap_err().to_string();
            assert!(message.contains("must not be a symbolic link"), "{message}");
        }
    }
}
