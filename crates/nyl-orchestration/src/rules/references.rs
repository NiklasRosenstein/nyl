//! The one resolver of `fromUnit` references.
//!
//! Contract: [References](../../../../design/orchestration-core.md#references),
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit)
//! (provenance, readiness), and consumer freshness in
//! [Observed unit](../../../../design/orchestration-core.md#observed-unit).
//!
//! Unit specs, KubernetesPublication `releases`, `nyl get output`/`get
//! artifact`, and later PromotionPath selectors resolve through
//! [`resolve_reference`], so they cannot disagree.
//!
//! - A reference resolves only against the producer's current receipt: its
//!   uid and execution key equal the producer's desired document as this
//!   resolution sees it ([`super::freshness`]). A producer whose desired
//!   document changed blocks its consumers, even ones with receipts.
//! - An output reference reads a declared, non-sensitive output; an artifact
//!   reference reads an artifact the receipt lists, after checking the
//!   artifact document's digest against the receipt. `pointer` (RFC 6901)
//!   selects inside the output or the artifact's `spec`; `kind` must match.
//! - A unit outside the ownership set, an undeclared or sensitive output, a
//!   kind mismatch, or a value of the wrong type for the declared output is a
//!   resolution error ([`Code::ReferenceInvalid`](nyl_core::orchestration::codes::Code::ReferenceInvalid)).
//!   A producer without a current receipt blocks the reference instead.
//! - Provenance cites the receipt: `{unit, uid, executionKey, observedCommit}`
//!   for a receipt read from the snapshot, or `{…, run}` for one produced
//!   earlier in the same run; never a checkpoint or the commit being written.
//! - A citation is stable: when a reference resolves to the receipt (unit,
//!   uid, executionKey) that the stored desired document already cites at
//!   that pointer, the stored citation (its `run` or `observedCommit`) is
//!   kept unchanged. Re-resolving against unchanged receipts therefore
//!   yields byte-identical provenance, and a run that changes nothing writes
//!   nothing, although the observed tip and the run ID differ.
//! - The artifact digest is [`ArtifactDocument::digest`], the one function
//!   receipts are written and checked with.
//! - Cross-environment references use one immutable snapshot per declared producer.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::{resolve_document, Origin, Provided, Providers, Provision, Resolved, UnitInputSource};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, JsonPointer, RunId, UnitName};
use nyl_core::orchestration::state::{
    ArtifactDocument, BlockedReason, DesiredUnit, ObservedUnit, Provenance, Readiness, Receipt, ReceiptProvenance,
};
use nyl_core::resources::units::{DependsOn, Evidence, OutputDeclaration};
use serde_json::Value;

use super::freshness::receipt_is_current;

/// What references resolve against.
pub struct ReferenceView<'a> {
    /// Snapshots of declared producer environments, read once for this run.
    pub environments: &'a BTreeMap<EnvironmentName, nyl_core::orchestration::ports::EnvironmentSnapshot>,
    pub environment: &'a EnvironmentName,
    /// Desired documents of the ownership set as this resolution sees them:
    /// producers already resolved in this pass carry their new execution keys.
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// Observed documents: the snapshot's, overlaid with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    /// Artifacts of the latest receipts, overlaid likewise.
    pub artifacts: &'a BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    /// The observed commit the snapshot was read at, cited by provenance.
    pub observed_commit: Option<&'a CommitId>,
    /// This run, when receipts of `produced_in_run` came from it.
    pub run: Option<&'a RunId>,
    pub produced_in_run: &'a BTreeSet<UnitName>,
    /// The snapshot's stored desired documents, whose provenance
    /// [`resolve_unit`] keeps where it still cites the same receipt.
    pub previous: &'a BTreeMap<UnitName, DesiredUnit>,
}

/// What one reference found.
#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceValue {
    Value {
        value: Value,
        provenance: ReceiptProvenance,
    },
    /// The producer has no current receipt yet.
    Blocked { unit: UnitName, reason: String },
}

/// The reason a reference or `dependsOn` entry waits for `producer`.
fn no_current_receipt(producer: &UnitName) -> String {
    format!("{producer} has no current receipt")
}

/// The producer's desired document, which must be in the ownership set.
fn producer<'v>(view: &'v ReferenceView<'_>, unit: &str) -> Result<(&'v UnitName, &'v DesiredUnit), String> {
    let name = UnitName::new(unit).map_err(|error| format!("names an invalid unit: {error}"))?;
    view.desired.get_key_value(&name).ok_or_else(|| {
        format!(
            "references unit {unit:?}, which is outside the ownership set of environment {}",
            view.environment
        )
    })
}

