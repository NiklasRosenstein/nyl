//! The cluster end-to-end suite of Nyl.
//!
//! One scenario runs against a kind cluster with each reconciler (the
//! variants): Nyl's direct reconciler, and Argo CD read through the
//! Kubernetes API or through its API server. The suite drives the real `nyl`
//! binary as a subprocess, the way a CI job does, so it depends on no Nyl crate.
//!
//! The pieces, from the bottom up:
//!
//! - [`cmd`], [`wait`], [`log`]: running programs with captured output,
//!   polling with backoff and good timeout messages, and CI-friendly output;
//! - [`infra`]: the kind cluster, the registry, the Git server, and Argo CD;
//! - [`fixture`]: the project, rendered per variant from one template;
//! - [`nyl`], [`checks`]: running `nyl` with only the admitted environment, and
//!   the decisions over what it and the cluster report;
//! - [`reconciler`]: what differs between variants;
//! - [`scenario`], [`runner`], [`diag`], [`cli`]: the steps, running variants,
//!   diagnostics on failure, and the command line.

pub mod checks;
pub mod cli;
pub mod cmd;
pub mod diag;
pub mod docker;
pub mod env;
pub mod error;
pub mod fixture;
pub mod infra;
pub mod kubectl;
pub mod log;
pub mod nyl;
pub mod prereq;
pub mod project;
pub mod reconciler;
pub mod runner;
pub mod scenario;
pub mod variant;
pub mod wait;
