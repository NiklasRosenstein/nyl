//! Reading observed values during a reconcile.
//!
//! Contract: [Observed values](../../../../../design/orchestration-core.md#observed-values),
//! "When Nyl observes", "Waiting", and "Unavailable reads". The decisions
//! (the record a reading leaves, what a consumer may resolve) are
//! [`crate::rules::observed_values`]'s; this module performs the reads.
//!
//! - A reconcile reads every unit that has a current receipt and declares
//!   observations once per receipt, before it resolves the unit's
//!   consumers, including a unit it did not execute: that is how a value
//!   that changed by itself reaches its consumers in one run.
//! - A unit the run executed with an `observationWait` is read again about
//!   every [`Settings::observation_poll`](crate::shell::Settings) until every
//!   declared value has one or the wait, counted on the injected clock from
//!   the execution's end and bounded by the unit's `timeout`, has passed. The
//!   run waits only when no other unit is ready, so units that do not depend
//!   on the observation are not held back.
//! - A read that failed keeps the last recorded value and warns with
//!   `NYL-OBSERVATION-FAILED`; it writes nothing.

#[allow(clippy::wildcard_imports)] // The run module's private helpers and imports.
use super::*;
use crate::rules::observed_values::{self, SettleInputs};
use nyl_core::orchestration::driver::{ObservedValues, Supported};

/// What a run remembers of one unit's reads.
struct ReadState {
    /// The receipt the reads are for.
    key: ExecutionKey,
    /// The end of the wait the run owes after executing the unit.
    wait_until: Option<Timestamp>,
    /// The earliest time the next poll may read.
    next_poll: Timestamp,
    /// Whether a declared value still has none.
    unresolved: bool,
    /// Whether the run read the unit's current receipt at all yet.
    read: bool,
}

impl ReadState {
    /// Whether the run still owes a poll: a declared value has none, and the
    /// wait is open. The wait closes only with a read that started at or after
    /// its end, so the last read is made when the wait is up, however the
    /// clock lands relative to it.
    fn waiting(&self) -> bool {
        self.unresolved && self.wait_until.is_some()
    }
}

/// The observed values a run read, what changed, and what it still waits for.
#[derive(Default)]
pub(super) struct Observing {
    states: BTreeMap<UnitName, ReadState>,
    /// Per unit, the observations whose recorded value changed.
    pub(super) changed: BTreeMap<UnitName, BTreeSet<String>>,
    /// The `NYL-OBSERVATION-FAILED` warnings of the latest read of a unit.
    pub(super) warnings: BTreeMap<UnitName, Vec<Finding>>,
}

/// One unit's read, as its thread returns it.
struct Reading {
    name: UnitName,
    result: Result<ObservedValues, String>,
}

/// What every read of a batch shares.
struct Shared<'s> {
    context: &'s RunContext<'s>,
    slots: &'s Slots,
    environment: &'s EnvironmentName,
    project_dir: &'s str,
    cache_dir: PathBuf,
    lease_deadline: Option<Timestamp>,
    directory: &'s Path,
}

/// One unit to read.
struct Job<'j> {
    unit: &'j DesiredUnit,
    observed: &'j ObservedUnit,
    artifacts: BTreeMap<String, ArtifactDocument>,
    commit: &'j CommitId,
    slot: usize,
}

