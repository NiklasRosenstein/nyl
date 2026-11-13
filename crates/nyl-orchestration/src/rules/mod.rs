//! Pure decisions over snapshots. Each module owns one rule of the
//! contracts, links its section, and has its tests.

pub mod checks;
pub mod desired;
pub mod drift;
pub mod evidence;
pub mod execution_key;
pub mod expiry;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod freshness;
pub mod graph;
pub mod hold;
pub mod keep;
pub mod lease;
pub mod lifecycle;
pub mod local;
pub mod observed_values;
pub mod recovery;
pub mod references;
pub mod selection;
pub mod source_ref;
pub mod sources;
pub mod state_location;
pub mod takeover;
pub mod teardown;
pub mod teardown_wait;
pub mod transitions;
pub mod unit_state;

pub mod multi_environment;
pub mod peers;

pub mod approval;