/// The producer's declared outputs, from its rendered spec.
fn declared_outputs(name: &UnitName, spec: &Value) -> Result<BTreeMap<String, OutputDeclaration>, String> {
    match spec.get("outputs") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(outputs) => serde_json::from_value(outputs.clone())
            .map_err(|error| format!("unit {name} declares invalid outputs: {error}")),
    }
}

/// Resolve one reference. `Err` is a resolution error, reported with
/// [`Code::ReferenceInvalid`](nyl_core::orchestration::codes::Code::ReferenceInvalid).
pub fn resolve_reference(view: &ReferenceView<'_>, source: &UnitInputSource) -> Result<ReferenceValue, String> {
    if let Some(error) = unsupported_evidence(source) {
        return Err(error);
    }
    if let Some(environment) = source
        .environment
        .as_deref()
        .filter(|environment| *environment != view.environment.as_str())
    {
        return resolve_external(view, source, environment);
    }
    let (name, desired) = producer(view, &source.unit)?;
    // Static checks against the producer's declaration come first, so an
    // invalid reference fails even while its producer has no receipt.
    let declaration = check_declaration(name, desired, source)?;
    let observed = view.observed.get(name);
    if !receipt_is_current(desired, observed) {
        return Ok(ReferenceValue::Blocked {
            unit: name.clone(),
            reason: no_current_receipt(name),
        });
    }
    let receipt = observed
        .and_then(|observed| observed.receipt.as_ref())
        .expect("a current receipt exists");
    let whole = if let (Some(output), Some(declaration)) = (&source.output, declaration) {
        let Some(value) = receipt.outputs.get(output) else {
            return Err(format!(
                "reads output {output:?}, which the receipt of unit {name} does not record"
            ));
        };
        if !declaration.output_type.accepts(value) {
            return Err(format!(
                "reads output {output:?} of unit {name}, whose recorded value is not of its declared type {:?}",
                declaration.output_type
            ));
        }
        value.clone()
    } else {
        read_artifact(view, name, source, receipt)?
    };
    let pointer = source.pointer.as_deref().unwrap_or("");
    let Some(value) = nyl_core::json_pointer::resolve(&whole, pointer) else {
        return Err(format!(
            "selects JSON Pointer {pointer:?}, which has no value in what unit {name} recorded"
        ));
    };
    let in_run = view.produced_in_run.contains(name).then_some(view.run).flatten();
    Ok(ReferenceValue::Value {
        value: value.clone(),
        provenance: ReceiptProvenance {
            environment: None,
            unit: name.clone(),
            uid: desired.unit.uid.clone().expect("a current receipt has a uid"),
            execution_key: receipt.execution_key.clone(),
            observed_commit: if in_run.is_some() {
                None
            } else {
                view.observed_commit.cloned()
            },
            run: in_run.cloned(),
        },
    })
}

/// Resolve against the producer's immutable state snapshot; never its lease or checkpoints.
fn resolve_external(
    view: &ReferenceView<'_>,
    source: &UnitInputSource,
    environment: &str,
) -> Result<ReferenceValue, String> {
    let name = EnvironmentName::new(environment).map_err(|error| error.to_string())?;
    let snapshot = view
        .environments
        .get(&name)
        .ok_or_else(|| format!("references undeclared environment {environment:?}"))?;
    // Source validation checks membership; a newly declared producer may have no desired state yet.
    if !snapshot
        .desired
        .contains_key(&UnitName::new(&source.unit).map_err(|error| error.to_string())?)
    {
        return Ok(ReferenceValue::Blocked {
            unit: UnitName::new(&source.unit).map_err(|error| error.to_string())?,
            reason: format!("{environment}/{} has no current receipt", source.unit),
        });
    }
    if let Ok(unit) = UnitName::new(&source.unit) {
        if snapshot.desired.get(&unit).is_some_and(|unit| {
            matches!(
                unit.lifecycle,
                nyl_core::orchestration::state::Lifecycle::Deleting { .. }
                    | nyl_core::orchestration::state::Lifecycle::Retained { .. }
            )
        }) {
            return Err(format!(
                "references unit {environment}/{unit}, which is outside the ownership set"
            ));
        }
    }
    let external = ReferenceView {
        environments: view.environments,
        environment: &name,
        desired: &snapshot.desired,
        observed: &snapshot.observed,
        artifacts: &snapshot.artifacts,
        observed_commit: snapshot.tips.observed.as_ref(),
        run: None,
        produced_in_run: &BTreeSet::new(),
        previous: &snapshot.desired,
    };
    let mut source = source.clone();
    source.environment = None;
    let mut result = resolve_reference(&external, &source)?;
    match &mut result {
        ReferenceValue::Value { provenance, .. } => provenance.environment = Some(name),
        ReferenceValue::Blocked { reason, .. } => *reason = format!("{environment}/{reason}"),
    }
    Ok(result)
}

