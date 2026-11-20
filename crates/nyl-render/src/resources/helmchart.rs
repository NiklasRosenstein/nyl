/// HelmChart resource definition
///
/// Represents a declarative Helm chart deployment
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use nyl_core::constants::API_VERSION;

/// Reference to a Helm chart
///
/// Supports Git, OCI, traditional Helm repositories, and local filesystem paths
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ChartRef {
    /// Repository URL for Git, OCI, or traditional Helm repositories
    ///
    /// Protocol prefixes indicate the repository type:
    /// - `git+https://` or `git+git@` - Git repository
    /// - `oci://` - OCI registry
    /// - `https://` or no prefix - Traditional Helm repository
    ///
    /// Examples:
    /// - Git: "git+https://github.com/user/repo.git"
    /// - Git SSH: "git+git@github.com:user/repo.git"
    /// - OCI: "oci://ghcr.io/owner/chart"
    /// - Helm: "https://charts.example.com"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,

    /// Universal name field with context-dependent meaning
    ///
    /// When `repository` is set:
    /// - For Helm/OCI repositories: Specifies the chart name (required)
    /// - For Git repositories: Specifies subdirectory path within repo (optional)
    ///
    /// When `repository` is NOT set:
    /// - Treated as local filesystem path (absolute or relative)
    ///
    /// Examples:
    /// - Local filesystem: "./charts/mychart", "/opt/charts/app"
    /// - Git subpath: "charts/mychart", "deploy/helm/app"
    /// - Helm/OCI chart: "nginx", "prometheus"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Chart version or Git reference
    ///
    /// For Helm repositories: Specifies the chart version (required)
    /// For Git repositories: Specifies the Git reference - branch, tag, or commit
    ///                       (optional, defaults to HEAD)
    ///
    /// Examples:
    /// - Branch: "main", "develop"
    /// - Tag: "v1.0.0", "1.2.3"
    /// - Commit: "abc123..." (full or short hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Kubernetes object metadata
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectMetadata {
    /// Resource name
    pub name: String,

    /// Namespace (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// Labels
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,

    /// Annotations
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

impl ObjectMetadata {
    /// Create minimal metadata with just a name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        }
    }
}

/// The code of the error for `spec.lookup` written as an object, the form
/// `lookup: {}` that enabled lookup before the field became a boolean.
pub const LOOKUP_OBJECT_REMOVED_CODE: &str = "NYL-HELM-LOOKUP-OBJECT-REMOVED";

/// Whether a chart's Helm `lookup` reads the Release's objects: `true` enables
/// it, `false` declares that it is deliberately off.
///
/// Leaving the field out is the third state: undeclared, which warns when the
/// chart calls `lookup` anyway. The object form `{}` is no longer accepted and
/// fails with [`LOOKUP_OBJECT_REMOVED_CODE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookupSetting {
    enabled: bool,
}

impl LookupSetting {
    /// `lookup: true`
    pub const ENABLED: Self = Self { enabled: true };
    /// `lookup: false`
    pub const DISABLED: Self = Self { enabled: false };

    /// Whether lookup is on.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Serialize for LookupSetting {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bool(self.enabled)
    }
}

impl<'de> Deserialize<'de> for LookupSetting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct SettingVisitor;

        impl<'de> serde::de::Visitor<'de> for SettingVisitor {
            type Value = LookupSetting;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("`true` or `false`")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(if value {
                    LookupSetting::ENABLED
                } else {
                    LookupSetting::DISABLED
                })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, _map: A) -> std::result::Result<Self::Value, A::Error> {
                Err(serde::de::Error::custom(format!(
                    "{LOOKUP_OBJECT_REMOVED_CODE}: spec.lookup is a boolean; write `lookup: true` \
                     (or `lookup: false` to opt out)"
                )))
            }
        }

        deserializer.deserialize_any(SettingVisitor)
    }
}

impl JsonSchema for LookupSetting {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LookupSetting".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "boolean"})
    }
}

