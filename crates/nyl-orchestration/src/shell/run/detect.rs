//! `nyl reconcile --detect-drift`: the drift check a run makes under its own
//! lease before it selects what to execute.
//!
//! Contract: [Drift](../../../../../design/orchestration-core.md#drift) and
//! [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases).
//!
//! The run checks the units [`crate::rules::checks::reconcile_targets`]
//! names, each as it resolved it, through the code `nyl detect-drift` runs
//! ([`crate::shell::check`]): the driver's `detect_drift`, the drift record,
//! and `settle_drift`. It writes nothing itself. The results go back to the
//! run, which selects with the fresh records laid over the snapshot's, so
//! the one selection rule decides what to repair, and writes the records in
//! its own transition commit (never a second one), only while they still
//! describe the unit's receipt.

#[allow(clippy::wildcard_imports)] // The run module's private helpers and imports.
use super::*;
use crate::run_report::CheckResult;
use crate::shell::check::{self, Halted};
use nyl_core::orchestration::state::DriftRecord;

/// What a run's drift check learned.
#[derive(Debug, Default)]
pub(super) struct Detection {
    /// Each checked unit's result, for its report entry.
    pub(super) checks: BTreeMap<UnitName, CheckResult>,
    /// The records of the checks that concluded, as settled against the
    /// stored ones, with the receipt each describes, so the commit writes
    /// one only while that receipt still stands.
    pub(super) records: BTreeMap<UnitName, (DriftRecord, Receipt)>,
}

impl Detection {
    /// The drift records as this run stands: the snapshot's, with what the
    /// check found laid over them, so selection sees the fresh results.
    pub(super) fn overlay(&self, stored: &BTreeMap<UnitName, DriftRecord>) -> BTreeMap<UnitName, DriftRecord> {
        let mut records = stored.clone();
        records.extend(
            self.records
                .iter()
                .map(|(name, (record, _))| (name.clone(), record.clone())),
        );
        records
    }
}

impl Reconcile<'_, '_, '_> {
    /// Check `targets` for drift, with the lease's deadline moved for each
    /// batch as a standalone check moves it. A driver failure is an error in
    /// the report (the check recorded nothing for the unit) and the run
    /// goes on; a signal ends it as it ends a run between waves.
    pub(super) fn detect_drift(
        &mut self,
        targets: &BTreeSet<UnitName>,
        selectable: &BTreeMap<UnitName, DesiredUnit>,
        observed: &BTreeMap<UnitName, ObservedUnit>,
        artifacts: &BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        stored: &BTreeMap<UnitName, DriftRecord>,
        project_dir: &str,
    ) -> Result<Detection, Ending> {
        let mut detection = Detection::default();
        if targets.is_empty() {
            return Ok(detection);
        }
        let context = self.context();
        let request = self.session.request;
        // The units as this run resolved them, with the receipts and
        // artifacts as the run stands: what its own executions would see.
        let view = EnvironmentSnapshot {
            desired: targets
                .iter()
                .map(|name| (name.clone(), selectable[name].clone()))
                .collect(),
            observed: observed.clone(),
            artifacts: artifacts.clone(),
            drift: stored.clone(),
            ..EnvironmentSnapshot::default()
        };
        let names: Vec<&UnitName> = targets.iter().collect();
        let mut extend = |longest: Duration| -> Result<Timestamp, Ending> {
            let deadline = lease::deadline(context.base, context.base.clock.now(), longest);
            self.phase(Vec::new(), deadline, &[])?;
            Ok(self.held.lease.deadline)
        };
        let checked = match check::detect_in_run(context, request, &view, &names, project_dir, &mut extend) {
            Ok(checked) => checked,
            Err(Halted::Cancelled(_)) => return Err(self.cancelled()),
            Err(Halted::Lease(ending)) => return Err(ending),
        };
        for unit in checked {
            self.report.errors.extend(unit.error.clone());
            if let Some(record) = unit.drift_record() {
                if let Some(receipt) = observed.get(&unit.name).and_then(|observed| observed.receipt.clone()) {
                    detection.records.insert(unit.name.clone(), (record.clone(), receipt));
                }
            }
            detection.checks.insert(unit.name, unit.result.result);
        }
        Ok(detection)
    }
}