impl Observing {
    /// Whether `unit` is read by a reconcile: it has a current receipt
    /// without a condition, and its kind reads observed values for its spec.
    fn eligible<'o>(
        session: &Session<'_, '_>,
        unit: &DesiredUnit,
        observed: &'o BTreeMap<UnitName, ObservedUnit>,
    ) -> Option<&'o ObservedUnit> {
        if session
            .request
            .units
            .as_ref()
            .is_some_and(|named| !named.contains(&unit.unit.name))
        {
            return None;
        }
        let current = observed
            .get(&unit.unit.name)
            .filter(|_| receipt_is_current(unit, observed.get(&unit.unit.name)))?;
        if current.condition.is_some() || !observed_values::declares(unit) {
            return None;
        }
        let spec = observed_values::spec_of(unit);
        session
            .context
            .drivers
            .get(&unit.unit.api_version, &unit.unit.kind)
            .filter(|driver| driver.capabilities(spec).observed_values)
            .map(|_| current)
    }

    /// Open the wait a run owes after it executed `unit` with `receipt`,
    /// when the unit declares an `observationWait`; `timeout` bounds it.
    pub(super) fn executed(&mut self, unit: &DesiredUnit, receipt: &Receipt, timeout: Duration, now: Timestamp) {
        let wait = observed_values::observation_wait(unit);
        let wait_until = (wait > Duration::default() && observed_values::declares(unit))
            .then(|| wait.after(receipt.finished_at).min(timeout.after(receipt.started_at)));
        self.states.insert(
            unit.unit.name.clone(),
            ReadState {
                key: receipt.execution_key.clone(),
                wait_until,
                next_poll: now,
                unresolved: true,
                read: false,
            },
        );
    }

    /// The units to read now: those with a current receipt that no read
    /// covered yet this run, and those whose wait polls again.
    pub(super) fn due(
        &self,
        session: &Session<'_, '_>,
        units: &BTreeMap<UnitName, DesiredUnit>,
        observed: &BTreeMap<UnitName, ObservedUnit>,
        now: Timestamp,
    ) -> Vec<UnitName> {
        units
            .values()
            .filter_map(|unit| {
                let current = Self::eligible(session, unit, observed)?;
                let key = &current.receipt.as_ref()?.execution_key;
                let due = match self.states.get(&unit.unit.name) {
                    Some(state) if &state.key == key => !state.read || (state.waiting() && now >= state.next_poll),
                    _ => true,
                };
                due.then(|| unit.unit.name.clone())
            })
            .collect()
    }

    /// When the next poll of a unit the run still waits for is due, if any.
    /// A wait that no read closed a poll interval after its end is given up,
    /// so a unit that stopped being readable never keeps the run waiting.
    pub(super) fn next_poll(&self, now: Timestamp, poll: Duration) -> Option<Timestamp> {
        self.states
            .values()
            .filter(|state| state.waiting() && state.wait_until.is_some_and(|until| now < poll.after(until)))
            .map(|state| state.next_poll)
            .min()
    }

    /// Read `due` and record what the readings leave in `observations`;
    /// whether any recorded value changed, so that consumers resolve again.
    #[allow(clippy::too_many_arguments)] // One pass: the run's session, its resolution, and the evidence it overlays.
    pub(super) fn read(
        &mut self,
        session: &Session<'_, '_>,
        prepared: &Prepared,
        resolved: &Resolved,
        observed: &BTreeMap<UnitName, ObservedUnit>,
        artifacts: &BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        observations: &mut BTreeMap<UnitName, ObservationRecord>,
        due: &[UnitName],
    ) -> bool {
        let context = session.context;
        let environment = session.environment();
        let directory = session.directory.join("observe");
        let shared = Shared {
            context,
            slots: &session.slots,
            environment,
            project_dir: &prepared.rendered.project_dir,
            cache_dir: context.cache_dir(),
            lease_deadline: session.lease_deadline.get(),
            directory: &directory,
        };
        let mut any = false;
        for batch in due.chunks(session.request.concurrency.max(1)) {
            let jobs: Vec<Job<'_>> = batch
                .iter()
                .enumerate()
                .map(|(slot, name)| Job {
                    unit: &resolved.units[name],
                    observed: &observed[name],
                    artifacts: artifacts.get(name).cloned().unwrap_or_default(),
                    commit: &resolved.commits[name],
                    slot,
                })
                .collect();
            let started = context.base.clock.now();
            let readings: Vec<Reading> = std::thread::scope(|scope| {
                let handles: Vec<_> = jobs
                    .into_iter()
                    .map(|job| {
                        let shared = &shared;
                        scope.spawn(move || read_unit(shared, &job))
                    })
                    .collect();
                handles.into_iter().map(join).collect()
            });
            let now = context.base.clock.now();
            for reading in readings {
                any |= self.record(
                    session,
                    &reading,
                    &resolved.units[&reading.name],
                    observed[&reading.name]
                        .receipt
                        .as_ref()
                        .expect("a unit with a current receipt has one"),
                    observations,
                    (started, now),
                );
            }
        }
        let _ = std::fs::remove_dir_all(&directory);
        any
    }

    /// Fold one reading into the run's records, the changes, and the
    /// warnings; whether the unit's record changed.
    fn record(
        &mut self,
        session: &Session<'_, '_>,
        reading: &Reading,
        unit: &DesiredUnit,
        receipt: &Receipt,
        observations: &mut BTreeMap<UnitName, ObservationRecord>,
        (started, now): (Timestamp, Timestamp),
    ) -> bool {
        let name = &unit.unit.name;
        let environment = session.environment();
        let key = receipt.execution_key.clone();
        let warn = |reason: &str| {
            Finding::new(
                Code::ObservationFailed,
                format!("observed values of {name} could not be read: {reason}; the last recorded value for its current receipt stays usable"),
                format!("fix the cause, such as missing credentials, and run `nyl observe -e {environment}` or `nyl reconcile -e {environment}` again"),
            )
        };
        let declared = match observed_values::declarations(observed_values::spec_of(unit)) {
            Ok(declared) => declared,
            Err(message) => {
                self.warnings.insert(name.clone(), vec![warn(&message)]);
                return false;
            }
        };
        let readings = match &reading.result {
            Ok(readings) => readings.clone(),
            Err(message) => {
                self.warnings.insert(name.clone(), vec![warn(message)]);
                // Nothing was read: the unit stays unresolved for its wait.
                self.settle_state(name, &key, observations.get(name), &declared, (started, now), session);
                return false;
            }
        };
        let settled = observed_values::settle(&SettleInputs {
            unit,
            receipt,
            declared: &declared,
            stored: observations.get(name),
            readings: &readings,
            now,
            refresh: session.base().settings.observation_refresh,
        });
        let before = observations.get(name).cloned();
        let record_changed = settled.record != before;
        match settled.record.clone() {
            Some(record) => {
                observations.insert(name.clone(), record);
            }
            None => {
                observations.remove(name);
            }
        }
        if !settled.changed.is_empty() {
            self.changed
                .entry(name.clone())
                .or_default()
                .extend(settled.changed.iter().cloned());
        }
        let findings: Vec<Finding> = settled.failed.values().map(|reason| warn(reason)).collect();
        if findings.is_empty() {
            self.warnings.remove(name);
        } else {
            self.warnings.insert(name.clone(), findings);
        }
        self.settle_state(name, &key, settled.record.as_ref(), &declared, (started, now), session);
        record_changed
    }

    /// Update the unit's wait state after a read: unresolved while a
    /// declared value has none, and next due a poll from now.
    fn settle_state(
        &mut self,
        name: &UnitName,
        key: &ExecutionKey,
        record: Option<&ObservationRecord>,
        declared: &BTreeMap<String, nyl_core::resources::units::ObservationDeclaration>,
        (started, now): (Timestamp, Timestamp),
        session: &Session<'_, '_>,
    ) {
        let unresolved = declared.keys().any(|observation| {
            !record.is_some_and(|record| {
                record.execution_key == *key
                    && matches!(
                        record.values.get(observation),
                        Some(nyl_core::orchestration::state::RecordedObservation::Value { .. })
                    )
            })
        });
        let poll = session.base().settings.observation_poll;
        let state = self.states.entry(name.clone()).or_insert_with(|| ReadState {
            key: key.clone(),
            wait_until: None,
            next_poll: now,
            unresolved,
            read: true,
        });
        if &state.key != key {
            *state = ReadState {
                key: key.clone(),
                wait_until: None,
                next_poll: now,
                unresolved,
                read: true,
            };
        }
        state.read = true;
        state.unresolved = unresolved;
        // The next poll is a poll interval away, or the wait's end when that
        // comes first; a read made at the end closes the wait.
        state.next_poll = poll.after(now).min(state.wait_until.unwrap_or(Timestamp::MAX_UTC));
        if state.wait_until.is_some_and(|until| started >= until) {
            state.wait_until = None;
        }
    }
}

