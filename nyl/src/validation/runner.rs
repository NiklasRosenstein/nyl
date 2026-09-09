//! Command-independent validation of final artifacts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{
    resolve::{self, Origin, SchemaDocument, SchemaResolver},
    schemas, store, ValidationArgs,
};
use crate::config::ProjectConfig;
use crate::gitops::{resolve_cluster_contract, CompiledTargetTree, EffectiveCluster, GitOpsInventory};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// A final Kubernetes document and its stable source identity.
pub struct ValidationDocument {
    pub manifest: Value,
    pub source: String,
}

/// Documents installed into one destination, with independently resolved schema provenance.
pub struct ValidationPartition {
    pub destination: String,
    pub version: String,
    pub schema_source: Option<String>,
    pub schema_capabilities_fingerprint: Option<String>,
    pub documents: Vec<ValidationDocument>,
}

/// Structured diagnostics are independent of the concrete validator process.
#[derive(Debug)]
pub struct ValidationDiagnostic {
    pub validator: String,
    pub destination: String,
    pub source: String,
    pub resource: String,
    pub message: String,
}

#[derive(Debug, Default)]
struct ValidationReport {
    valid: usize,
    skipped: usize,
    diagnostics: Vec<ValidationDiagnostic>,
}

trait ManifestValidator {
    async fn validate(
        &self,
        stage: &Path,
        files: &BTreeMap<PathBuf, (String, String)>,
        partition: &ValidationPartition,
    ) -> Result<ValidationReport>;
}

struct KubeconformValidator<'a> {
    executable: &'a Path,
    strict: bool,
}

#[derive(Deserialize)]
struct ToolReport {
    resources: Vec<ToolResource>,
}

#[derive(Deserialize)]
struct ToolResource {
    filename: String,
    status: String,
    #[serde(default)]
    msg: String,
}

impl ManifestValidator for KubeconformValidator<'_> {
    async fn validate(
        &self,
        stage: &Path,
        files: &BTreeMap<PathBuf, (String, String)>,
        partition: &ValidationPartition,
    ) -> Result<ValidationReport> {
        if files.is_empty() {
            return Ok(ValidationReport::default());
        }
        let mut command = tokio::process::Command::new(self.executable);
        command
            .kill_on_drop(true)
            .args([
                "-output",
                "json",
                "-verbose",
                "-summary",
                "-kubernetes-version",
                &partition.version,
            ])
            .arg("-schema-location")
            .arg(stage.join("schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json"));
        if self.strict {
            command.arg("-strict");
        }
        // A directory avoids platform argument-length limits on large targets.
        command.arg(stage.join("manifests"));
        let output = command.output().await.map_err(|error| {
            NylError::process(format!(
                "Cannot run kubeconform: {error}; install the version pinned in mise.toml or use the Nyl CI image"
            ))
        })?;
        let tool: ToolReport = serde_json::from_slice(&output.stdout).map_err(|_| {
            NylError::process(format!(
                "kubeconform returned malformed JSON (status {})",
                output.status
            ))
        })?;
        let mut report = ValidationReport::default();
        let mut observed = BTreeSet::new();
        for resource in tool.resources {
            let path = PathBuf::from(&resource.filename);
            let (source, identity) = files
                .get(&path)
                .ok_or_else(|| NylError::process("kubeconform returned an unknown manifest path"))?;
            if !observed.insert(path) {
                return Err(NylError::process("kubeconform returned duplicate manifest results"));
            }
            match resource.status.as_str() {
                "statusValid" | "VALID" => report.valid += 1,
                "statusInvalid" | "statusError" | "INVALID" | "ERROR" => {
                    report.diagnostics.push(ValidationDiagnostic {
                        validator: "kubeconform".to_owned(),
                        destination: partition.destination.clone(),
                        source: source.clone(),
                        resource: identity.clone(),
                        message: resource.msg,
                    });
                }
                _ => {
                    return Err(NylError::process(
                        "kubeconform returned an unexpected validation status",
                    ))
                }
            }
        }
        if observed.len() != files.len() || (!output.status.success() && report.diagnostics.is_empty()) {
            return Err(NylError::process(
                "kubeconform did not validate every submitted resource",
            ));
        }
        Ok(report)
    }
}

fn partition_for(inventory: &GitOpsInventory, effective: EffectiveCluster) -> Result<ValidationPartition> {
    let capabilities = effective
        .cluster
        .spec
        .kubernetes
        .as_ref()
        .ok_or_else(|| NylError::config("Unresolved Cluster capabilities"))?;
    let version = capabilities
        .kube_version
        .as_deref()
        .ok_or_else(|| NylError::config("Validation requires Cluster.spec.kubernetes.kubeVersion"))?;
    let source = resolve_cluster_contract(inventory, &effective.schemas_source)?;
    let source_capabilities = source
        .cluster
        .spec
        .kubernetes
        .as_ref()
        .ok_or_else(|| NylError::config("Schema source requires local capabilities"))?;
    Ok(ValidationPartition {
        destination: effective.cluster.metadata.name,
        version: resolve::normalize_version(version)?,
        schema_source: Some(effective.schemas_source),
        schema_capabilities_fingerprint: Some(store::capabilities_fingerprint(source_capabilities)?),
        documents: Vec::new(),
    })
}

