//! External attestations: `nyl attest -e <env> [--unit <u>] --name <n>
//! --result pass|fail`.
//!
//! Contract: [Attestations](../../../../design/orchestration-core.md#attestations)
//! and the [promotion contract](../../../../design/promotion.md#attestations).
//!
//! Someone or something else knows a result, so the caller supplies it and
//! Nyl only records it. [`record`] decides, as a pure function of the
//! snapshot, what the new attestation set of its scope is:
//!
//! - **Unit scope** (`--unit`): the unit's current receipt
//!   ([`super::freshness::receipt_is_current`]) is attested, keyed by the
//!   receipt's execution key. The name must be declared in the unit's
//!   `spec.attestations` with `from: external`; a name its driver reports,
//!   or one declared without `from`, is the driver's to record
//!   (`NYL-ATTESTATION-UNDECLARED`). A unit without a current receipt is
//!   `NYL-ATTEST-NO-RECEIPT`. Stored attestations of an earlier receipt are
//!   dropped with the write, like a driver attest drops them.
//! - **Environment scope**: the environment's current state is attested, keyed
//!   to the state's desired commit
//!   ([`StateStore::state_commit`](nyl_core::orchestration::ports::StateStore::state_commit)):
//!   the newest commit that changed this environment's `desired/` documents,
//!   not the ref's tip, which observed-only commits (this command's own
//!   included), other environments of a shared ref, and a ref shared by both
//!   sides all move. The name must be in `Environment.spec.attestations`.
//!   Names recorded for other states stay; the newest per name wins and
//!   readers compare the commit.
//!
//! The attester is recorded under `source: {external: {by, identity,
//! source, url}}`, with the identity the approval machinery resolved
//! (`verified`, `asserted`, or `local`). Recording always writes: a repeated
//! result is a new attestation with a new time, which is what keeps evidence
//! fresh for `maxAttestationAge`.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{
    ApprovalRecord, ArtifactUnit, Attestation, AttestationKind, AttestationResult, StateV1,
};
use nyl_core::orchestration::time::Timestamp;
use serde_json::{json, Value};

use super::freshness::receipt_is_current;

/// What the caller states: one result for one name.
#[derive(Debug, Clone)]
pub struct External {
    pub name: String,
    pub result: AttestationResult,
    pub reason: Option<String>,
    pub url: Option<String>,
    /// Who records it and how Nyl knows.
    pub attester: ApprovalRecord,
}

/// The complete attestation set a scope holds after the recording.
#[derive(Debug, Clone, PartialEq)]
pub enum Recording {
    Unit {
        unit: UnitName,
        set: BTreeMap<String, Attestation>,
    },
    Environment {
        set: BTreeMap<String, Attestation>,
    },
}

