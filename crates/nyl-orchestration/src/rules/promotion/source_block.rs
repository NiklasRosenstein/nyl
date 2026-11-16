//! Validation of a `record: source` block against its path and the source
//! state it names.
//!
//! Contract: [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source):
//! "validation rejects a block that does not match the source state it
//! names". The block lives in source, where anyone can edit it, so a run
//! does not take it on trust the way it takes a record `nyl promote` wrote
//! into state. [`check_block`] compares it with the PromotionPath it names
//! (the path's source environment, its declared values and their
//! selectors) and with the state it names, which the shell reads from that
//! environment's history by the block's desired commit: the state's run,
//! its source commit, and every value, read again from that state through
//! the same reader `nyl promote` used (the shared reference resolver), at
//! the time the promotion was requested.

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::promotion::{PromotionFrom, PromotionRecord};
use nyl_core::resources::promotion::PromotionPath;

use super::values::{self, Unreadable};
use super::RecordedState;

/// Check `block` against `path`, the PromotionPath it names, and `state`,
/// the recorded state of the path's source environment its `from` names (by
/// desired commit). A mismatch is `NYL-PROMOTION-RECORD-INVALID`.
///
/// The path promotes the target's source, so every value comes from `from`:
/// a value that names a state of its own is refused.
pub fn check_block(block: &PromotionRecord, path: &PromotionPath, state: &RecordedState) -> Result<(), Finding> {
    let invalid = |problem: String| {
        Finding::new(
            Code::PromotionRecordInvalid,
            format!(
                "the promoted block of path {} (sequence {}) does not match {problem}",
                block.path, block.sequence
            ),
            "restore the block `nyl promote` wrote, or promote again; the block is written by `nyl promote`, never by hand",
        )
    };
    if block.path.as_str() != path.metadata.name {
        return Err(invalid(format!(
            "path {}, which it was checked against",
            path.metadata.name
        )));
    }
    let Some(source) = path
        .spec
        .from
        .environment
        .as_deref()
        .and_then(|name| EnvironmentName::new(name).ok())
    else {
        return Err(invalid("its path, which names no source environment".to_owned()));
    };
    let PromotionFrom::Environment(from) = &block.from else {
        return Err(invalid("its path: it names a target as its source".to_owned()));
    };
    if from.environment != source {
        return Err(invalid(format!(
            "its path, which promotes from {source}: it names a state of {}",
            from.environment
        )));
    }
    if from.desired_commit != state.desired_commit {
        return Err(invalid(format!(
            "the state of {source} it was checked against: it names {}, not {}",
            from.desired_commit, state.desired_commit
        )));
    }
    let named = |problem: String| invalid(format!("the state of {source} it names: {problem}"));
    if from.run != state.run || from.observed_commit != state.observed_commit {
        return Err(named(format!(
            "the state was recorded by run {}, not {}",
            state.run, from.run
        )));
    }
    if block.source_commit.as_ref() != state.source_commit() {
        return Err(named(format!(
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
    let declared: BTreeSet<&String> = path.spec.values.keys().collect();
    let carried: BTreeSet<&String> = block.values.keys().collect();
    if declared != carried {
        return Err(invalid(format!(
            "its path: it carries the values {carried:?}, and the path declares {declared:?}"
        )));
    }
    for (name, value) in &block.values {
        // A path that promotes the source takes every value from `from`.
        if value.state.is_some() {
            return Err(invalid(format!(
                "its path: value {name:?} names a state of its own, and a path that promotes the source takes every value from one state"
            )));
        }
        let select = &path.spec.values[name].select;
        if &value.select != select {
            return Err(invalid(format!(
                "its path: value {name:?} records another selector than the path declares"
            )));
        }
        let read = values::read(&source, state, name, select, block.requested.at).map_err(|unreadable| {
            named(match unreadable {
                Unreadable::Invalid(finding) => format!("value {name:?} cannot be read there: {}", finding.message),
                Unreadable::Blocked { message, .. } => message,
            })
        })?;
        if &read.value != value {
            return Err(named(format!(
                "value {name:?} there is {}, not {}",
                serde_json::to_string(&read.value).unwrap_or_default(),
                serde_json::to_string(value).unwrap_or_default()
            )));
        }
    }
    Ok(())
}