/// Build partitions using installation destinations, not Application.spec.destination.
pub fn tree_partitions(inventory: &GitOpsInventory, compiled: &CompiledTargetTree) -> Result<Vec<ValidationPartition>> {
    let workload = compiled.target.cluster_name();
    let catalog = if let Some(reference) = &compiled.target.spec.argocd_ref {
        match inventory
            .get(GitOpsResourceKind::ArgoCDInstance, &reference.name)
            .and_then(|resource| resource.resource.as_ref())
        {
            Some(GitOpsResource::ArgoCDInstance(instance)) => instance.spec.cluster_ref.name.as_str(),
            _ => return Err(NylError::config("ArgoCDInstance must resolve before validation")),
        }
    } else {
        workload
    };
    let mut partitions = BTreeMap::new();
    for name in [workload, catalog] {
        partitions.insert(
            name.to_owned(),
            partition_for(inventory, resolve_cluster_contract(inventory, name)?)?,
        );
    }
    for (path, bytes) in &compiled.files {
        if !path
            .extension()
            .is_some_and(|extension| extension == "yaml" || extension == "yml")
        {
            continue;
        }
        let destination = if path.starts_with("_nyl/catalog") {
            catalog
        } else {
            workload
        };
        let partition = partitions.get_mut(destination).expect("destination was inserted");
        for (index, manifest) in serde_saphyr::read::<_, Value>(&mut bytes.as_slice()).enumerate() {
            partition.documents.push(ValidationDocument {
                manifest: manifest?,
                source: format!("{} (document {})", path.display(), index + 1),
            });
        }
    }
    Ok(partitions.into_values().collect())
}

/// Validate a complete compiled target, including cache hits and generated catalog objects.
pub async fn validate_tree(
    args: &ValidationArgs,
    inventory: &GitOpsInventory,
    compiled: &CompiledTargetTree,
) -> Result<()> {
    if !args.enabled(&inventory.project_config.config.validation)? {
        return Ok(());
    }
    let partitions = tree_partitions(inventory, compiled)?;
    validate_partitions(args, &inventory.project_config, &inventory.project_root, &partitions).await
}

/// Validate exactly the documents a file-based command emits, diffs, or applies.
pub async fn validate_manifests(
    args: &ValidationArgs,
    config: &ProjectConfig,
    project: &Path,
    cluster: Option<&str>,
    explicit_version: Option<&str>,
    manifests: &[Value],
    source: &str,
) -> Result<()> {
    if !args.enabled(&config.config.validation)? {
        return Ok(());
    }
    let mut partition = if let Some(cluster) = cluster {
        let inventory = crate::gitops::discover_gitops_inventory(project, None)?;
        partition_for(&inventory, resolve_cluster_contract(&inventory, cluster)?)?
    } else {
        ValidationPartition {
            destination: "targetless".to_owned(),
            version: resolve::normalize_version(
                explicit_version
                    .ok_or_else(|| NylError::config("Targetless validation requires --offline --kube-version"))?,
            )?,
            schema_source: None,
            schema_capabilities_fingerprint: None,
            documents: Vec::new(),
        }
    };
    partition.documents = manifests
        .iter()
        .enumerate()
        .map(|(index, manifest)| ValidationDocument {
            manifest: manifest.clone(),
            source: format!("{source} (document {})", index + 1),
        })
        .collect();
    validate_partitions(args, config, project, &[partition]).await
}

