//! Pure decisions over snapshots. Each module owns one rule of the
//! contracts, links its section, and has its tests.

pub mod desired;
pub mod environment_groups;
pub mod execution_key;
pub mod freshness;
pub mod graph;
pub mod lease;
pub mod references;
pub mod selection;
pub mod sources;
pub mod state_location;
pub mod unit_state;