/// The state-free checks of a reference against its producer's rendered
/// spec: exactly one of `output` and `artifact`, `kind` only on artifact
/// references, and an output that the producer declares as non-sensitive.
/// Returns the output's declaration for an output reference. Resolution and
/// graph extraction ([`super::graph::extract`], which `nyl validate` runs
/// too) both check through it.
pub(crate) fn check_declaration(
    name: &UnitName,
    producer: &DesiredUnit,
    source: &UnitInputSource,
) -> Result<Option<OutputDeclaration>, String> {
    check_spec_declaration(name, &producer.spec, source)
}

/// Validate a reference against a rendered producer spec, before state exists.
pub fn check_spec_declaration(
    name: &UnitName,
    spec: &Value,
    source: &UnitInputSource,
) -> Result<Option<OutputDeclaration>, String> {
    match (&source.output, &source.artifact) {
        (Some(output), None) => {
            if let Some(kind) = &source.kind {
                return Err(format!(
                    "asserts kind {kind:?} for output {output:?} of unit {name}; kind applies to artifact references only"
                ));
            }
            let outputs = declared_outputs(name, spec)?;
            let Some(declaration) = outputs.get(output) else {
                return Err(format!(
                    "reads output {output:?}, which unit {name} does not declare; declared outputs: {}",
                    outputs.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            };
            if declaration.sensitive {
                return Err(format!(
                    "reads output {output:?} of unit {name}, which is sensitive and never referenceable"
                ));
            }
            Ok(Some(declaration.clone()))
        }
        // Artifact names are checked once kinds declare them (M5).
        (None, Some(_)) => Ok(None),
        _ => Err("must set exactly one of output and artifact".to_owned()),
    }
}

/// Why a reference's evidence requirement cannot be met yet: no kind
/// records attestation results in M3, so `evidence: attested` and named
/// `attestations` are rejected rather than silently met by a bare receipt.
pub(crate) fn unsupported_evidence(source: &UnitInputSource) -> Option<String> {
    let evidence = source.evidence.as_deref().filter(|evidence| *evidence != "published");
    if evidence.is_none() && source.attestations.is_empty() {
        return None;
    }
    let mut required = Vec::new();
    if let Some(evidence) = evidence {
        required.push(format!("evidence {evidence:?}"));
    }
    if !source.attestations.is_empty() {
        required.push(format!("attestations {}", source.attestations.join(", ")));
    }
    Some(format!(
        "requires {} of unit {:?}; attestations are not recorded yet, so only evidence \"published\" (a current receipt) is supported",
        required.join(" and "),
        source.unit
    ))
}

/// A rendered spec's `dependsOn` entries; none when the field is absent.
pub(crate) fn depends_on(spec: &Value) -> Result<Vec<DependsOn>, serde_json::Error> {
    match spec.get("dependsOn") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(entries) => serde_json::from_value(entries.clone()),
    }
}

/// Why a `dependsOn` entry's evidence requirement cannot be met yet, as for
/// [`unsupported_evidence`].
pub(crate) fn unsupported_depends_on(entry: &DependsOn) -> Option<String> {
    let DependsOn::Entry(entry) = entry else {
        return None;
    };
    unsupported_evidence(&UnitInputSource {
        environment: None,
        unit: entry.unit.clone(),
        output: None,
        artifact: None,
        kind: None,
        pointer: None,
        evidence: entry
            .evidence
            .filter(|evidence| *evidence != Evidence::Published)
            .map(|_| "attested".to_owned()),
        attestations: entry.attestations.clone(),
    })
}

/// The `spec` of the artifact an artifact reference reads, after checking
/// that the current receipt lists it with the asserted kind and the
/// document's digest.
fn read_artifact(
    view: &ReferenceView<'_>,
    name: &UnitName,
    source: &UnitInputSource,
    receipt: &Receipt,
) -> Result<Value, String> {
    let artifact = source.artifact.as_ref().expect("an artifact reference");
    let Some(listed) = receipt.artifacts.iter().find(|listed| &listed.name == artifact) else {
        return Err(format!(
            "reads artifact {artifact:?}, which the current receipt of unit {name} does not list"
        ));
    };
    if let Some(kind) = &source.kind {
        if kind != &listed.kind {
            return Err(format!(
                "expects artifact {artifact:?} of unit {name} to be of kind {kind}, but it is a {}",
                listed.kind
            ));
        }
    }
    let Some(document) = view.artifacts.get(name).and_then(|artifacts| artifacts.get(artifact)) else {
        return Err(format!(
            "reads artifact {artifact:?} of unit {name}, whose document is missing from state"
        ));
    };
    let digest = document.digest();
    if digest != listed.digest {
        return Err(format!(
            "reads artifact {artifact:?} of unit {name}, whose document has digest {digest} but its receipt lists {}",
            listed.digest
        ));
    }
    Ok(document.spec.clone())
}

/// A unit's spec with its references resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The spec with every reference replaced, or `None` while blocked.
    pub resolved_spec: Option<Value>,
    pub provenance: BTreeMap<JsonPointer, Provenance>,
    /// `Blocked` lists each blocked pointer with its reason.
    pub readiness: Readiness,
    /// Resolution errors, each naming the pointer.
    pub errors: Vec<String>,
}

