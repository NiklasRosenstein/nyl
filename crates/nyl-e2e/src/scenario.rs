//! The scenario, run once per variant: build, apply, publish, deploy, attest,
//! observe, change an input, do it again, tear down. Every step is a command
//! a user or a CI job would run, and every assertion is on something visible
//! from outside: the registry, the published branch, the cluster, `nyl`'s
//! reports, and the state refs.

use std::time::{Duration, Instant};

use crate::checks::{attestation_ready, deployment_ready, observations_ready, Observed};
use crate::ensure;
use crate::env::Env;
use crate::error::{Context, Result};
use crate::fixture::{
    self, expected_database_host, Declarations, Params, ENVIRONMENT, NAMESPACE, PUBLICATION_BRANCH, PUBLICATION_PREFIX,
    UNIT_DATABASE, UNIT_IMAGE, UNIT_PUBLICATION,
};
use crate::infra::kind::KUBE_VERSION;
use crate::infra::{registry, KUBE_CONTEXT};
use crate::log;
use crate::reconciler::Reconciler;
use crate::wait::{Poll, Waiter};

/// What one revision of the project produced, as `nyl get` reports it.
#[derive(Clone, Debug)]
struct Revision {
    number: u32,
    /// The image reference with its digest, `localhost:<port>/web@sha256:...`.
    image: String,
    database_host: String,
}

struct Scenario<'a> {
    env: &'a Env,
    reconciler: &'a dyn Reconciler,
    declarations: Declarations,
}

/// Run the whole scenario against `env`.
pub fn run(env: &Env, reconciler: &dyn Reconciler) -> Result<()> {
    let scenario = Scenario {
        env,
        reconciler,
        declarations: reconciler.declarations(&env.infra)?,
    };
    scenario.run()
}

/// Run one step in a folded section of the log, naming it in any error.
fn step<T>(title: &str, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let group = log::Group::open(title);
    let started = Instant::now();
    let result = body();
    drop(group);
    match &result {
        Ok(_) => log::line(format!("ok ({:.0}s): {title}", started.elapsed().as_secs_f64())),
        Err(_) => log::line(format!("FAILED: {title}")),
    }
    result.map_err(|error| error.context(format!("step \"{title}\"")))
}

