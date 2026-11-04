//! Emit execution lifecycle events from the shell using its injected clock.
//! Contract: [Execution](../../../../../design/orchestration-core.md#execution).
use super::run::RunContext;
use nyl_core::orchestration::ids::UnitAddress;
use nyl_core::orchestration::progress::{ExecutionReason, UnitPhase, UnitProgress, UnitResult};
use nyl_core::orchestration::state::{DesiredUnit, Operation};
use nyl_core::orchestration::time::Timestamp;

/// One dispatched unit's progress, including preparation and driver execution.
pub(crate) struct Progress<'a> {
    context: &'a RunContext<'a>,
    event: UnitProgress,
}

impl<'a> Progress<'a> {
    pub(crate) fn start(
        context: &'a RunContext<'a>,
        unit: UnitAddress,
        kind: &str,
        operation: Operation,
        reason: Option<ExecutionReason>,
    ) -> Self {
        let started = context.base.clock.now();
        let event = UnitProgress {
            unit,
            kind: kind.to_owned(),
            operation,
            phase: UnitPhase::Started { at: started, reason },
        };
        if let Some(observer) = context.progress {
            observer.observe(&event);
        }
        Self { context, event }
    }

    pub(crate) fn for_unit(
        context: &'a RunContext<'a>,
        environment: &nyl_core::orchestration::ids::EnvironmentName,
        unit: &DesiredUnit,
        operation: Operation,
        reason: Option<ExecutionReason>,
    ) -> Self {
        Self::start(
            context,
            UnitAddress::new(environment.clone(), unit.unit.name.clone()),
            &unit.unit.kind,
            operation,
            reason,
        )
    }

    pub(crate) fn into_event(self) -> UnitProgress {
        self.event
    }

    pub(crate) fn finish(self, result: &UnitResult, message: Option<&str>) {
        Self::finish_event(self.context, self.event, self.context.base.clock.now(), result, message);
    }

    pub(crate) fn finish_event(
        context: &RunContext<'_>,
        mut event: UnitProgress,
        at: Timestamp,
        result: &UnitResult,
        message: Option<&str>,
    ) {
        let UnitPhase::Started { at: started, .. } = event.phase else {
            return;
        };
        event.phase = UnitPhase::Finished {
            started,
            at,
            result: result.clone(),
            message: message.map(str::to_owned),
        };
        if let Some(observer) = context.progress {
            observer.observe(&event);
        }
    }
}
