//! Pure decisions over snapshots. Each module owns one rule of the
//! contracts, links its section, and has its tests.

pub mod desired;
pub mod execution_key;
#[cfg(test)]
mod fixtures;
pub mod freshness;
pub mod graph;
pub mod lease;
pub mod local;
pub mod recovery;
pub mod references;
pub mod selection;
pub mod sources;
pub mod state_location;
pub mod takeover;
pub mod transitions;
pub mod unit_state;
pub mod verification;