/// HelmChart specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelmChartSpec {
    /// Reference to the chart
    pub chart: ChartRef,

    /// Values files and object-typed Release inputs, merged in order before `values`.
    ///
    /// Each entry sets exactly one of `file` and `input`. Maps merge key by key and
    /// every other value replaces, which is Helm's own rule for `-f`; inline `values`
    /// merge last. The merged values are data: Nyl never resolves source clauses in them.
    #[serde(default, rename = "valuesFrom", skip_serializing_if = "Vec::is_empty")]
    pub values_from: Vec<HelmValuesSource>,

    /// Values to pass to the chart, merged over `valuesFrom`
    #[serde(default)]
    pub values: serde_json::Value,

    /// Whether to include CRDs from the chart's crds/ directory in the rendered output.
    /// Defaults to true when unset. In a Release bundle, `{$input: name}` reads it from a
    /// Release input.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "includeCrds")]
    pub include_crds: Option<bool>,

    /// Lets the chart's Helm `lookup` read the objects of the Release it belongs
    /// to, so a chart that reuses what it generated earlier, such as a Secret
    /// holding a CA, finds it when it renders again.
    ///
    /// Declare it as `lookup: true`, or `lookup: false` to state that the chart must not use it
    /// (which silences `NYL-HELM-LOOKUP-UNDECLARED`).
    /// `lookup` then reads the Release's objects from the
    /// target's last publication (or, with `--lookup=live`, from the cluster) and sees
    /// nothing else: any other object is absent. Without a target, or before the
    /// Release is published, nothing is found and the chart generates.
    ///
    /// Charts that declare it render through Helm's server-side dry run against a
    /// read-only API that Nyl serves on 127.0.0.1 for the duration of the render;
    /// Helm never gets a cluster or credentials. The Cluster's declared Kubernetes
    /// version and API versions describe that API, plus the API versions of
    /// CustomResourceDefinitions the chart ships in `crds/`.
    ///
    /// In a Release bundle, `{$input: name}` reads it from a Release input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup: Option<LookupSetting>,
}

/// One source of chart values in `spec.valuesFrom`: exactly one of `file` and
/// `input`, optionally narrowed by `pointer` and mounted under `key`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelmValuesSource {
    /// Static, normalized path to exactly one YAML mapping document in a Git-visible regular file, read without templating. Relative paths start at the directory of the file that declares the HelmChart; `/` starts at the Git worktree root (for a remote ApplicationGroup source, its checkout). Leading `..` segments may reach outside the Nyl project, but never outside its worktree or through symlinks. Ignored untracked files, submodules, and the project's `.nyl/cache` directory are excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Name of an `object`-typed input that the Release of this bundle declares in `spec.inputs`. Valid only in a Release bundle; its effective value is the target's binding or the declared default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// JSON Pointer that selects a value inside the file or input, such as `/web`; the whole value when omitted. Without `key`, the selected value must be a mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Mount the selected value under this literal values key. Omit to merge a mapping at the values root. Dots are literal characters, not path separators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Where one `spec.valuesFrom` entry reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelmValuesOrigin<'a> {
    /// `file`
    File(&'a str),
    /// `input`
    Input(&'a str),
}

impl HelmValuesSource {
    /// Check the static form of the entry at `field`, such as `spec.valuesFrom[0]`,
    /// and say where it reads.
    pub fn origin(&self, field: &str) -> nyl_core::Result<HelmValuesOrigin<'_>> {
        let origin = match (&self.file, &self.input) {
            (Some(file), None) => {
                super::ValuesFromFile {
                    file: file.clone(),
                    key: None,
                }
                .validate()?;
                HelmValuesOrigin::File(file)
            }
            (None, Some(input)) => {
                nyl_core::bindings::validate_input_name(&format!("{field}.input"), input)?;
                HelmValuesOrigin::Input(input)
            }
            _ => {
                return Err(nyl_core::CoreError::config(format!(
                    "{field} must set exactly one of file and input"
                )))
            }
        };
        if let Some(pointer) = &self.pointer {
            nyl_core::json_pointer::validate(&format!("{field}.pointer"), pointer)?;
        }
        if let Some(key) = &self.key {
            nyl_core::resources::validate_static_required(&format!("{field}.key"), key)?;
        }
        Ok(origin)
    }

    /// The values this entry contributes, from `document`, the whole file or
    /// input it reads: the value at `pointer`, mounted under `key`, or else a
    /// mapping merged at the root. `what`
    /// names the source in messages, such as `values file a.yaml`.
    pub fn select(&self, document: &serde_json::Value, what: &str) -> std::result::Result<serde_json::Value, String> {
        let pointer = self.pointer.as_deref().unwrap_or_default();
        let selected = document
            .pointer(pointer)
            .ok_or_else(|| format!("pointer {pointer:?} selects nothing in {what}"))?;
        if self.key.is_none() && !selected.is_object() {
            let at = if pointer.is_empty() {
                String::new()
            } else {
                format!(" at {pointer:?}")
            };
            return Err(format!(
                "{what}{at} has type {}, not a mapping; chart values are a mapping, so select one with pointer, or mount it with key",
                nyl_core::bindings::json_type_name(selected)
            ));
        }
        Ok(match &self.key {
            Some(key) => serde_json::Value::Object(serde_json::Map::from_iter([(key.clone(), selected.clone())])),
            None => selected.clone(),
        })
    }
}

