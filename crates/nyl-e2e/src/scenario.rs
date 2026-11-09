//! The scenario, run once per variant: build, apply, publish, deploy, attest,
//! observe, change an input, do it again, tear down. Every step is a command
//! a user or a CI job would run, and every assertion is on something visible
//! from outside: the registry, the published branch, the cluster, `nyl`'s
//! reports, and the state refs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::checks::{
    attestation_ready, deployment_ready, hook_ran_after_workload, observations_ready, parse_attestation_record,
    Observed,
};
use crate::env::Env;
use crate::error::{Context, Error, Result};
use crate::fixture::{
    self, expected_database_host, Declarations, Params, ENVIRONMENT, NAMESPACE, PUBLICATION_BRANCH, PUBLICATION_PREFIX,
    UNIT_DATABASE, UNIT_IMAGE, UNIT_PUBLICATION,
};
use crate::infra::kind::KUBE_VERSION;
use crate::infra::{registry, KUBE_CONTEXT};
use crate::log;
use crate::nyl::Report;
use crate::reconciler::Reconciler;
use crate::wait::{Poll, Waiter};
use crate::{bail, ensure};

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
            &format!("nyl reconcile builds, applies, publishes, and waits for {name} to report healthy"),
            || self.reconcile_and_deploy("executed", 1),
        )?;
        step("the image is in the registry and the tree is on the branch", || {
            self.check_published(&first)
        })?;
        step("the workload runs revision 1", || self.await_workload(&first))?;
        let first_hook = step("the PostSync hook ran after the workload was created", || {
            self.await_hook(None)
        })?;
        step("reconcile recorded accepted and healthy, and nyl attest agrees", || {
            self.attest()
        })?;
        step("nyl observe reads the live values", || self.observe(&first))?;

        step(
            "commit revision 2: another page in the image and another OpenTofu output",
            || self.publish_revision(2),
        )?;
        let second = step(
            &format!("nyl reconcile executes all three units again and waits for {name}"),
            || self.reconcile_and_deploy("executed", 2),
        )?;
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
        step("the workload runs revision 2", || self.await_workload(&second))?;
        step("the PostSync hook ran again, as a new Job", || {
            self.await_hook(Some(&first_hook)).map(|_| ())
        })?;
        step(
            "reconcile recorded accepted and healthy for revision 2, and nyl attest agrees",
            || self.attest(),
        )?;
        step("nyl observe reads the live values of revision 2", || {
            self.observe(&second)
        })?;

        step("a repeated reconcile executes nothing", || {
            self.reconcile_report("current")?;
            Ok(())
        })?;

        // Nothing but Nyl keeps a direct target's cluster in line with its
        // tree; with Argo CD, Argo CD does.
        if !self.reconciler.needs_operator() {
            step(
                "a change made behind Nyl's back is drift, and a reconcile repairs it",
                || self.drift(),
            )?;
        }

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

    /// `nyl reconcile`, which with `observe.waitHealthy` returns once the
    /// reconciler reports the workload healthy. When an operator has to make
    /// the cluster run the tree meanwhile (Argo CD's workload sync is manual),
    /// the reconcile runs on a thread of its own while this one waits for the
    /// publication to land and then deploys it. Returns what the revision
    /// produced.
    fn reconcile_and_deploy(&self, result: &str, number: u32) -> Result<Revision> {
        if !self.reconciler.needs_operator() {
            self.reconcile_report(result)?;
            return self.read_revision(number);
        }
        let before = self.env.project.remote_head(PUBLICATION_BRANCH)?;
        let stop = AtomicBool::new(false);
        let (reconciled, deployed) = std::thread::scope(|scope| {
            let reconcile = scope.spawn(|| {
                let reconciled = self.reconcile_report(result);
                if reconciled.is_err() {
                    stop.store(true, Ordering::SeqCst);
                }
                reconciled
            });
            let deployed = self
                .await_publication(before.as_deref(), &stop)
                .and_then(|()| self.reconciler.deploy(self.env, &stop));
            let reconciled = reconcile
                .join()
                .unwrap_or_else(|_| Err(Error::new("the reconcile thread panicked")));
            (reconciled, deployed)
        });
        // The reconcile's own failure explains more than a deploy that gave up.
        reconciled?;
        deployed?;
        self.read_revision(number)
    }

    /// Wait until the publication branch moves past `before`: the reconcile
    /// has pushed the tree.
    fn await_publication(&self, before: Option<&str>, stop: &AtomicBool) -> Result<()> {
        Waiter::new(
            format!("nyl reconcile to publish to the {PUBLICATION_BRANCH} branch"),
            Duration::from_secs(10 * 60),
        )
        .backoff(Duration::from_secs(2), Duration::from_secs(5))
        .until(|| {
            if stop.load(Ordering::SeqCst) {
                return Err(Error::new("the reconcile failed before it published"));
            }
            Ok(match self.env.project.remote_head(PUBLICATION_BRANCH)? {
                Some(head) if Some(head.as_str()) != before => Poll::Ready(()),
                _ => Poll::pending("the branch has not moved yet"),
            })
        })
    }

    /// `nyl reconcile` and check that all three units have `result`.
    fn reconcile_report(&self, result: &str) -> Result<Report> {
        let report = self.env.nyl.report(&["reconcile", "-e", ENVIRONMENT], &[0])?;
        for unit in [UNIT_DATABASE, UNIT_IMAGE, UNIT_PUBLICATION] {
            let actual = report.result(unit)?;
            ensure!(
                actual == result,
                "unit {unit} is {actual:?} after `nyl reconcile`, expected {result:?}; the report is\n{}",
                serde_json::to_string_pretty(&report.json).unwrap_or_default()
            );
        }
        Ok(report)
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
        let mut required_files = vec![format!("{PUBLICATION_PREFIX}/_nyl/index.json")];
        required_files.extend(self.reconciler.published_files());
        for required in required_files {
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

    /// Wait until the Release's `PostSync` hook Job succeeded after the Deployment
    /// was created, and, with `previous`, is another object than the Job that
    /// ran for the revision before: a sync runs its hooks, whichever reconciler
    /// runs it, and the default delete policy replaces the old Job with a new one.
    /// Returns the Job's UID.
    fn await_hook(&self, previous: Option<&str>) -> Result<String> {
        let kubectl = &self.env.kubectl;
        Waiter::new(
            format!("the {} hook Job to have run", fixture::HOOK_JOB),
            Duration::from_secs(5 * 60),
        )
        .backoff(Duration::from_secs(2), Duration::from_secs(10))
        .until(|| {
            let Some(job) = kubectl.get_optional(&["job", fixture::HOOK_JOB, "--namespace", NAMESPACE])? else {
                return Ok(Poll::pending("the hook Job does not exist yet"));
            };
            let Some(deployment) =
                kubectl.get_optional(&["deployment", fixture::DEPLOYMENT, "--namespace", NAMESPACE])?
            else {
                return Ok(Poll::pending("the Deployment does not exist"));
            };
            Ok(match hook_ran_after_workload(&job, &deployment) {
                Poll::Ready(uid) if Some(uid.as_str()) == previous => {
                    Poll::pending("the hook Job is still the one of the revision before")
                }
                other => other,
            })
        })
    }

    /// The reconcile's wait recorded `accepted` and `healthy` as passing, and a
    /// single `nyl attest` agrees. The recorded attestations are read from the
    /// observed state before `nyl attest` runs, because both write the record.
    fn attest(&self) -> Result<()> {
        self.env.project.fetch()?;
        let record = self
            .env
            .project
            .show(
                &format!("nyl/{ENVIRONMENT}/observed"),
                &format!("observed/units/{UNIT_PUBLICATION}/attestations.yaml"),
            )
            .map_err(|error| error.context("the reconcile recorded no attestations"))?;
        let recorded = parse_attestation_record(&record);
        for name in ["accepted", "healthy"] {
            let attestation = recorded.get(name);
            ensure!(
                attestation.is_some_and(|attestation| attestation.result == "pass"),
                "attestation {name} recorded by the reconcile is {attestation:?}; the record is\n{record}"
            );
            ensure!(
                attestation.is_some_and(|attestation| attestation.driver == "reconcile"),
                "attestation {name} was recorded by {:?}, not by the reconcile's wait; the record is\n{record}",
                attestation.map(|attestation| attestation.driver.as_str())
            );
        }
        let report = self.env.nyl.report(&["attest", "-e", ENVIRONMENT], &[0])?;
        match attestation_ready(&report.json, UNIT_PUBLICATION) {
            Poll::Ready(()) => Ok(()),
            Poll::Pending(reason) => bail!("nyl attest disagrees with the reconcile: {reason}"),
        }
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

    /// `nyl detect-drift` finds the publication clean, then drifted after a
    /// `kubectl scale` of the workload, naming the Deployment; the next
    /// reconcile executes the publication again and the Deployment is back to
    /// the replicas it applied.
    fn drift(&self) -> Result<()> {
        let nyl = &self.env.nyl;
        let kubectl = &self.env.kubectl;
        let check = |expected: &str, exits: &[i32]| -> Result<String> {
            let report = nyl.report(&["detect-drift", "-e", ENVIRONMENT, "--unit", UNIT_PUBLICATION], exits)?;
            let entry = report.unit(UNIT_PUBLICATION)?;
            let actual = entry
                .get("check")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(none)");
            ensure!(
                actual == expected,
                "nyl detect-drift reports {UNIT_PUBLICATION} {actual:?}, expected {expected:?}; the report is\n{}",
                serde_json::to_string_pretty(&report.json).unwrap_or_default()
            );
            Ok(entry
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned())
        };
        check("clean", &[0])?;
        kubectl.run(&[
            "scale",
            "deployment",
            fixture::DEPLOYMENT,
            "--namespace",
            NAMESPACE,
            "--replicas=3",
        ])?;
        let summary = check("drifted", &[3])?;
        ensure!(
            summary.contains(&format!("Deployment {NAMESPACE}/{}", fixture::DEPLOYMENT)),
            "the drift summary does not name the Deployment: {summary}"
        );
        let report = nyl.report(&["reconcile", "-e", ENVIRONMENT], &[0])?;
        let result = report.result(UNIT_PUBLICATION)?;
        ensure!(
            result == "executed",
            "the reconcile did not repair the drifted {UNIT_PUBLICATION}: its result is {result:?}"
        );
        Waiter::new(
            "the Deployment to have the replicas Nyl applied again",
            Duration::from_secs(2 * 60),
        )
        .backoff(Duration::from_secs(2), Duration::from_secs(10))
        .until(|| {
            let deployment = kubectl.json(&["get", "deployment", fixture::DEPLOYMENT, "--namespace", NAMESPACE])?;
            let replicas = deployment.pointer("/spec/replicas").and_then(serde_json::Value::as_i64);
            Ok(if replicas == Some(1) {
                Poll::Ready(())
            } else {
                Poll::pending(format!("the Deployment has {replicas:?} replicas"))
            })
        })?;
        check("clean", &[0])?;
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
