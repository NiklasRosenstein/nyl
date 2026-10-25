//! `nyl verify -e <env>`: driver verification against current receipts.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (`verify` takes the lease, its deadline set by the timeouts of the units
//! it verifies), [Drivers](../../../../design/orchestration-core.md#drivers),
//! and [Inspection](../../../../design/orchestration-core.md#inspection).
//!
//! Reads one snapshot, picks the units with a current receipt
//! ([`crate::rules::verification::targets`]), checks each out at its
//! effective commit in a concurrency slot ([`super::run::slots`]), calls the
//! driver's `verify`, and writes one observed commit (operation `verify`)
//! with the latest observation per type and the driver's attestations; it
//! never writes a receipt and writes nothing when nothing changed. The
//! report is a `VerifyReport`, rendered like a reconcile's.

use super::run::{RunContext, RunRequest};
use crate::run_report::RunReport;

/// `nyl verify -e <env>`; `request.units` limits the units verified.
pub fn verify(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let _ = (context, request);
    todo!("recover-verify")
}