/// The chart values of a HelmChart whose `valuesFrom` entries contributed
/// `sources`, in order: each merged over the previous with
/// [`crate::util::deep_merge_value`], then the inline `values`. Omitted inline
/// values add nothing.
pub fn merge_chart_values(
    sources: impl IntoIterator<Item = serde_json::Value>,
    inline: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let mut merged = serde_json::json!({});
    for source in sources {
        merged = crate::util::deep_merge_value(Some(merged), source);
    }
    match inline {
        serde_json::Value::Null => Ok(merged),
        inline @ serde_json::Value::Object(_) => Ok(crate::util::deep_merge_value(Some(merged), inline)),
        other => Err(format!(
            "spec.values is {}; with valuesFrom, values must be a mapping",
            nyl_core::bindings::json_type_name(&other)
        )),
    }
}

impl Default for HelmChartSpec {
    fn default() -> Self {
        Self {
            chart: ChartRef::default(),
            values_from: Vec::new(),
            values: serde_json::Value::Object(serde_json::Map::new()),
            include_crds: None,
            lookup: None,
        }
    }
}

/// Expands a local or remote Helm chart into Kubernetes manifests.
///
/// Nyl renders the chart and recursively processes its output. Cluster and target values are template inputs; pass values to the chart explicitly through `spec.values`.
///
/// ## When needed
///
/// Use this optional resource to render a Helm chart with explicit chart coordinates and values. Declare `spec.lookup` for a chart that reuses objects it generated earlier through Helm's `lookup`, such as a CA Secret.
///
/// ## If omitted
///
/// Nyl does not discover and render charts automatically. Invoke charts through a HelmChart or a component resource; projects containing only authored Kubernetes manifests need neither. Without `spec.lookup`, the chart renders offline and `lookup` finds nothing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::catalog::resource_example(super::catalog::ResourceKind::HelmChart))]
pub struct HelmChart {
    /// API version (e.g., "k8s.nyl/v1")
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Resource kind (always "HelmChart")
    pub kind: String,

    /// Kubernetes metadata
    pub metadata: ObjectMetadata,

    /// Chart specification
    pub spec: HelmChartSpec,
}

impl HelmChart {
    /// Create a new HelmChart resource
    pub fn new(name: impl Into<String>, chart_ref: ChartRef) -> Self {
        Self {
            api_version: API_VERSION.to_string(),
            kind: "HelmChart".to_string(),
            metadata: ObjectMetadata::new(name),
            spec: HelmChartSpec {
                chart: chart_ref,
                ..Default::default()
            },
        }
    }

    /// Set the values
    #[must_use]
    pub fn with_values(mut self, values: serde_json::Value) -> Self {
        self.spec.values = values;
        self
    }