/// The names `spec.attestations` declares with `from: external`.
fn external_names(spec: &Value) -> Vec<String> {
    spec.get("attestations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.get("from").and_then(Value::as_str) == Some("external"))
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn undeclared(scope: &str, name: &str, declared: &[String], elsewhere: bool, fix_scope: &str) -> Finding {
    let message = if declared.is_empty() {
        format!("{scope} declares no external attestation, so {name:?} cannot be recorded for it")
    } else {
        format!(
            "{scope} does not declare {name:?} as an external attestation; it declares {}",
            declared.join(", ")
        )
    };
    let fix = if elsewhere {
        format!("{name} is produced by the driver: record it with `nyl attest` without --name, or declare it in {fix_scope} with `from: external`")
    } else if declared.is_empty() {
        format!("declare it in {fix_scope}, then run the command again")
    } else {
        format!("use one of {}, or declare {name} in {fix_scope}", declared.join(", "))
    };
    Finding::new(Code::AttestationUndeclared, message, fix)
}

impl External {
    /// `source: {external: {by, identity, source, url}}`.
    fn source(&self) -> Value {
        let mut external = json!({
            "by": self.attester.by,
            "identity": self.attester.identity,
        });
        if let Some(source) = &self.attester.source {
            external["source"] = json!(source);
        }
        if let Some(url) = &self.url {
            external["url"] = json!(url);
        }
        json!({ "external": external })
    }

    fn attestation(&self, subject: Option<ArtifactUnit>, key: String, now: Timestamp) -> Attestation {
        Attestation {
            api_version: StateV1,
            kind: AttestationKind,
            name: self.name.clone(),
            unit: subject,
            execution_key: key,
            result: self.result,
            at: now,
            source: self.source(),
            reason: self.reason.clone(),
            details: Value::Null,
        }
    }
}

/// The attestation set of the unit's current receipt, or of the
/// environment's current state when `unit` is `None`, with `request`
/// recorded at `now`.
pub fn record(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    unit: Option<&UnitName>,
    state_commit: Option<&CommitId>,
    request: &External,
    now: Timestamp,
) -> Result<Recording, Finding> {
    match unit {
        Some(unit) => record_unit(environment, snapshot, unit, request, now),
        None => record_environment(environment, snapshot, state_commit, request, now),
    }
}

fn record_unit(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    unit: &UnitName,
    request: &External,
    now: Timestamp,
) -> Result<Recording, Finding> {
    let desired = snapshot.desired.get(unit).ok_or_else(|| {
        Finding::new(
            Code::UnitUnknown,
            format!("--unit {unit} names no unit in the state of {environment}"),
            format!("list the environment's units with `nyl get units -e {environment}`"),
        )
    })?;
    let declared = external_names(&desired.spec);
    if !declared.contains(&request.name) {
        let elsewhere =
            desired.attestations.contains(&request.name) || super::evidence::declared(desired).contains(&request.name);
        return Err(undeclared(
            &format!("unit {unit}"),
            &request.name,
            &declared,
            elsewhere,
            &format!("{unit}'s `spec.attestations`"),
        ));
    }
    let observed = snapshot.observed.get(unit);
    let receipt = observed
        .filter(|_| receipt_is_current(desired, observed))
        .and_then(|observed| observed.receipt.as_ref())
        .ok_or_else(|| {
            Finding::new(
                Code::AttestNoReceipt,
                format!("unit {unit} has no current receipt in {environment} to attest"),
                format!("run `nyl reconcile -e {environment} --unit {unit}`, then record the attestation"),
            )
        })?;
    let key = receipt.execution_key.to_string();
    let subject = desired.unit.uid.clone().map(|uid| ArtifactUnit {
        environment: environment.clone(),
        name: unit.clone(),
        uid,
    });
    // An earlier receipt's attestations no longer describe the unit.
    let mut set: BTreeMap<String, Attestation> = snapshot
        .attestations
        .get(unit)
        .into_iter()
        .flatten()
        .filter(|(_, stored)| {
            stored.execution_key == key
                && stored
                    .unit
                    .as_ref()
                    .is_none_or(|subject| Some(&subject.uid) == desired.unit.uid.as_ref())
        })
        .map(|(name, stored)| (name.clone(), stored.clone()))
        .collect();
    set.insert(request.name.clone(), request.attestation(subject, key, now));
    Ok(Recording::Unit {
        unit: unit.clone(),
        set,
    })
}

fn record_environment(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    state_commit: Option<&CommitId>,
    request: &External,
    now: Timestamp,
) -> Result<Recording, Finding> {
    let commit = state_commit.filter(|_| snapshot.environment.is_some()).ok_or_else(|| {
        Finding::new(
            Code::AttestNoReceipt,
            format!("{environment} has no recorded state to attest: nothing was reconciled yet"),
            format!("run `nyl reconcile -e {environment}`, then record the attestation"),
        )
    })?;
    let declared = snapshot
        .environment
        .as_ref()
        .map(|record| super::evidence::declared_environment(&record.spec))
        .unwrap_or_default();
    if !declared.contains(&request.name) {
        return Err(undeclared(
            &format!("environment {environment}"),
            &request.name,
            &declared,
            false,
            "the Environment's `spec.attestations`",
        ));
    }
    let mut set = snapshot.environment_attestations.clone();
    set.insert(request.name.clone(), request.attestation(None, commit.to_string(), now));
    Ok(Recording::Environment { set })
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::state::{ApprovalIdentity, EnvironmentRecord, EnvironmentRecordKind, SourceChoice};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{commit, desired, env, key, name, observed, receipt, COMMIT};

    fn request(name: &str, result: AttestationResult) -> External {
        External {
            name: name.to_owned(),
            result,
            reason: Some("signed off".to_owned()),
            url: Some("https://ci.example/1".to_owned()),
            attester: ApprovalRecord {
                by: vec!["alice".to_owned()],
                identity: ApprovalIdentity::Verified,
                source: Some("hook lookup".to_owned()),
                digest: None,
                reviewed: false,
            },
        }
    }

    fn now() -> Timestamp {
        "2026-09-25T10:00:00Z".parse().unwrap()
    }

    fn snapshot(spec: Value, with_receipt: bool) -> EnvironmentSnapshot {
        let mut snapshot = EnvironmentSnapshot::default();
        snapshot
            .desired
            .insert(name("gate"), desired("gate", "Command", spec, "g"));
        let receipt = with_receipt.then(|| receipt("g", json!({}), Vec::new()));
        snapshot
            .observed
            .insert(name("gate"), observed("gate", "Command", receipt));
        snapshot.environment = Some(EnvironmentRecord {
            api_version: StateV1,
            kind: EnvironmentRecordKind,
            environment: env(),
            spec: json!({"attestations": ["qa"]}),
            source_commit: commit(COMMIT),
            source_chosen_by: SourceChoice::Entry,
            applied_promotion_sequence: None,
            ownership: BTreeMap::new(),
        });
        snapshot
    }

    const SPEC: fn() -> Value = || json!({"attestations": [{"name": "qa", "from": "external"}, {"name": "smoke"}]});

    /// Catches an external result recorded for the wrong subject or without
    /// its attester: a unit's attestation names the current receipt and the
    /// unit's incarnation, and its source holds identity, source, and url.
    #[test]
    fn test_a_unit_attestation_names_the_current_receipt_and_its_attester() {
        let snapshot = snapshot(SPEC(), true);
        let recorded = record(
            &env(),
            &snapshot,
            Some(&name("gate")),
            None,
            &request("qa", AttestationResult::Fail),
            now(),
        )
        .unwrap();
        let Recording::Unit { unit, set } = recorded else {
            panic!("unit scope");
        };
        assert_eq!(unit, name("gate"));
        let qa = &set["qa"];
        assert_eq!(qa.execution_key, key("g").to_string());
        assert_eq!(qa.result, AttestationResult::Fail);
        assert_eq!(qa.unit.as_ref().unwrap().name, name("gate"));
        assert_eq!(
            qa.source,
            json!({"external": {"by": ["alice"], "identity": "verified", "source": "hook lookup", "url": "https://ci.example/1"}})
        );
        assert_eq!(qa.reason.as_deref(), Some("signed off"));
    }

    /// Catches evidence nobody requires, and names the driver owns: a typo,
    /// a name declared without `from: external`, and an undeclared unit scope
    /// are refused with `NYL-ATTESTATION-UNDECLARED`, each with its fix.
    #[test]
    fn test_undeclared_names_are_refused_with_their_fix() {
        let snapshot = snapshot(SPEC(), true);
        for (unit, name, fix) in [
            (Some("gate"), "q", "use one of qa"),
            (Some("gate"), "smoke", "produced by the driver"),
            (None, "q", "use one of qa"),
        ] {
            let unit = unit.map(self::name);
            let finding = record(
                &env(),
                &snapshot,
                unit.as_ref(),
                Some(&commit(COMMIT)),
                &request(name, AttestationResult::Pass),
                now(),
            )
            .unwrap_err();
            assert_eq!(finding.code, Code::AttestationUndeclared, "{name}");
            assert!(finding.fix.contains(fix), "{}", finding.fix);
        }
        let none = super::tests::snapshot(json!({}), true);
        let finding = record(
            &env(),
            &none,
            Some(&name("gate")),
            None,
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap_err();
        assert!(
            finding.message.contains("declares no external attestation"),
            "{}",
            finding.message
        );
    }

    /// Catches an attestation recorded for a receipt that is not current, or
    /// for a state that does not exist, which no reader would ever count.
    #[test]
    fn test_a_scope_without_a_current_receipt_or_state_cannot_be_attested() {
        let no_receipt = snapshot(SPEC(), false);
        let finding = record(
            &env(),
            &no_receipt,
            Some(&name("gate")),
            None,
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap_err();
        assert_eq!(finding.code, Code::AttestNoReceipt);
        let no_state = snapshot(SPEC(), true);
        let finding = record(
            &env(),
            &no_state,
            None,
            None,
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap_err();
        assert_eq!(finding.code, Code::AttestNoReceipt);
        let finding = record(
            &env(),
            &no_state,
            Some(&name("nope")),
            None,
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap_err();
        assert_eq!(finding.code, Code::UnitUnknown);
    }

    /// Catches the environment scope keying on anything but the desired
    /// commit, and a new unit attestation keeping the ones of a replaced
    /// receipt while it keeps the current receipt's others.
    #[test]
    fn test_environment_attestations_name_the_desired_commit_and_unit_sets_drop_stale_entries() {
        let mut snapshot = snapshot(SPEC(), true);
        let Recording::Environment { set } = record(
            &env(),
            &snapshot,
            None,
            Some(&commit(COMMIT)),
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap() else {
            panic!("environment scope");
        };
        assert_eq!(set["qa"].execution_key, COMMIT);
        assert!(set["qa"].unit.is_none());

        let current = |name: &str, seed: &str| Attestation {
            api_version: StateV1,
            kind: AttestationKind,
            name: name.to_owned(),
            unit: None,
            execution_key: key(seed).to_string(),
            result: AttestationResult::Pass,
            at: now(),
            source: json!({"driver": "attest"}),
            reason: None,
            details: Value::Null,
        };
        snapshot.attestations.insert(
            name("gate"),
            BTreeMap::from([
                ("smoke".to_owned(), current("smoke", "g")),
                ("old".to_owned(), current("old", "earlier")),
            ]),
        );
        let Recording::Unit { set, .. } = record(
            &env(),
            &snapshot,
            Some(&name("gate")),
            None,
            &request("qa", AttestationResult::Pass),
            now(),
        )
        .unwrap() else {
            panic!("unit scope");
        };
        assert_eq!(set.keys().collect::<Vec<_>>(), ["qa", "smoke"]);
    }
}