async fn validate_partitions(
    args: &ValidationArgs,
    config: &ProjectConfig,
    project: &Path,
    partitions: &[ValidationPartition],
) -> Result<()> {
    let settings = config
        .config
        .validation
        .kubeconform
        .as_ref()
        .expect("enabled validates selection");
    let root = store::vendor_root(project, config)?;
    let mut resolver = SchemaResolver::new(project, root, settings, false, false)?;
    let operation = async {
        let mut combined = ValidationReport::default();
        for partition in partitions {
            if let Some(source) = &partition.schema_source {
                eprintln!(
                    "Validating {} with CRD snapshot from {} (Kubernetes {})",
                    partition.destination, source, partition.version
                );
            }
            let stage = tempfile::TempDir::new()?;
            let (files, skipped) = prepare_partition(args, partition, &mut resolver, stage.path(), false).await?;
            let validator = KubeconformValidator {
                executable: Path::new("kubeconform"),
                strict: settings.strict,
            };
            let report = validator.validate(stage.path(), &files, partition).await?;
            combined.valid += report.valid;
            combined.skipped += skipped;
            combined.diagnostics.extend(report.diagnostics);
        }
        for diagnostic in &combined.diagnostics {
            eprintln!(
                "{}: {}: {}: {}: {}",
                diagnostic.validator,
                diagnostic.destination,
                diagnostic.source,
                diagnostic.resource,
                diagnostic.message
            );
        }
        eprintln!(
            "Validation: {} valid, {} failed, {} skipped",
            combined.valid,
            combined.diagnostics.len(),
            combined.skipped
        );
        if combined.diagnostics.is_empty() {
            Ok(())
        } else {
            Err(NylError::validation(format!(
                "{} resource(s) failed kubeconform validation",
                combined.diagnostics.len()
            )))
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(settings.timeout_seconds), operation)
        .await
        .map_err(|_| {
            NylError::validation(format!(
                "kubeconform validation timed out after {} seconds",
                settings.timeout_seconds
            ))
        })?
}

/// Flatten validation inputs while keeping item paths for diagnostics and desired CRD discovery.
fn expand_documents(documents: &[ValidationDocument], skip: &[String]) -> Result<Vec<ValidationDocument>> {
    fn expand(manifest: &Value, source: String, skip: &[String], output: &mut Vec<ValidationDocument>) -> Result<()> {
        let kind = manifest.get("kind").and_then(Value::as_str).unwrap_or("");
        let api = manifest.get("apiVersion").and_then(Value::as_str).unwrap_or("");
        if kind.eq_ignore_ascii_case("list") && !skip.contains(&format!("{api}/{kind}")) {
            let items = manifest
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| NylError::validation(format!("{source}: List requires an items array")))?;
            for (index, item) in items.iter().enumerate() {
                expand(item, format!("{source}.items[{index}]"), skip, output)?;
            }
        } else {
            output.push(ValidationDocument {
                manifest: manifest.clone(),
                source,
            });
        }
        Ok(())
    }
    let mut output = Vec::new();
    for document in documents {
        expand(&document.manifest, document.source.clone(), skip, &mut output)?;
    }
    Ok(output)
}

async fn prepare_partition(
    args: &ValidationArgs,
    partition: &ValidationPartition,
    resolver: &mut SchemaResolver<'_>,
    stage: &Path,
    builtin_only: bool,
) -> Result<(BTreeMap<PathBuf, (String, String)>, usize)> {
    let documents = expand_documents(&partition.documents, &resolver.settings.skip)?;
    let inputs = PartitionSchemas::new(args, partition, resolver, &documents)?;
    let mut files = BTreeMap::new();
    let mut resolved = BTreeSet::new();
    let mut skipped = 0;
    for (number, document) in documents.iter().enumerate() {
        let api = document
            .manifest
            .get("apiVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| NylError::validation(format!("{}: missing apiVersion", document.source)))?;
        let kind = document
            .manifest
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| NylError::validation(format!("{}: missing kind", document.source)))?;
        let gvk = format!("{api}/{kind}");
        resolve::resource_parts(&gvk)?;
        if resolver.settings.skip.contains(&gvk) {
            skipped += 1;
            continue;
        }
        if resolved.insert(gvk.clone()) {
            let Some(schema) = inputs.resolve(&gvk, partition, resolver, builtin_only).await? else {
                continue;
            };
            resolver
                .materialize(schema, stage.join(resolve::schema_relative_path(&gvk)?), stage)
                .await?;
        }
        if !builtin_only {
            let path = stage.join("manifests").join(format!("{number}.yaml"));
            store::atomic_write(
                &path,
                crate::yaml::serialize_yaml_value(&document.manifest)
                    .map_err(NylError::YamlEmit)?
                    .as_bytes(),
            )?;
            let namespace = document
                .manifest
                .pointer("/metadata/namespace")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = document
                .manifest
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>");
            files.insert(path, (document.source.clone(), format!("{gvk} {namespace}/{name}")));
        }
    }
    Ok((files, skipped))
}

struct PartitionSchemas {
    desired: BTreeMap<String, schemas::CrdSchemas>,
    captured: Option<store::ClusterSchemaIndex>,
}

impl PartitionSchemas {
    fn new(
        args: &ValidationArgs,
        partition: &ValidationPartition,
        resolver: &SchemaResolver<'_>,
        documents: &[ValidationDocument],
    ) -> Result<Self> {
        let desired = if args.use_desired_crds {
            schemas::extract_crds(
                &documents
                    .iter()
                    .map(|document| document.manifest.clone())
                    .collect::<Vec<_>>(),
            )?
        } else {
            BTreeMap::new()
        };
        let captured = partition
            .schema_source
            .as_deref()
            .map(|name| store::read_cluster_index(&resolver.vendor, name))
            .transpose()?
            .flatten();
        if let Some(index) = &captured {
            if Some(&index.capabilities_fingerprint) != partition.schema_capabilities_fingerprint.as_ref() {
                return Err(NylError::validation(format!(
                    "Schema snapshot for {} does not match its source capabilities; run nyl capture cluster {} --crds",
                    index.cluster, index.cluster
                )));
            }
        }
        Ok(Self { desired, captured })
    }