    /// Set the namespace
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.metadata.namespace = Some(namespace.into());
        self
    }

    /// Get the release name (from metadata)
    pub fn release_name(&self) -> &str {
        &self.metadata.name
    }

    /// Get the release namespace (from metadata)
    pub fn release_namespace(&self) -> Option<&str> {
        self.metadata.namespace.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_ref_local_path() {
        let chart_ref = ChartRef {
            name: Some("./charts/mychart".to_string()),
            ..Default::default()
        };

        let yaml = serde_saphyr::to_string(&chart_ref).unwrap();
        assert!(yaml.contains("name: ./charts/mychart"));

        let deserialized: ChartRef = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(deserialized.name, Some("./charts/mychart".to_string()));
    }

    #[test]
    fn test_chart_ref_repository() {
        let chart_ref = ChartRef {
            repository: Some("https://charts.example.com".to_string()),
            name: Some("nginx".to_string()),
            version: Some("1.0.0".to_string()),
        };

        let yaml = serde_saphyr::to_string(&chart_ref).unwrap();
        assert!(yaml.contains("repository"));
        assert!(yaml.contains("nginx"));
    }

    #[test]
    fn test_object_metadata() {
        let mut metadata = ObjectMetadata::new("my-app");
        metadata.namespace = Some("default".to_string());
        metadata.labels.insert("app".to_string(), "web".to_string());

        assert_eq!(metadata.name, "my-app");
        assert_eq!(metadata.namespace, Some("default".to_string()));
        assert_eq!(metadata.labels.get("app").unwrap(), "web");
    }

    #[test]
    fn test_helm_chart_new() {
        let chart_ref = ChartRef {
            name: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let helm_chart = HelmChart::new("my-app", chart_ref);

        assert_eq!(helm_chart.api_version, "k8s.nyl/v1");
        assert_eq!(helm_chart.kind, "HelmChart");
        assert_eq!(helm_chart.metadata.name, "my-app");
        assert_eq!(helm_chart.spec.chart.name, Some("./charts/app".to_string()));
    }

    #[test]
    fn test_helm_chart_builder() {
        let chart_ref = ChartRef {
            name: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let values = serde_json::json!({"replicas": 3});

        let helm_chart = HelmChart::new("my-app", chart_ref)
            .with_namespace("production")
            .with_values(values.clone());

        assert_eq!(helm_chart.release_name(), "my-app");
        assert_eq!(helm_chart.release_namespace(), Some("production"));
        assert_eq!(helm_chart.spec.values, values);
    }

    #[test]
    fn test_helm_chart_serialization() {
        let chart_ref = ChartRef {
            name: Some("./charts/nginx".to_string()),
            ..Default::default()
        };

        let values = serde_json::json!({
            "replicaCount": 2,
            "image": {
                "repository": "nginx",
                "tag": "1.21"
            }
        });

        let helm_chart = HelmChart::new("nginx-app", chart_ref)
            .with_namespace("default")
            .with_values(values);

        let yaml = serde_saphyr::to_string(&helm_chart).unwrap();
        assert!(yaml.contains("apiVersion: k8s.nyl/v1"));
        assert!(yaml.contains("kind: HelmChart"));
        assert!(yaml.contains("name: nginx-app"));
        assert!(yaml.contains("replicaCount: 2"));

        // Round-trip
        let deserialized: HelmChart = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(deserialized.metadata.name, "nginx-app");
        assert_eq!(deserialized.spec.chart.name, Some("./charts/nginx".to_string()));
    }

    #[test]
    fn test_helm_chart_spec_defaults() {
        let spec = HelmChartSpec::default();
        assert!(spec.values.is_object());
        assert_eq!(spec.include_crds, None);
    }

    #[test]
    fn test_helm_chart_spec_include_crds_explicit_false() {
        let yaml = r"
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  includeCrds: false
";
        let result: HelmChart = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, Some(false));
    }

    #[test]
    fn test_helm_chart_spec_include_crds_explicit_true() {
        let yaml = r"
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  includeCrds: true
";
        let result: HelmChart = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, Some(true));
    }

    #[test]
    fn test_helm_chart_spec_include_crds_omitted_is_none() {
        let yaml = r"
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
";
        let result: HelmChart = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, None);
    }

    #[test]
    fn test_helm_chart_spec_include_crds_not_serialized_when_none() {
        let helm_chart = HelmChart::new(
            "test",
            ChartRef {
                name: Some("mychart".to_string()),
                ..Default::default()
            },
        );
        let yaml = serde_saphyr::to_string(&helm_chart).unwrap();
        assert!(!yaml.contains("includeCrds"));
    }

    fn chart_with_lookup(lookup: &str) -> std::result::Result<HelmChart, String> {
        let line = if lookup.is_empty() {
            String::new()
        } else {
            format!("  lookup: {lookup}\n")
        };
        serde_saphyr::from_str(&format!(
            "apiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata:\n  name: test\nspec:\n  chart:\n    name: c\n{line}"
        ))
        .map_err(|error| error.to_string())
    }

    #[test]
    fn test_lookup_is_a_boolean_and_off_when_omitted() {
        assert_eq!(chart_with_lookup("").unwrap().spec.lookup, None);
        let on = chart_with_lookup("true").unwrap();
        assert_eq!(on.spec.lookup, Some(LookupSetting::ENABLED));
        assert!(serde_saphyr::to_string(&on).unwrap().contains("lookup: true"));
        let off = chart_with_lookup("false").unwrap();
        assert_eq!(off.spec.lookup, Some(LookupSetting::DISABLED));
        assert!(serde_saphyr::to_string(&off).unwrap().contains("lookup: false"));
        assert!(!serde_saphyr::to_string(&chart_with_lookup("").unwrap())
            .unwrap()
            .contains("lookup"));
    }

    #[test]
    fn test_lookup_object_is_removed_with_its_code_and_fix() {
        for object in ["{}", "{source: live}"] {
            let error = chart_with_lookup(object).unwrap_err();
            assert!(error.contains(LOOKUP_OBJECT_REMOVED_CODE), "{object}: {error}");
            assert!(error.contains("lookup: true"), "{object}: {error}");
        }
    }

    #[test]
    fn test_lookup_rejects_other_values() {
        for invalid in ["\"yes\"", "1", "[]"] {
            assert!(chart_with_lookup(invalid).is_err(), "{invalid}");
        }
    }

    fn values_source(value: serde_json::Value) -> HelmValuesSource {
        serde_json::from_value(value).unwrap()
    }

    /// Catches `valuesFrom` entries contributing anything but the selected
    /// value at the root, or under `key`.
    #[test]
    fn test_values_source_selects_and_mounts() {
        let document = serde_json::json!({"web": {"replicas": 2}, "tags": ["a"]});
        let whole = values_source(serde_json::json!({"file": "a.yaml"}));
        assert_eq!(whole.select(&document, "f").unwrap(), document);
        let pointed = values_source(serde_json::json!({"file": "a.yaml", "pointer": "/web"}));
        assert_eq!(
            pointed.select(&document, "f").unwrap(),
            serde_json::json!({"replicas": 2})
        );
        let mounted = values_source(serde_json::json!({"input": "v", "pointer": "/tags", "key": "my.tags"}));
        assert_eq!(
            mounted.select(&document, "f").unwrap(),
            serde_json::json!({"my.tags": ["a"]})
        );
        let list = values_source(serde_json::json!({"file": "a.yaml", "pointer": "/tags"}));
        assert_eq!(
            list.select(&document, "values file a.yaml").unwrap_err(),
            "values file a.yaml at \"/tags\" has type array, not a mapping; chart values are a mapping, so select one with pointer, or mount it with key"
        );
        let missing = values_source(serde_json::json!({"file": "a.yaml", "pointer": "/db"}));
        assert!(missing.select(&document, "f").unwrap_err().contains("selects nothing"));
    }

    /// Catches an entry that reads from both sources, or none, or carries an
    /// invalid pointer, file, or input name.
    #[test]
    fn test_values_source_reads_exactly_one_source() {
        let origin = |value| {
            values_source(value)
                .origin("spec.valuesFrom[0]")
                .map(|origin| format!("{origin:?}"))
                .map_err(|error| error.to_string())
        };
        assert_eq!(
            origin(serde_json::json!({"file": "a.yaml"})).unwrap(),
            "File(\"a.yaml\")"
        );
        assert_eq!(
            origin(serde_json::json!({"input": "chartValues"})).unwrap(),
            "Input(\"chartValues\")"
        );
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"file": "a.yaml", "input": "v"}),
            serde_json::json!({"file": "../.git/config"}),
            serde_json::json!({"input": "Bad-Name"}),
            serde_json::json!({"input": "v", "pointer": "web"}),
            serde_json::json!({"input": "v", "key": ""}),
        ] {
            assert!(origin(invalid.clone()).is_err(), "{invalid}");
        }
        assert!(
            serde_json::from_value::<HelmValuesSource>(serde_json::json!({"file": "a", "optional": true})).is_err()
        );
    }

    /// Catches a merge order other than the entries in order, then the
    /// inline values: maps merge, everything else replaces.
    #[test]
    fn test_merge_chart_values_order() {
        let merged = merge_chart_values(
            [
                serde_json::json!({"a": {"x": 1, "list": [1]}, "b": 1}),
                serde_json::json!({"a": {"y": 2, "list": []}, "b": 2}),
            ],
            serde_json::json!({"b": 3}),
        )
        .unwrap();
        assert_eq!(merged, serde_json::json!({"a": {"x": 1, "y": 2, "list": []}, "b": 3}));
        assert_eq!(
            merge_chart_values([serde_json::json!({"a": 1})], serde_json::Value::Null).unwrap(),
            serde_json::json!({"a": 1})
        );
        assert!(merge_chart_values([], serde_json::json!([1])).is_err());
    }

    #[test]
    fn test_chart_ref_rejects_unknown_fields() {
        let yaml = r#"
name: mychart
version: "1.0.0"
unknownField: value
"#;
        let result: std::result::Result<ChartRef, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }

    #[test]
    fn test_helm_chart_rejects_unknown_fields() {
        let yaml = r"
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  unknownField: value
";
        let result: std::result::Result<HelmChart, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }
}