impl Scenario<'_> {
    fn run(&self) -> Result<()> {
        let nyl = &self.env.nyl;
        let name = self.reconciler.variant().name();

        step("commit revision 1 of the project", || self.publish_revision(1))?;
        step("nyl validate", || nyl.command(&["validate"]).run_ok().map(|_| ()))?;
        step("nyl state init", || {
            nyl.command(&["state", "init", "-e", ENVIRONMENT]).run_ok().map(|_| ())
        })?;

        let first = step(
            "nyl reconcile builds the image, applies OpenTofu, and publishes",
            || self.reconcile("executed", 1),
        )?;
        step("the image is in the registry and the tree is on the branch", || {
            self.check_published(&first)
        })?;
        step(&format!("{name}: run the published tree"), || {
            self.reconciler.deploy(self.env)
        })?;
        step("the workload runs revision 1", || self.await_workload(&first))?;
        step("nyl attest records accepted and healthy", || self.attest())?;
        step("nyl observe reads the live values", || self.observe(&first))?;

        step(
            "commit revision 2: another page in the image and another OpenTofu output",
            || self.publish_revision(2),
        )?;
        let second = step("nyl reconcile executes all three units again", || {
            self.reconcile("executed", 2)
        })?;
        ensure!(
            second.image != first.image,
            "revision 2 built the same image as revision 1: {}",
            first.image
        );
        ensure!(
            second.database_host != first.database_host,
            "revision 2 has the same database host as revision 1: {}",
            first.database_host
        );
        step("the image is in the registry and the tree is on the branch", || {
            self.check_published(&second)
        })?;
        step(&format!("{name}: run the new tree"), || {
            self.reconciler.deploy(self.env)
        })?;
        step("the workload runs revision 2", || self.await_workload(&second))?;
        step("nyl attest records accepted and healthy for revision 2", || {
            self.attest()
        })?;
        step("nyl observe reads the live values of revision 2", || {
            self.observe(&second)
        })?;

        step("a repeated reconcile executes nothing", || {
            self.reconcile("current", 2).map(|_| ())
        })?;

        step("nyl teardown removes everything, waiting for the reconciler", || {
            self.teardown()
        })?;
        step(&format!("{name}: it removed the workload"), || {
            self.reconciler.after_teardown(self.env)
        })?;
        step("the cluster, the branch, and the state show the removal", || {
            self.check_removed()
        })?;
        Ok(())
    }

    fn params(&self, revision: u32) -> Params {
        Params {
            registry_host: self.env.infra.registry_host(),
            repo_url: self.env.infra.git_repo_url(),
            publish_url: self.env.infra.git_publish_url(),
            state_dir: self.env.state_dir.clone(),
            kube_context: KUBE_CONTEXT.to_owned(),
            kube_version: KUBE_VERSION.to_owned(),
            revision,
            docker_env: self.env.docker_env.clone(),
        }
    }

    /// Render revision `number` of the project, commit it, and push it.
    fn publish_revision(&self, number: u32) -> Result<()> {
        let files = fixture::render(&self.params(number), &self.declarations)?;
        self.env.project.write(&files)?;
        let commit = self
            .env
            .project
            .commit_and_push(&format!("Revision {number} of the e2e project"))?;
        log::line(format!("revision {number} is commit {commit}"));
        Ok(())
    }

    /// `nyl reconcile` and check that all three units have `result`; returns
    /// what the revision produced.
    fn reconcile(&self, result: &str, number: u32) -> Result<Revision> {
        let report = self.env.nyl.report(&["reconcile", "-e", ENVIRONMENT], &[0])?;
        for unit in [UNIT_DATABASE, UNIT_IMAGE, UNIT_PUBLICATION] {
            let actual = report.result(unit)?;
            ensure!(
                actual == result,
                "unit {unit} is {actual:?} after `nyl reconcile`, expected {result:?}; the report is\n{}",
                serde_json::to_string_pretty(&report.json).unwrap_or_default()
            );
        }
        self.read_revision(number)
    }

    fn read_revision(&self, number: u32) -> Result<Revision> {
        let nyl = &self.env.nyl;
        let image = nyl.text(&[
            "get",
            "artifact",
            &format!("{UNIT_IMAGE}/image"),
            "-e",
            ENVIRONMENT,
            "--pointer",
            "/reference",
        ])?;
        let database_host = nyl.text(&[
            "get",
            "output",
            &format!("{UNIT_DATABASE}/connection"),
            "-e",
            ENVIRONMENT,
            "--pointer",
            "/host",
        ])?;
        let prefix = format!("{}/web@sha256:", self.env.infra.registry_host());
        ensure!(
            image.starts_with(&prefix),
            "the image reference {image:?} is not a digest in the local registry (expected prefix {prefix:?})"
        );
        ensure!(
            database_host == expected_database_host(number),
            "OpenTofu output host {database_host:?}, expected {:?}",
            expected_database_host(number)
        );
        log::line(format!(
            "revision {number}: image {image}, database host {database_host}"
        ));
        Ok(Revision {
            number,
            image,
            database_host,
        })
    }

    /// The registry holds the image's digest, and the publication branch holds
    /// the tree, the catalog, and the ownership index.
    fn check_published(&self, revision: &Revision) -> Result<()> {
        let digest = revision
            .image
            .rsplit_once('@')
            .map(|(_, digest)| digest)
            .context(format!("the image reference {:?} has no digest", revision.image))?;
        ensure!(
            registry::has_manifest(self.env.infra.registry.host_port, "web", digest)?,
            "the registry does not hold web@{digest}"
        );
        self.env.project.fetch()?;
        let tree = self.env.project.tree(PUBLICATION_BRANCH)?;
        for required in [
            format!("{PUBLICATION_PREFIX}/_nyl/index.json"),
            fixture::CATALOG_MANIFEST.to_owned(),
        ] {
            ensure!(
                tree.contains(&required),
                "the {PUBLICATION_BRANCH} branch lacks {required}; it holds {tree:?}"
            );
        }
        ensure!(
            tree.iter()
                .any(|path| path.starts_with(&format!("{PUBLICATION_PREFIX}/web/"))),
            "the {PUBLICATION_BRANCH} branch holds no rendered Release under {PUBLICATION_PREFIX}/web/: {tree:?}"
        );
        Ok(())
    }

    /// Wait until the Deployment runs the revision's image and host, with every
    /// replica ready.
    fn await_workload(&self, revision: &Revision) -> Result<()> {
        let kubectl = &self.env.kubectl;
        Waiter::new(
            format!(
                "the Deployment to run revision {} ({})",
                revision.number, revision.image
            ),
            Duration::from_secs(10 * 60),
        )
        .backoff(Duration::from_secs(2), Duration::from_secs(10))
        .until(|| {
            let Some(deployment) =
                kubectl.get_optional(&["deployment", fixture::DEPLOYMENT, "--namespace", NAMESPACE])?
            else {
                return Ok(Poll::pending("the Deployment does not exist yet"));
            };
            Ok(deployment_ready(&deployment, &revision.image, &revision.database_host))
        })
    }

    /// Poll `nyl attest` until `accepted` and `healthy` pass: a reconcile
    /// attests nothing, and the reconciler may need a moment to report health.
    fn attest(&self) -> Result<()> {
        Waiter::new("nyl attest to record accepted and healthy", Duration::from_secs(6 * 60))
            .backoff(Duration::from_secs(5), Duration::from_secs(15))
            .until(|| {
                let report = self.env.nyl.report(&["attest", "-e", ENVIRONMENT], &[0, 2, 3])?;
                Ok(match attestation_ready(&report.json, UNIT_PUBLICATION) {
                    Poll::Ready(()) => Poll::Ready(()),
                    Poll::Pending(reason) => Poll::Pending(format!("exit {}: {reason}", report.exit)),
                })
            })
    }

    /// Poll `nyl observe` until both live values arrive, then check them
    /// against the cluster and against what `nyl get observation` prints.
    fn observe(&self, revision: &Revision) -> Result<()> {
        let observed: Observed = Waiter::new("nyl observe to read the live values", Duration::from_secs(6 * 60))
            .backoff(Duration::from_secs(5), Duration::from_secs(15))
            .until(|| {
                let report = self.env.nyl.report(&["observe", "-e", ENVIRONMENT], &[0, 2])?;
                Ok(match observations_ready(&report.json, UNIT_PUBLICATION) {
                    Poll::Ready(observed) => Poll::Ready(observed),
                    Poll::Pending(reason) => Poll::Pending(format!("exit {}: {reason}", report.exit)),
                })
            })?;
        ensure!(
            observed.ready_replicas == 1,
            "revision {} observed {} ready replicas, expected 1",
            revision.number,
            observed.ready_replicas
        );
        let service = self.env.kubectl.run(&[
            "get",
            "service",
            fixture::SERVICE,
            "--namespace",
            NAMESPACE,
            "--output",
            "jsonpath={.spec.clusterIP}",
        ])?;
        ensure!(
            service.stdout_trimmed() == observed.service_cluster_ip,
            "the Service's cluster IP is {:?} in the cluster but nyl observed {:?}",
            service.stdout_trimmed(),
            observed.service_cluster_ip
        );
        for (name, expected) in [
            ("readyReplicas", observed.ready_replicas.to_string()),
            ("serviceClusterIP", observed.service_cluster_ip.clone()),
        ] {
            let recorded = self.env.nyl.text(&[
                "get",
                "observation",
                &format!("{UNIT_PUBLICATION}/{name}"),
                "-e",
                ENVIRONMENT,
            ])?;
            ensure!(
                recorded.trim_matches('"') == expected,
                "`nyl get observation {UNIT_PUBLICATION}/{name}` prints {recorded:?}, expected {expected:?}"
            );
        }
        Ok(())
    }

    /// Tear the whole environment down; the publication waits, reading the
    /// reconciler, until the workloads are gone.
    fn teardown(&self) -> Result<()> {
        // The publication's own timeout (6m) bounds the wait; this bounds
        // anything that hangs beyond it.
        let report = self.env.nyl.report_within(
            &["teardown", "-e", ENVIRONMENT, "--all", "--reason", "nyl-e2e teardown"],
            &[0],
            Duration::from_secs(9 * 60),
        )?;
        for (unit, accepted) in [
            (UNIT_PUBLICATION, &["removed"][..]),
            (UNIT_DATABASE, &["removed"]),
            // An image cannot be deleted from a registry; its unit is released.
            (UNIT_IMAGE, &["released", "removed"]),
        ] {
            let result = report.result(unit)?;
            ensure!(
                accepted.contains(&result),
                "unit {unit} is {result:?} after `nyl teardown`, expected one of {accepted:?}"
            );
        }
        Ok(())
    }

    /// The workload is gone from the cluster, the publication branch holds no
    /// tree for the target, and the state holds the publication without an
    /// incarnation.
    fn check_removed(&self) -> Result<()> {
        let kubectl = &self.env.kubectl;
        Waiter::new(
            format!("the Namespace {NAMESPACE} to be gone"),
            Duration::from_secs(5 * 60),
        )
        .backoff(Duration::from_secs(2), Duration::from_secs(10))
        .until(|| {
            Ok(match kubectl.get_optional(&["namespace", NAMESPACE])? {
                None => Poll::Ready(()),
                Some(_) => Poll::pending("the Namespace still exists"),
            })
        })?;
        self.env.project.fetch()?;
        let tree = self.env.project.tree(PUBLICATION_BRANCH)?;
        let left = tree
            .iter()
            .filter(|path| path.starts_with(&format!("{PUBLICATION_PREFIX}/")))
            .collect::<Vec<_>>();
        ensure!(
            left.is_empty(),
            "the {PUBLICATION_BRANCH} branch still holds {left:?} after the teardown"
        );
        let state = self.env.project.show(
            &format!("nyl/{ENVIRONMENT}/desired"),
            &format!("desired/units/{UNIT_PUBLICATION}.yaml"),
        )?;
        ensure!(
            state.contains("state: held"),
            "the desired state of {UNIT_PUBLICATION} is not held after `nyl teardown --all`:\n{state}"
        );
        Ok(())
    }
}