    async fn resolve(
        &self,
        gvk: &str,
        partition: &ValidationPartition,
        resolver: &mut SchemaResolver<'_>,
        builtin_only: bool,
    ) -> Result<Option<SchemaDocument>> {
        let (api, kind) = resolve::resource_parts(gvk)?;
        let (group, version) = api.split_once('/').unwrap_or(("", api));
        let desired_crd = self.desired.values().find(|crd| crd.group == group && crd.kind == kind);
        let captured_crd = self
            .captured
            .as_ref()
            .and_then(|index| index.crds.values().find(|crd| crd.group == group && crd.kind == kind));
        let schema = if let Some(crd) = desired_crd {
            let variants = crd
                .versions
                .get(version)
                .ok_or_else(|| NylError::validation(format!("{gvk}: desired CRD does not serve this version")))?;
            SchemaDocument {
                value: if resolver.settings.strict {
                    &variants.strict
                } else {
                    &variants.permissive
                }
                .clone(),
                origin: Origin::Captured,
            }
        } else if let Some(local) = resolver.local(gvk, &partition.version)? {
            local
        } else if let Some(crd) = captured_crd {
            let variants = crd
                .versions
                .get(version)
                .ok_or_else(|| NylError::validation(format!("{gvk}: captured CRD does not serve this version")))?;
            let hash = if resolver.settings.strict {
                &variants.strict
            } else {
                &variants.permissive
            };
            SchemaDocument {
                value: serde_json::from_slice(&store::read_blob(&resolver.vendor, hash)?)?,
                origin: Origin::Captured,
            }
        } else {
            if !resolve::is_builtin_group(group) {
                if builtin_only {
                    return Ok(None);
                }
                let capture_hint = partition
                    .schema_source
                    .as_deref()
                    .map(|source| format!("run nyl capture cluster {source} --crds or "))
                    .unwrap_or_default();
                return Err(NylError::validation(format!(
                    "no schema for {gvk} in destination {}; {capture_hint}configure schema_locations",
                    partition.destination
                )));
            }
            match resolver.builtin_resource(gvk, &partition.version).await? {
                Some(schema) => schema,
                None => {
                    return Err(NylError::validation(format!(
                        "no schema for {gvk} in destination {}; configure schema_locations",
                        partition.destination
                    )))
                }
            }
        };
        Ok(Some(schema))
    }
}

