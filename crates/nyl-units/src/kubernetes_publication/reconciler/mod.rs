//! Reading the reconciler that runs a published target: the one
//! driver-owned seam between the publication driver and what the reconciler
//! reports, and the neutral facts a reading consists of.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! (observer, reconciler readers) and [Credentials](../../../../../design/orchestration-core.md#credentials).
//!
//! A [`ReconcilerReader`] answers one question: for the deployments a target
//! expects (the [`ExpectedDeployment`]s recorded at resolution under
//! `/reconciler`), what does the reconciler report? The answer is a
//! [`Reading`], which holds no reconciler's types or field names:
//!
//! - per expected deployment that exists, an [`Observed`] one: the revision it
//!   [`Running`]-ly holds ([`Running::Exact`], [`Running::Mixed`], or
//!   [`Running::None`]), its [`Health`], and the [`Source`] it takes its
//!   manifests from;
//! - per anchor, whether it is visible; and, for an expected deployment the
//!   reading lacks, a [`Visibility`] answer saying whether the credential
//!   could have seen it, so that its absence means something.
//!
//! Everything after the reading, the match of the running revision to a
//! publication, health, the attestations, and the answer to a teardown wait,
//! is [`evaluate`]'s: one set of rules over neutral types, written and tested
//! once whichever reconciler was read. What is specific to a reconciler kind
//! stays in its module:
//!
//! - [`argocd`](super::argocd) maps Argo CD into a [`Reading`], through the
//!   Kubernetes API of the instance's control-plane Cluster or its API server.
//! - Its renderer, in [`nyl_render::gitops::observation`], derives the
//!   expected deployments and anchors from what the target generates.
//!
//! A second reconciler kind adds a variant of
//! [`ReconcilerLocation`](nyl_render::gitops::observation::ReconcilerLocation)
//! and a renderer for its expected deployments next to Argo CD's, a reader
//! here that implements [`ReconcilerReader`] and the match arm of [`reader`]
//! that builds it. [`evaluate`], the driver's observation, and the engine do
//! not change.
//!
//! Credentials come only from the unit's admitted environment: a missing one
//! is a [`ReadErrorKind::Credentials`] error that names the variable and the
//! `env` entry that admits it, never a fallback to the process environment,
//! the current kube context, or `~/.kube/config`.

use std::collections::BTreeMap;
use std::time::Duration;

use nyl_render::gitops::observation::{Anchor, ExpectedDeployment, ReconcilerLocation, Source};

pub mod evaluate;

/// Why a reconciler could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// A credential the declaration needs is not admitted to the unit.
    Credentials,
    /// The declaration cannot be read as it stands, such as a Cluster
    /// without the local context the Kubernetes method needs.
    Declaration,
    /// The reconciler or its API did not answer: connection, timeout, server error.
    Unreachable,
    /// The credential was rejected (HTTP 401): invalid, or expired.
    Rejected,
    /// The credential is accepted but may not read (HTTP 403).
    Forbidden,
    /// The answer is not the one it should be.
    Malformed,
}

/// A failed reading, with the fix attached to the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub kind: ReadErrorKind,
    pub message: String,
}

impl ReadError {
    pub fn new(kind: ReadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReadError {}

/// The revision a deployment runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Running {
    /// One revision: the commit of the publication branch it was last fully synced to.
    Exact(String),
    /// A selective, dry-run, or unfinished operation newer than the last
    /// full one leaves a mix of revisions, which proves no revision. `last`
    /// is the last fully synced commit, when there is one, and `reason`
    /// says why it proves nothing and what to do.
    Mixed { last: Option<String>, reason: String },
    /// Nothing has completed: it runs nothing yet.
    None,
    /// The reconciler's answer names no revision Nyl can read, with why.
    Unreadable(String),
}

/// The health a reconciler reports for a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Progressing,
    Degraded,
    /// Anything else, and nothing reported: never a pass.
    Unknown,
}

impl Health {
    /// The name in findings and attestation details.
    pub fn name(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Progressing => "Progressing",
            Self::Degraded => "Degraded",
            Self::Unknown => "Unknown",
        }
    }
}

/// One expected deployment the reconciler has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub running: Running,
    pub health: Health,
    /// The reconciler is removing it.
    pub deleting: bool,
    /// The source it is configured with now.
    pub source: Option<Source>,
    /// The source its last full sync took, when the reconciler records one.
    pub synced_source: Option<Source>,
}

/// Whether the credential could have seen something that a reading lacks or
/// that anchors it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    Visible,
    /// It could not be seen, or that cannot be told: what an absence would
    /// prove nothing about, with the fix.
    Hidden(String),
}

/// What proves that a deployment missing from a reading is really missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// The reader answers for each deployment whether the credential could
    /// have seen it, because its listing silently leaves out what the
    /// credential may not read.
    PerDeployment,
    /// The reader's listing fails instead of filtering, so a missing
    /// deployment means something only when a reading shows what only the
    /// reconciler that runs the target has (its anchors).
    Anchors,
}

/// What one reading of a reconciler holds, over the deployments and anchors
/// a [`ReadRequest`] asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// The expected deployments the reconciler has, by id.
    pub deployments: BTreeMap<String, Observed>,
    /// Each anchor the reader established, by id.
    pub anchors: BTreeMap<String, Visibility>,
    /// For each requested deployment the reconciler lacks, by id.
    pub absent: BTreeMap<String, Visibility>,
    pub proof: Proof,
    /// Set when the reader can prove nothing about the target at all, such
    /// as a Kubernetes context with no anchor to show it reaches the
    /// reconciler that runs the target: why, with the fix.
    pub unprovable: Option<String>,
}

/// What a reader is asked.
#[derive(Debug, Clone, Copy)]
pub struct ReadRequest<'a> {
    pub deployments: &'a [ExpectedDeployment],
    pub anchors: &'a [Anchor],
    /// The caller concludes from absence (a teardown wait), so the reader
    /// establishes the anchors even where its own proof does not rest on them.
    pub concluding_absence: bool,
}

/// Reads one reconciler.
pub trait ReconcilerReader {
    /// Read what the request names. An error is a reading that could not be
    /// made, never a partial one.
    fn read(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError>;
}

/// The reader the recorded location asks for, built from the unit's admitted
/// environment, finishing each reading within `budget`.
pub fn reader(
    location: &ReconcilerLocation,
    env: &BTreeMap<String, String>,
    budget: Duration,
) -> Result<Box<dyn ReconcilerReader>, ReadError> {
    match location {
        ReconcilerLocation::ArgoCd(location) => super::argocd::reader(location, env, budget),
    }
}

/// The admitted variable naming the kubeconfig file(s) the Kubernetes method reads.
pub const KUBECONFIG_ENV: &str = "KUBECONFIG";

/// Run `future` on a thread with a current-thread runtime of its own, so a
/// driver calls a reader from synchronous code and from inside an
/// asynchronous runtime alike.
pub(crate) fn block_on<T: Send>(future: impl std::future::Future<Output = T> + Send) -> Result<T, ReadError> {
    // Every client builds a TLS configuration, which needs a process-level
    // crypto provider; the binary installs one at start, and a library caller
    // may not have. Installing is idempotent: it fails only when one exists.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|runtime| runtime.block_on(future))
                    .map_err(|error| {
                        ReadError::new(
                            ReadErrorKind::Unreachable,
                            format!("cannot start a runtime to read the reconciler: {error}"),
                        )
                    })
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