/// Read one unit's observed values in a worktree of its effective commit.
fn read_unit(shared: &Shared<'_>, job: &Job<'_>) -> Reading {
    let name = job.unit.unit.name.clone();
    Reading {
        result: read(shared, job),
        name,
    }
}

fn read(shared: &Shared<'_>, job: &Job<'_>) -> Result<ObservedValues, String> {
    let context = shared.context;
    let unit = job.unit;
    let name = &unit.unit.name;
    let driver = context
        .drivers
        .get(&unit.unit.api_version, &unit.unit.kind)
        .ok_or_else(|| format!("no driver serves kind {}", unit.unit.kind))?;
    let receipt = job
        .observed
        .receipt
        .as_ref()
        .ok_or_else(|| format!("{name} has no receipt"))?;
    let temp_dir = shared.directory.join(format!("tmp-{name}"));
    std::fs::create_dir_all(&temp_dir).map_err(|error| format!("cannot create {}: {error}", temp_dir.display()))?;
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let result = shared
        .slots
        .checkout_unit(context.sources, job.slot, job.commit, driver, spec, shared.project_dir)
        .and_then(|(worktree, source_roots)| {
            let (env, secrets) = execution_credentials(context, driver, &unit.spec)?;
            let now = context.base.clock.now();
            let timeout = unit_timeout(context.drivers, unit);
            let deadline = shared.lease_deadline.map_or_else(
                || timeout.after(now),
                |lease| {
                    timeout
                        .after(now)
                        .min(crate::shell::lease::bound(&context.base.settings, lease))
                },
            );
            let budget = crate::shell::tools::DeadlineRunner {
                tools: context.tools,
                clock: context.base.clock,
                deadline,
            };
            let ctx = ExecutionContext {
                instances: shared.context.instances,
                secrets,
                interactive: false,
                artifacts: job.artifacts.clone(),
                source_roots,
                unit: UnitAddress::new(shared.environment.clone(), name.clone()),
                operation: DriverOperation::Observe,
                project_dir: project_path(&worktree, shared.project_dir),
                worktree,
                temp_dir: temp_dir.clone(),
                env,
                approved_digest: None,
                deadline,
                now,
                observed: Some(job.observed),
                tools: &budget,
                commit_identity: context.commit_identity,
                cache_dir: shared.cache_dir.clone(),
                teardown_wait: None,
                incarnations: &nyl_core::orchestration::driver::UnknownIncarnations,
                dirty_source: false,
            };
            let outcome = driver.observe(&ctx, unit, receipt);
            drop(ctx);
            if context.base.clock.now() >= deadline {
                return Err("timeout: reading the observed values passed the unit's deadline".to_owned());
            }
            match outcome {
                Ok(Supported::Supported(values)) => Ok(values),
                Ok(Supported::Unsupported) => Err(format!("kind {} cannot read observed values", unit.unit.kind)),
                Err(error) => Err(error.to_string()),
            }
        });
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}