/// Materialize or verify builtin schema dependencies without invoking validators or live clusters.
pub async fn vendor_schemas(
    inventory: &GitOpsInventory,
    compiled: &[CompiledTargetTree],
    check: bool,
    preserve: bool,
    refresh: bool,
) -> Result<()> {
    let root = store::vendor_root(&inventory.project_root, &inventory.project_config)?;
    let settings = inventory.project_config.config.validation.kubeconform.as_ref();
    if let Some(settings) = settings.filter(|settings| settings.vendor_builtin_schemas) {
        let _lock = if check { None } else { Some(store::lock(&root)?) };
        let mut resolver =
            SchemaResolver::new(&inventory.project_root, root.clone(), settings, !check, check)?.with_refresh(refresh);
        for tree in compiled {
            for partition in tree_partitions(inventory, tree)? {
                let stage = tempfile::TempDir::new()?;
                // Schema discovery does not assert a complete desired CRD delivery scope.
                prepare_partition(
                    &ValidationArgs::default(),
                    &partition,
                    &mut resolver,
                    stage.path(),
                    true,
                )
                .await?;
            }
        }
        if !check {
            let mut index = if preserve {
                store::read_builtins(&root)?
            } else {
                store::BuiltinIndex {
                    version: 1,
                    ..store::BuiltinIndex::default()
                }
            };
            index.schemas.extend(resolver.observed_builtins);
            store::atomic_write(
                &store::safe_path(&root, Path::new("schemas/builtins.json"))?,
                &store::json_bytes(&index)?,
            )?;
        }
    }
    store::check_and_prune(&root, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn crd(value_type: &str) -> Value {
        json!({"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com"},"spec":{"group":"example.com","names":{"kind":"Widget"},
            "versions":[{"name":"v1","served":true,"schema":{"openAPIV3Schema":{"type":"object","properties":{
                "spec":{"type":"object","properties":{"count":{"type":value_type}},"required":["count"]}
            },"required":["spec"]}}}]}})
    }

    fn document(manifest: Value) -> ValidationDocument {
        ValidationDocument {
            manifest,
            source: "workload.yaml (document 1)".into(),
        }
    }

    fn fixture() -> (TempDir, ProjectConfig, ValidationPartition) {
        let directory = TempDir::new().unwrap();
        std::fs::write(directory.path().join("nyl.toml"),
            "[validation]\nenabled=true\n[validation.kubeconform]\nvendor_builtin_schemas=true\nschema_locations=['schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json']\n").unwrap();
        let config = ProjectConfig::load_from_dir(None, Some(directory.path())).unwrap();
        let definitions = schemas::extract_crds(&[crd("integer")]).unwrap();
        let schema = &definitions["widgets.example.com"].versions["v1"].strict;
        store::atomic_write(
            &directory.path().join("schemas/example.com/widget_v1.json"),
            &store::json_bytes(schema).unwrap(),
        )
        .unwrap();
        store::atomic_write(
            &directory
                .path()
                .join("schemas/apiextensions.k8s.io/customresourcedefinition_v1.json"),
            b"{}",
        )
        .unwrap();
        let partition = ValidationPartition {
            destination: "production".into(),
            version: "1.31.4".into(),
            schema_source: None,
            schema_capabilities_fingerprint: None,
            documents: vec![document(
                json!({"apiVersion":"example.com/v1","kind":"Widget","metadata":{"name":"example"},"spec":{"count":1}}),
            )],
        };
        (directory, config, partition)
    }

    #[tokio::test]
    async fn test_real_kubeconform_accepts_valid_and_rejects_invalid_final_documents() {
        let (directory, config, mut partition) = fixture();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        partition.documents[0].manifest["spec"]["count"] = json!("invalid");
        let error = validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("1 resource(s) failed"));
    }

    #[tokio::test]
    async fn test_crd_exclusive_and_inclusive_numeric_bounds_in_both_modes() {
        let (directory, mut config, mut partition) = fixture();
        let args = ValidationArgs {
            use_desired_crds: true,
            ..ValidationArgs::default()
        };
        for strict in [true, false] {
            config.config.validation.kubeconform.as_mut().unwrap().strict = strict;
            for exclusive in [true, false] {
                let mut definition = crd("number");
                definition["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
                    ["count"] = json!({
                    "type":"number", "minimum":0, "maximum":2,
                    "exclusiveMinimum":exclusive, "exclusiveMaximum":exclusive
                });
                partition.documents.truncate(1);
                partition.documents.push(document(definition));
                for count in [1, 0, 2, -1, 3] {
                    partition.documents[0].manifest["spec"]["count"] = json!(count);
                    let result =
                        validate_partitions(&args, &config, directory.path(), std::slice::from_ref(&partition)).await;
                    let expected = if exclusive {
                        count == 1
                    } else {
                        (0..=2).contains(&count)
                    };
                    assert_eq!(
                        result.is_ok(),
                        expected,
                        "strict={strict}, exclusive={exclusive}, count={count}: {result:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn test_list_items_supply_desired_crds_and_keep_item_diagnostics() {
        let (directory, mut config, mut partition) = fixture();
        let widget =
            json!({"apiVersion":"example.com/v1","kind":"Widget","metadata":{"name":"test"},"spec":{"count":"new"}});
        partition.documents = vec![document(
            json!({"apiVersion":"v1","kind":"List","metadata":{"name":"batch"},
            "items":[crd("string"), {"apiVersion":"v1","kind":"List","items":[widget]}]}),
        )];
        let args = ValidationArgs {
            use_desired_crds: true,
            ..ValidationArgs::default()
        };
        validate_partitions(&args, &config, directory.path(), std::slice::from_ref(&partition))
            .await
            .unwrap();
        let settings = config.config.validation.kubeconform.as_ref().unwrap();
        let mut resolver = SchemaResolver::new(
            directory.path(),
            directory.path().join("vendor"),
            settings,
            false,
            false,
        )
        .unwrap();
        let stage = TempDir::new().unwrap();
        let (files, skipped) = prepare_partition(&args, &partition, &mut resolver, stage.path(), false)
            .await
            .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(skipped, 0);
        assert!(files.values().any(|(source, _)| source.ends_with(".items[1].items[0]")));
        partition.documents[0].manifest["items"][1]["items"][0]["spec"]["count"] = json!(123);
        assert!(
            validate_partitions(&args, &config, directory.path(), std::slice::from_ref(&partition))
                .await
                .is_err()
        );
        config.config.validation.kubeconform.as_mut().unwrap().skip = vec!["example.com/v1/Widget".into()];
        validate_partitions(&args, &config, directory.path(), std::slice::from_ref(&partition))
            .await
            .unwrap();
        config.config.validation.kubeconform.as_mut().unwrap().skip = vec!["v1/List".into()];
        validate_partitions(&args, &config, directory.path(), &[partition])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_converted_crd_composition_nullable_enum_and_embedded_metadata() {
        let (directory, config, mut partition) = fixture();
        let mut definition = crd("integer");
        definition["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"] = json!({
            "type":"object", "properties":{
                "a":{"type":"string"}, "b":{"type":"string"}, "empty":{"type":"object"},
                "optional":{"type":"string","nullable":true,"enum":["yes"]},
                "nullable":{"type":"string","nullable":true,"enum":["yes",null]},
                "embedded":{"type":"object","x-kubernetes-embedded-resource":true,
                    "x-kubernetes-preserve-unknown-fields":true}
            }, "allOf":[{"properties":{"a":{"minLength":1}}}]
        });
        let definitions = schemas::extract_crds(&[definition]).unwrap();
        store::atomic_write(
            &directory.path().join("schemas/example.com/widget_v1.json"),
            &store::json_bytes(&definitions["widgets.example.com"].versions["v1"].strict).unwrap(),
        )
        .unwrap();
        let valid = json!({"a":"x","b":"y","empty":{},"optional":"yes","nullable":null,
            "embedded":{"apiVersion":"v1","kind":"Pod"}});
        partition.documents[0].manifest["spec"] = valid.clone();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        for (field, value) in [
            ("a", json!("")),
            ("empty", json!({"unknown": true})),
            ("optional", Value::Null),
            (
                "embedded",
                json!({"apiVersion":"v1","kind":"Pod","metadata":{"labels":"broken"}}),
            ),
            ("embedded", json!({"kind":"Pod"})),
        ] {
            partition.documents[0].manifest["spec"] = valid.clone();
            partition.documents[0].manifest["spec"][field] = value;
            assert!(
                validate_partitions(
                    &ValidationArgs::default(),
                    &config,
                    directory.path(),
                    std::slice::from_ref(&partition)
                )
                .await
                .is_err(),
                "accepted invalid {field}"
            );
        }
    }

    #[tokio::test]
    async fn test_fragment_targets_stage_all_dependencies_and_preserve_literal_defaults() {
        let (directory, config, mut partition) = fixture();
        let schemas = directory.path().join("schemas/example.com");
        std::fs::rename(schemas.join("widget_v1.json"), schemas.join("definition.json")).unwrap();
        std::fs::write(
            schemas.join("widget_v1.json"),
            serde_json::to_vec(&json!({
                "$ref":"#/components/schemas/%57idget",
                "default":{"$ref":"https://example.invalid/literal-not-a-schema"},
                "components":{"schemas":{"Widget":{"allOf":[
                    {"$ref":"import.json#/schemas/A"}, {"$ref":"import.json#/schemas/B"},
                    {"properties":{"next":{"$ref":"#/components/schemas/Widget"}}}
                ]}}}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            schemas.join("import.json"),
            serde_json::to_vec(&json!({"schemas":{
                "A":{"$ref":"definition.json"}, "B":{"$ref":"#/schemas/Constraint"},
                "Constraint":{"properties":{"spec":{"properties":{"count":{"minimum":1}}}}}
            }}))
            .unwrap(),
        )
        .unwrap();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        partition.documents[0].manifest["spec"]["count"] = json!(0);
        assert!(
            validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_fragment_references_respect_nested_schema_resource_scopes() {
        let (directory, config, mut partition) = fixture();
        let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"spec":{
            "$id":"spec.json", "$ref":"#/$defs/Spec", "$defs":{
                "Spec":{"type":"object","properties":{"count":{"type":"integer"}}}
            }
        }}});
        std::fs::write(
            directory.path().join("schemas/example.com/widget_v1.json"),
            serde_json::to_vec(&schema).unwrap(),
        )
        .unwrap();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        partition.documents[0].manifest["spec"]["count"] = json!("invalid");
        assert!(
            validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_fragment_dependencies_cannot_escape_the_project() {
        let (directory, config, partition) = fixture();
        let outside = TempDir::new().unwrap();
        let external = outside.path().join("schema.json");
        std::fs::write(&external, b"{}").unwrap();
        for reference in [
            reqwest::Url::from_file_path(&external).unwrap().to_string(),
            "https://example.invalid/schema.json".to_owned(),
        ] {
            let schema =
                json!({"$ref":"#/components/schemas/Widget", "components":{"schemas":{"Widget":{"$ref":reference}}}});
            std::fs::write(
                directory.path().join("schemas/example.com/widget_v1.json"),
                serde_json::to_vec(&schema).unwrap(),
            )
            .unwrap();
            let error = validate_partitions(
                &ValidationArgs::default(),
                &config,
                directory.path(),
                std::slice::from_ref(&partition),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("must stay within the project"));
        }
    }

    #[tokio::test]
    async fn test_schema_dependencies_are_staged_and_cannot_fetch_external_urls() {
        let (directory, config, partition) = fixture();
        let path = directory.path().join("schemas/example.com/widget_v1.json");
        std::fs::write(
            directory.path().join("schemas/example.com/dependency.json"),
            r#"{"properties":{"spec":{"properties":{"count":{"type":"integer"}}}}}"#,
        )
        .unwrap();
        std::fs::write(&path, r#"{"dependencies":{"spec":{"$ref":"dependency.json"}}}"#).unwrap();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        std::fs::write(
            &path,
            r#"{"dependencies":{"spec":{"$ref":"https://example.invalid/schema.json"}}}"#,
        )
        .unwrap();
        let error = validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must stay within the project"));
    }

    #[tokio::test]
    async fn test_desired_crds_require_explicit_scope_and_mask_removed_versions() {
        let (directory, config, mut partition) = fixture();
        std::fs::remove_file(directory.path().join("schemas/example.com/widget_v1.json")).unwrap();
        let root = directory.path().join("vendor");
        let capabilities = crate::resources::ClusterKubernetesCapabilities {
            kube_version: Some("1.30.0".into()),
            api_versions: vec!["v1".into()],
        };
        let (index, blobs) = store::prepare_capture(
            "staging",
            &capabilities,
            &schemas::extract_crds(&[crd("integer")]).unwrap(),
        )
        .unwrap();
        for bytes in blobs.values() {
            store::write_blob(&root, bytes).unwrap();
        }
        store::atomic_write(
            &store::cluster_index_path(&root, "staging").unwrap(),
            &store::json_bytes(&index).unwrap(),
        )
        .unwrap();
        partition.schema_source = Some("staging".into());
        partition.schema_capabilities_fingerprint = Some(index.capabilities_fingerprint);
        partition.documents[0].manifest["spec"]["count"] = json!("new");
        partition.documents.push(document(crd("string")));
        assert!(validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition)
        )
        .await
        .is_err());
        let desired = ValidationArgs {
            use_desired_crds: true,
            ..ValidationArgs::default()
        };
        validate_partitions(&desired, &config, directory.path(), std::slice::from_ref(&partition))
            .await
            .unwrap();
        partition.documents[1].manifest["spec"]["versions"][0]["name"] = json!("v2");
        let error = validate_partitions(&desired, &config, directory.path(), &[partition])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("desired CRD does not serve this version"));
    }

    #[tokio::test]
    async fn test_local_schema_reference_graph_is_materialized_offline() {
        let (directory, config, partition) = fixture();
        let schema = directory.path().join("schemas/example.com/widget_v1.json");
        std::fs::rename(&schema, directory.path().join("schemas/example.com/definition.json")).unwrap();
        std::fs::write(schema, r#"{"$ref":"definition.json"}"#).unwrap();
        validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_vendored_builtin_reference_graph_is_verified_offline() {
        let (directory, config, mut partition) = fixture();
        partition.documents = vec![document(
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"test"},"data":{"key":"value"}}),
        )];
        let root = directory.path().join("vendor");
        let settings = config.config.validation.kubeconform.as_ref().unwrap();
        let resolver = SchemaResolver::new(directory.path(), root.clone(), settings, false, false).unwrap();
        let url = resolver.builtin_url("v1/ConfigMap", &partition.version).unwrap();
        let dependency_url = reqwest::Url::parse(&url)
            .unwrap()
            .join("dependency.json")
            .unwrap()
            .to_string();
        let hash = store::write_blob(&root, br#"{"$ref":"dependency.json"}"#).unwrap();
        let dependency_hash = store::write_blob(
            &root,
            br#"{"properties":{"data":{"additionalProperties":{"type":"string"}}}}"#,
        )
        .unwrap();
        let index = store::BuiltinIndex {
            version: 1,
            schemas: BTreeMap::from([(url, hash), (dependency_url, dependency_hash.clone())]),
        };
        store::atomic_write(&root.join("schemas/builtins.json"), &store::json_bytes(&index).unwrap()).unwrap();
        validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap();
        std::fs::write(root.join(format!("schemas/blobs/{dependency_hash}.json")), b"{}").unwrap();
        assert!(
            validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_crd_shared_definitions_vendor_and_validate_recursively_offline() {
        let (directory, mut config, mut partition) = fixture();
        let root = directory.path().join("vendor");
        let definition = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.CustomResourceDefinition";
        let schema_ref = "#/definitions/JSONSchemaProps";
        for strict in [true, false] {
            let settings = config.config.validation.kubeconform.as_mut().unwrap();
            settings.schema_locations.clear();
            settings.strict = strict;
            let mut resolver = SchemaResolver::new(directory.path(), root.clone(), settings, true, false).unwrap();
            let url = resolver
                .builtin_url("apiextensions.k8s.io/v1/CustomResourceDefinition", &partition.version)
                .unwrap();
            let url = reqwest::Url::parse(&url).unwrap().join("_definitions.json").unwrap();
            let definitions = json!({"definitions": {
                definition: {
                    "type":"object", "required":["spec"], "additionalProperties": !strict,
                    "properties":{
                        "apiVersion":{"type":"string"}, "kind":{"type":"string"},
                        "metadata":{"type":"object"},
                        "spec":{"type":"object", "required":["schema"], "properties":{
                            "schema":{"$ref":schema_ref}
                        }}
                    }
                },
                "JSONSchemaProps": {
                    "type":"object", "additionalProperties":!strict,
                    "properties":{
                        "type":{"type":"string"},
                        "properties":{"type":"object","additionalProperties":{"$ref":schema_ref}},
                        "default":{}
                    }
                }
            }});
            let hash = store::write_blob(&root, &store::json_bytes(&definitions).unwrap()).unwrap();
            let index = store::BuiltinIndex {
                version: 1,
                schemas: BTreeMap::from([(url.to_string(), hash.clone())]),
            };
            store::atomic_write(&root.join("schemas/builtins.json"), &store::json_bytes(&index).unwrap()).unwrap();
            let valid = json!({
                "apiVersion":"apiextensions.k8s.io/v1", "kind":"CustomResourceDefinition",
                "metadata":{"name":"widgets.example.com"},
                "spec":{"schema":{"type":"object","properties":{
                    "count":{"type":"integer", "default":{"arbitrary":"application data"}}
                }}}
            });
            partition.documents = vec![document(valid.clone())];
            let stage = TempDir::new().unwrap();
            prepare_partition(
                &ValidationArgs::default(),
                &partition,
                &mut resolver,
                stage.path(),
                true,
            )
            .await
            .unwrap();
            assert_eq!(resolver.observed_builtins.get(url.as_str()), Some(&hash));
            validate_partitions(
                &ValidationArgs::default(),
                &config,
                directory.path(),
                std::slice::from_ref(&partition),
            )
            .await
            .unwrap();
            partition.documents[0].manifest["spec"]["schema"]["properties"]["count"]["type"] = json!(42);
            assert!(validate_partitions(
                &ValidationArgs::default(),
                &config,
                directory.path(),
                std::slice::from_ref(&partition)
            )
            .await
            .is_err());
            partition.documents[0].manifest = valid;
            partition.documents[0].manifest["spec"]["schema"]["properties"]["count"]["unknown"] = json!(true);
            assert_eq!(
                validate_partitions(
                    &ValidationArgs::default(),
                    &config,
                    directory.path(),
                    std::slice::from_ref(&partition)
                )
                .await
                .is_err(),
                strict
            );
        }
    }

    #[tokio::test]
    async fn test_missing_crd_schema_guidance_uses_resolved_source() {
        let (directory, config, mut partition) = fixture();
        partition.documents = vec![document(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
            "metadata": {"name": "test"}
        }))];
        for source in [Some("production"), Some("staging"), None] {
            partition.schema_source = source.map(str::to_owned);
            let error = validate_partitions(
                &ValidationArgs::default(),
                &config,
                directory.path(),
                std::slice::from_ref(&partition),
            )
            .await
            .unwrap_err();
            let hint = match source {
                Some(source) => format!("run nyl capture cluster {source} --crds or configure schema_locations"),
                None => "configure schema_locations".into(),
            };
            assert_eq!(
                match error {
                    NylError::Validation(message) => message,
                    error => panic!("Expected validation error: {error}"),
                },
                format!("no schema for argoproj.io/v1alpha1/Application in destination production; {hint}")
            );
        }
    }

    #[tokio::test]
    async fn test_missing_builtin_and_mismatched_snapshot_fail_without_network() {
        let (directory, config, mut partition) = fixture();
        partition.documents = vec![document(
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"test"}}),
        )];
        let error = validate_partitions(
            &ValidationArgs::default(),
            &config,
            directory.path(),
            std::slice::from_ref(&partition),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Missing vendored built-in schema"));
        let root = directory.path().join("vendor");
        let index = store::ClusterSchemaIndex {
            version: 1,
            cluster: "staging".into(),
            capabilities_fingerprint: "different".into(),
            crds: BTreeMap::new(),
        };
        store::atomic_write(
            &store::cluster_index_path(&root, "staging").unwrap(),
            &store::json_bytes(&index).unwrap(),
        )
        .unwrap();
        partition.schema_source = Some("staging".into());
        partition.schema_capabilities_fingerprint = Some("expected".into());
        let error = validate_partitions(&ValidationArgs::default(), &config, directory.path(), &[partition])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not match its source capabilities"));
    }

    #[tokio::test]
    async fn test_missing_validator_fails_and_explicit_skip_is_reported() {
        let (directory, config, partition) = fixture();
        let settings = config.config.validation.kubeconform.as_ref().unwrap();
        let mut resolver = SchemaResolver::new(
            directory.path(),
            directory.path().join("vendor"),
            settings,
            false,
            false,
        )
        .unwrap();
        let stage = TempDir::new().unwrap();
        let (files, _) = prepare_partition(
            &ValidationArgs::default(),
            &partition,
            &mut resolver,
            stage.path(),
            false,
        )
        .await
        .unwrap();
        let missing = directory.path().join("missing-validator");
        let validator = KubeconformValidator {
            executable: &missing,
            strict: true,
        };
        assert!(validator
            .validate(stage.path(), &files, &partition)
            .await
            .unwrap_err()
            .to_string()
            .contains("Cannot run kubeconform"));
        let mut skip_settings = settings.clone();
        skip_settings.skip = vec!["example.com/v1/Widget".into()];
        let mut resolver = SchemaResolver::new(
            directory.path(),
            directory.path().join("vendor"),
            &skip_settings,
            false,
            false,
        )
        .unwrap();
        let (files, skipped) = prepare_partition(
            &ValidationArgs::default(),
            &partition,
            &mut resolver,
            stage.path(),
            false,
        )
        .await
        .unwrap();
        assert!(files.is_empty());
        assert_eq!(skipped, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_validator_rejects_incomplete_output_and_bounds_process_execution() {
        use std::os::unix::fs::PermissionsExt as _;
        let (directory, config, partition) = fixture();
        let settings = config.config.validation.kubeconform.as_ref().unwrap();
        let mut resolver = SchemaResolver::new(
            directory.path(),
            directory.path().join("vendor"),
            settings,
            false,
            false,
        )
        .unwrap();
        let stage = TempDir::new().unwrap();
        let (files, _) = prepare_partition(
            &ValidationArgs::default(),
            &partition,
            &mut resolver,
            stage.path(),
            false,
        )
        .await
        .unwrap();
        let executable = directory.path().join("validator");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s' '{\"resources\":[]}'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let validator = KubeconformValidator {
            executable: &executable,
            strict: true,
        };
        assert!(validator
            .validate(stage.path(), &files, &partition)
            .await
            .unwrap_err()
            .to_string()
            .contains("every submitted resource"));
        std::fs::write(&executable, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(100),
            validator.validate(stage.path(), &files, &partition)
        )
        .await
        .is_err());
    }
}
