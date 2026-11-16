//! Validation of a `record: source` block against the source state it names.
//!
//! Contract: [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source):
//! "validation rejects a block that does not match the source state it
//! names". The block lives in source, where anyone can edit it, so a run
//! does not take it on trust the way it takes a record `nyl promote` wrote
//! into state. [`check_block`] compares it with the state it names, which
//! the shell reads from the source environment's history by the block's
//! desired commit: the state's run, its source commit, and every value,
//! read again from that state through the same reader `nyl promote` used
//! (the shared reference resolver), at the time the promotion was requested.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::promotion::{PromotionFrom, PromotionRecord};

use super::values::{self, Unreadable};
use super::RecordedState;

/// Check `block` against `state`, the recorded state of `environment` its
/// `from` names (by desired commit). A mismatch is
/// `NYL-PROMOTION-RECORD-INVALID`.
pub fn check_block(
    block: &PromotionRecord,
    environment: &EnvironmentName,
    state: &RecordedState,
) -> Result<(), Finding> {
    let invalid = |problem: String| {
        Finding::new(
            Code::PromotionRecordInvalid,
            format!(
                "the promoted block of path {} (sequence {}) does not match the state of {environment} it names: {problem}",
                block.path, block.sequence
            ),
            "restore the block `nyl promote` wrote, or promote again; the block is written by `nyl promote`, never by hand",
        )
    };
    let PromotionFrom::Environment(from) = &block.from else {
        return Err(invalid("it names a target as its source".to_owned()));
    };
    if &from.environment != environment || from.desired_commit != state.desired_commit {
        return Err(invalid(format!(
            "it names state {} of {}",
            from.desired_commit, from.environment
        )));
    }
    if from.run != state.run || from.observed_commit != state.observed_commit {
        return Err(invalid(format!(
            "the state was recorded by run {}, not {}",
            state.run, from.run
        )));
    }
    if block.source_commit.as_ref() != state.source_commit() {
        return Err(invalid(format!(
            "the state ran source commit {}, not {}",
            state
                .source_commit()
                .map_or_else(|| "(none)".to_owned(), ToString::to_string),
            block
                .source_commit
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), ToString::to_string),
        )));
    }
    for (name, value) in &block.values {
        // A value another state supplied names that state; a path that
        // promotes the source takes every value from `from`.
        if value.state.is_some() {
            continue;
        }
        let read = values::read(environment, state, name, &value.select, block.requested.at).map_err(|unreadable| {
            invalid(match unreadable {
                Unreadable::Invalid(finding) => format!("value {name:?} cannot be read there: {}", finding.message),
                Unreadable::Blocked { message, .. } => message,
            })
        })?;
        let mut expected = read.value;
        expected.state.clone_from(&value.state);
        if &expected != value {
            return Err(invalid(format!(
                "value {name:?} there is {}, not {}",
                serde_json::to_string(&expected).unwrap_or_default(),
                serde_json::to_string(value).unwrap_or_default()
            )));
        }
    }
    Ok(())
}