/// Where a resolved value came from, for [`resolve_document`].
#[derive(Debug, Clone, PartialEq)]
enum Cited {
    Receipt(ReceiptProvenance),
    /// Never produced: a document resolves only reference objects.
    Literal,
}

impl Origin for Cited {
    fn from_default() -> Self {
        Self::Literal
    }
    fn from_override() -> Self {
        Self::Literal
    }
    fn from_value() -> Self {
        Self::Literal
    }
    fn describe(&self, field: &str) -> String {
        match self {
            Self::Receipt(provenance) => format!("{field} (from unit {})", provenance.unit),
            Self::Literal => field.to_owned(),
        }
    }
}

struct ViewProviders<'a, 'v> {
    view: &'a ReferenceView<'v>,
}

impl Providers for ViewProviders<'_, '_> {
    type Origin = Cited;

    fn unit(&self, field: &str, source: &UnitInputSource) -> Provided<Cited> {
        match resolve_reference(self.view, source) {
            Ok(ReferenceValue::Value { value, provenance }) => Ok(Provision::Value(Resolved {
                value,
                origin: Cited::Receipt(provenance),
            })),
            Ok(ReferenceValue::Blocked { reason, .. }) => Ok(Provision::Blocked(reason)),
            Err(error) => Err(format!("{field} {error}")),
        }
    }
}

/// Whether two citations name the same receipt (unit, uid, executionKey).
fn same_receipt(stored: &ReceiptProvenance, new: &ReceiptProvenance) -> bool {
    stored.environment == new.environment
        && stored.unit == new.unit
        && stored.uid == new.uid
        && stored.execution_key == new.execution_key
}

/// Resolve every reference of `unit.spec` through
/// [`nyl_core::bindings::resolve_document`] with providers backed by
/// [`resolve_reference`]. Where `view.previous[unit]` cites the same
/// receipt (unit, uid, executionKey) at a pointer, that stored citation is
/// returned instead of a new one. An explicit `dependsOn` entry whose
/// producer has no current receipt blocks at `/dependsOn/<index>`.
pub fn resolve_unit(view: &ReferenceView<'_>, unit: &DesiredUnit) -> Resolution {
    let mut errors = Vec::new();
    let resolved = resolve_document("spec", "", &unit.spec, &ViewProviders { view }, &mut errors);
    let stored = view.previous.get(&unit.unit.name).map(|previous| &previous.provenance);

    let mut provenance = BTreeMap::new();
    for (pointer, cited) in resolved.provenance {
        let Cited::Receipt(new) = cited else { continue };
        let pointer = match JsonPointer::new(pointer.as_str()) {
            Ok(pointer) => pointer,
            Err(error) => {
                errors.push(format!("spec reference at {pointer:?}: {error}"));
                continue;
            }
        };
        let citation = match stored.and_then(|stored| stored.get(&pointer)) {
            Some(Provenance::Receipt(stored)) if same_receipt(stored, &new) => stored.clone(),
            _ => new,
        };
        provenance.insert(pointer, Provenance::Receipt(citation));
    }

    let mut reasons = resolved
        .blocked
        .into_iter()
        .map(|blocked| BlockedReason {
            pointer: blocked.pointer,
            reason: blocked.reason,
        })
        .collect::<Vec<_>>();
    reasons.extend(depends_on_reasons(view, unit, &mut errors));

    let readiness = if reasons.is_empty() {
        Readiness::Ready
    } else {
        Readiness::Blocked { reasons }
    };
    let resolved_spec = (matches!(readiness, Readiness::Ready) && errors.is_empty()).then_some(resolved.value);
    Resolution {
        resolved_spec,
        provenance,
        readiness,
        errors,
    }
}

/// The `dependsOn` entries whose producer has no current receipt, each
/// blocking at `/dependsOn/<index>`. Entries naming another environment or a
/// unit outside the ownership set are errors.
fn depends_on_reasons(view: &ReferenceView<'_>, unit: &DesiredUnit, errors: &mut Vec<String>) -> Vec<BlockedReason> {
    let entries = match depends_on(&unit.spec) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("spec.dependsOn is invalid: {error}"));
            return Vec::new();
        }
    };
    let mut reasons = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let field = format!("spec.dependsOn[{index}]");
        if let Some(error) = unsupported_depends_on(entry) {
            errors.push(format!("{field} {error}"));
            continue;
        }
        if let Some(environment) = entry.environment() {
            if environment != view.environment.as_str() {
                let snapshot = EnvironmentName::new(environment)
                    .ok()
                    .and_then(|name| view.environments.get(&name));
                match snapshot {
                    Some(snapshot) if snapshot.desired.is_empty() => reasons.push(BlockedReason {
                        pointer: format!("/dependsOn/{index}"),
                        reason: format!("{environment}/{} has no current receipt", entry.unit()),
                    }),
                    Some(snapshot) => match UnitName::new(entry.unit())
                        .ok()
                        .and_then(|name| snapshot.desired.get_key_value(&name))
                    {
                        Some((name, desired))
                            if matches!(
                                desired.lifecycle,
                                nyl_core::orchestration::state::Lifecycle::Deleting { .. }
                                    | nyl_core::orchestration::state::Lifecycle::Retained { .. }
                            ) =>
                        {
                            errors.push(format!(
                                "{field} references unit {environment}/{name} outside the ownership set"
                            ));
                        }
                        Some((name, desired)) if receipt_is_current(desired, snapshot.observed.get(name)) => {}
                        Some((name, _)) => reasons.push(BlockedReason {
                            pointer: format!("/dependsOn/{index}"),
                            reason: format!("{environment}/{}", no_current_receipt(name)),
                        }),
                        None => reasons.push(BlockedReason {
                            pointer: format!("/dependsOn/{index}"),
                            reason: format!("{environment}/{} has no current receipt", entry.unit()),
                        }),
                    },
                    None => errors.push(format!("{field} references undeclared environment {environment:?}")),
                }
                continue;
            }
        }
        match producer(view, entry.unit()) {
            Ok((name, desired)) => {
                if !receipt_is_current(desired, view.observed.get(name)) {
                    reasons.push(BlockedReason {
                        pointer: format!("/dependsOn/{index}"),
                        reason: no_current_receipt(name),
                    });
                }
            }
            Err(error) => errors.push(format!("{field} {error}")),
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::fixtures::*;
    use super::*;

    struct World {
        environments: BTreeMap<EnvironmentName, nyl_core::orchestration::ports::EnvironmentSnapshot>,
        desired: BTreeMap<UnitName, DesiredUnit>,
        observed: BTreeMap<UnitName, ObservedUnit>,
        artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        previous: BTreeMap<UnitName, DesiredUnit>,
        produced_in_run: BTreeSet<UnitName>,
        observed_commit: CommitId,
        run: RunId,
        environment: EnvironmentName,
    }

    impl World {
        /// `network` with a current receipt, `web-image` with a current
        /// receipt listing its `image` artifact, and `database` consuming them.
        fn new() -> Self {
            let image = artifact(
                "web-image",
                "image",
                "ContainerImage",
                "web-image",
                json!({"repository": "registry.example.com/web", "tags": ["a", "b"]}),
            );
            let mut world = Self {
                environments: BTreeMap::new(),
                desired: BTreeMap::new(),
                observed: BTreeMap::new(),
                artifacts: BTreeMap::new(),
                previous: BTreeMap::new(),
                produced_in_run: BTreeSet::new(),
                observed_commit: commit(COMMIT),
                run: run(RUN_1),
                environment: env(),
            };
            world.add("network", network_spec());
            world.add("web-image", json!({}));
            world.receipt(
                "network",
                receipt(
                    "network",
                    json!({"vpcId": "vpc-0abc123", "subnets": {"a": "subnet-1"}, "token": "x"}),
                    Vec::new(),
                ),
            );
            world.receipt("web-image", receipt("web-image", json!({}), vec![listed(&image)]));
            world
                .artifacts
                .insert(name("web-image"), BTreeMap::from([("image".to_owned(), image)]));
            world
        }

        fn add(&mut self, unit: &str, spec: Value) {
            self.desired.insert(name(unit), desired(unit, "Command", spec, unit));
        }

        fn receipt(&mut self, unit: &str, receipt: nyl_core::orchestration::state::Receipt) {
            self.observed
                .insert(name(unit), observed(unit, "Command", Some(receipt)));
        }

        fn view(&self) -> ReferenceView<'_> {
            ReferenceView {
                environments: &self.environments,
                environment: &self.environment,
                desired: &self.desired,
                observed: &self.observed,
                artifacts: &self.artifacts,
                observed_commit: Some(&self.observed_commit),
                run: Some(&self.run),
                produced_in_run: &self.produced_in_run,
                previous: &self.previous,
            }
        }

        fn resolve(&self, reference: Value) -> Result<ReferenceValue, String> {
            resolve_reference(&self.view(), &serde_json::from_value(reference).unwrap())
        }
    }

    fn consumer(variables: &Value) -> DesiredUnit {
        desired("database", "Command", json!({"variables": variables}), "database")
    }

    #[test]
    fn test_cross_environment_reference_cites_producer_snapshot_and_blocks_stale_evidence() {
        let producer = World::new();
        let mut consumer = World::new();
        let environment = EnvironmentName::new("infra").unwrap();
        let mut snapshot = nyl_core::orchestration::ports::EnvironmentSnapshot {
            desired: producer.desired.clone(),
            observed: producer.observed.clone(),
            artifacts: producer.artifacts.clone(),
            ..nyl_core::orchestration::ports::EnvironmentSnapshot::default()
        };
        snapshot.tips.observed = Some(producer.observed_commit.clone());
        consumer.environments.insert(environment.clone(), snapshot);
        let reference = json!({"environment": "infra", "unit": "network", "output": "vpcId"});
        let ReferenceValue::Value { value, provenance } = consumer.resolve(reference.clone()).unwrap() else {
            panic!("the producer has a current receipt")
        };
        assert_eq!(value, json!("vpc-0abc123"));
        assert_eq!(provenance.environment, Some(environment.clone()));
        assert_eq!(provenance.observed_commit, Some(producer.observed_commit));
        assert!(provenance.run.is_none());
        consumer.environments.get_mut(&environment).unwrap().observed.clear();
        assert!(matches!(
            consumer.resolve(reference.clone()).unwrap(),
            ReferenceValue::Blocked { .. }
        ));
        consumer.environments.get_mut(&environment).unwrap().desired.clear();
        assert!(matches!(
            consumer.resolve(reference).unwrap(),
            ReferenceValue::Blocked { .. }
        ));
    }

    #[test]
    fn test_resolve_reference_blocks_without_a_producer_receipt() {
        let mut world = World::new();
        world.observed.remove(&name("network"));
        assert_eq!(
            world.resolve(json!({"unit": "network", "output": "vpcId"})).unwrap(),
            ReferenceValue::Blocked {
                unit: name("network"),
                reason: "network has no current receipt".to_owned(),
            }
        );
    }

    #[test]
    fn test_resolve_reference_blocks_when_the_producer_key_changed() {
        let mut world = World::new();
        // The producer re-rendered: its receipt is for an older execution key.
        world.desired.get_mut(&name("network")).unwrap().execution_key = Some(key("network-v2"));
        assert!(matches!(
            world.resolve(json!({"unit": "network", "output": "vpcId"})).unwrap(),
            ReferenceValue::Blocked { .. }
        ));
        let resolution = resolve_unit(
            &world.view(),
            &consumer(&json!({"vpc": {"fromUnit": {"unit": "network", "output": "vpcId"}}})),
        );
        assert_eq!(resolution.resolved_spec, None);
        assert_eq!(
            resolution.readiness,
            Readiness::Blocked {
                reasons: vec![BlockedReason {
                    pointer: "/variables/vpc".to_owned(),
                    reason: "network has no current receipt".to_owned(),
                }]
            }
        );
    }

    #[test]
    fn test_resolve_reference_cites_the_observed_commit_or_the_run() {
        let mut world = World::new();
        let ReferenceValue::Value { value, provenance } =
            world.resolve(json!({"unit": "network", "output": "vpcId"})).unwrap()
        else {
            panic!("network is current")
        };
        assert_eq!(value, json!("vpc-0abc123"));
        assert_eq!(provenance.observed_commit, Some(commit(COMMIT)));
        assert_eq!(provenance.run, None);
        assert_eq!(provenance.uid, uid("network"));
        assert_eq!(provenance.execution_key, key("network"));

        world.produced_in_run.insert(name("network"));
        let ReferenceValue::Value { provenance, .. } =
            world.resolve(json!({"unit": "network", "output": "vpcId"})).unwrap()
        else {
            panic!("network is current")
        };
        assert_eq!(provenance.observed_commit, None);
        assert_eq!(provenance.run, Some(run(RUN_1)));
    }

    #[test]
    fn test_resolve_unit_keeps_a_stable_citation() {
        let mut world = World::new();
        let unit = consumer(&json!({"vpc": {"fromUnit": {"unit": "network", "output": "vpcId"}}}));
        // Run R1 produced network and resolved database against it.
        world.produced_in_run.insert(name("network"));
        let first = resolve_unit(&world.view(), &unit);
        let mut stored = unit.clone();
        stored.provenance = first.provenance.clone();
        world.previous.insert(name("database"), stored);

        // A later run reads network's unchanged receipt from a newer snapshot.
        world.produced_in_run.clear();
        world.observed_commit = commit(OTHER_COMMIT);
        world.run = run(RUN_2);
        let second = resolve_unit(&world.view(), &unit);
        assert_eq!(
            serde_json::to_vec(&second.provenance).unwrap(),
            serde_json::to_vec(&first.provenance).unwrap()
        );
        let Provenance::Receipt(citation) = &second.provenance[&JsonPointer::new("/variables/vpc").unwrap()] else {
            panic!("a receipt citation")
        };
        assert_eq!(citation.run, Some(run(RUN_1)));
        assert_eq!(citation.observed_commit, None);

        // A changed receipt is cited anew.
        world.desired.get_mut(&name("network")).unwrap().execution_key = Some(key("network-v2"));
        world.receipt("network", receipt("network-v2", json!({"vpcId": "vpc-2"}), Vec::new()));
        let third = resolve_unit(&world.view(), &unit);
        let Provenance::Receipt(citation) = &third.provenance[&JsonPointer::new("/variables/vpc").unwrap()] else {
            panic!("a receipt citation")
        };
        assert_eq!(citation.observed_commit, Some(commit(OTHER_COMMIT)));
        assert_eq!(third.resolved_spec.unwrap()["variables"]["vpc"], json!("vpc-2"));
    }

    #[test]
    fn test_resolve_reference_rejects_undeclared_and_sensitive_outputs() {
        let world = World::new();
        let undeclared = world
            .resolve(json!({"unit": "network", "output": "vpcID"}))
            .unwrap_err();
        assert!(undeclared.contains("does not declare"), "{undeclared}");
        let sensitive = world
            .resolve(json!({"unit": "network", "output": "token"}))
            .unwrap_err();
        assert!(sensitive.contains("sensitive"), "{sensitive}");
        let outside = world.resolve(json!({"unit": "cache", "output": "host"})).unwrap_err();
        assert!(outside.contains("outside the ownership set"), "{outside}");
        let cross = world
            .resolve(json!({"environment": "infra", "unit": "network", "output": "vpcId"}))
            .unwrap_err();
        assert!(cross.contains("undeclared environment"), "{cross}");
    }

    #[test]
    fn test_resolve_reference_rejects_a_value_of_the_wrong_type() {
        let mut world = World::new();
        world.receipt("network", receipt("network", json!({"vpcId": 7}), Vec::new()));
        let error = world
            .resolve(json!({"unit": "network", "output": "vpcId"}))
            .unwrap_err();
        assert!(error.contains("declared type"), "{error}");
    }

    #[test]
    fn test_resolve_reference_checks_the_artifact_digest() {
        let mut world = World::new();
        let ReferenceValue::Value { value, .. } = world
            .resolve(json!({"unit": "web-image", "artifact": "image", "kind": "ContainerImage"}))
            .unwrap()
        else {
            panic!("web-image is current")
        };
        assert_eq!(value["repository"], json!("registry.example.com/web"));

        let image = world
            .artifacts
            .get_mut(&name("web-image"))
            .unwrap()
            .get_mut("image")
            .unwrap();
        image.spec["repository"] = json!("registry.example.com/tampered");
        let error = world
            .resolve(json!({"unit": "web-image", "artifact": "image"}))
            .unwrap_err();
        assert!(error.contains("digest"), "{error}");
    }

    #[test]
    fn test_resolve_reference_rejects_a_kind_mismatch() {
        let world = World::new();
        let error = world
            .resolve(json!({"unit": "web-image", "artifact": "image", "kind": "HelmChart"}))
            .unwrap_err();
        assert!(error.contains("kind HelmChart"), "{error}");
        let unlisted = world
            .resolve(json!({"unit": "web-image", "artifact": "chart"}))
            .unwrap_err();
        assert!(unlisted.contains("does not list"), "{unlisted}");
    }

    #[test]
    fn test_resolve_reference_applies_the_pointer() {
        let world = World::new();
        let ReferenceValue::Value { value, .. } = world
            .resolve(json!({"unit": "web-image", "artifact": "image", "pointer": "/tags/1"}))
            .unwrap()
        else {
            panic!("web-image is current")
        };
        assert_eq!(value, json!("b"));
        let ReferenceValue::Value { value, .. } = world
            .resolve(json!({"unit": "network", "output": "subnets", "pointer": "/a"}))
            .unwrap()
        else {
            panic!("network is current")
        };
        assert_eq!(value, json!("subnet-1"));
        let missing = world
            .resolve(json!({"unit": "network", "output": "subnets", "pointer": "/b"}))
            .unwrap_err();
        assert!(missing.contains("\"/b\""), "{missing}");
    }

    #[test]
    fn test_resolve_unit_blocks_on_depends_on_and_lists_errors() {
        let mut world = World::new();
        world.add("cache", json!({}));
        let unit = desired(
            "database",
            "Command",
            json!({
                "dependsOn": ["network", {"unit": "cache"}],
                "variables": {
                    "vpc": {"fromUnit": {"unit": "network", "output": "vpcId"}},
                    "bad": {"fromUnit": {"unit": "network", "output": "nope"}},
                },
            }),
            "database",
        );
        let resolution = resolve_unit(&world.view(), &unit);
        assert_eq!(
            resolution.readiness,
            Readiness::Blocked {
                reasons: vec![BlockedReason {
                    pointer: "/dependsOn/1".to_owned(),
                    reason: "cache has no current receipt".to_owned(),
                }]
            }
        );
        assert_eq!(resolution.errors.len(), 1, "{:?}", resolution.errors);
        assert!(resolution.errors[0].starts_with("spec.variables.bad reads output \"nope\""));
        assert_eq!(resolution.provenance.len(), 1);
        assert_eq!(resolution.resolved_spec, None);
    }

    /// Catches a consumer that asked for attested evidence resolving, and
    /// running, against a bare receipt: the one resolver rejects the
    /// requirement instead of downgrading it to `published`.
    #[test]
    fn test_resolve_rejects_evidence_requirements_until_attestations_exist() {
        let world = World::new();
        let error = world
            .resolve(json!({"unit": "network", "output": "vpcId", "evidence": "attested"}))
            .unwrap_err();
        assert!(error.contains("\"attested\""), "{error}");
        assert!(world
            .resolve(json!({"unit": "network", "output": "vpcId", "attestations": ["healthy"]}))
            .is_err());
        assert!(world
            .resolve(json!({"unit": "network", "output": "vpcId", "evidence": "published"}))
            .is_ok());
        let unit = desired(
            "database",
            "Command",
            json!({"dependsOn": [{"unit": "network", "evidence": "attested"}]}),
            "database",
        );
        let resolution = resolve_unit(&world.view(), &unit);
        assert_eq!(resolution.errors.len(), 1, "{:?}", resolution.errors);
        assert!(resolution.errors[0].starts_with("spec.dependsOn[0] requires evidence"));
        assert_eq!(resolution.resolved_spec, None);
    }
}
