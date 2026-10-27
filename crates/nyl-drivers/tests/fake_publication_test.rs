//! `FakePublication` against a test world built from the orchestration core
//! walkthrough project: a bare `project.git` that is both the source and the
//! publication repository, and a working clone at its first commit.
//!
//! Contract: [Fake kinds](../../../design/reference-scenarios.md#fake-kinds)
//! and the [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit).
//! Git runs as the `git` command line with its identity passed per
//! invocation, never through the process environment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use nyl_core::orchestration::driver::{
    Change, Driver, DriverOperation, DriverResult, ExecutionContext, Outcome, ResolutionContext, Supported,
    TeardownSettings, ToolExit, ToolInvocation, ToolRunner, WaitCompletion,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, RunId, Uid, UnitAddress, UnitName};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{
    Deletion, DeletionIntent, DeletionReason, DesiredUnit, DesiredUnitKind, Lifecycle, ObservedUnit, ObservedUnitKind,
    Readiness, StateV1, TeardownProgress, TeardownWaiting, UnitIdentity,
};
use nyl_drivers::fake::{FakePublication, Script};
use serde_json::{json, Value};
use tempfile::TempDir;

struct NoTools;

impl ToolRunner for NoTools {
    fn run(&self, _invocation: ToolInvocation) -> DriverResult<ToolExit> {
        panic!("FakePublication runs no tools")
    }
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .args(["-c", "user.name=Scenario", "-c", "user.email=scenario@example.invalid"])
        .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct World {
    root: TempDir,
}

impl World {
    /// The walkthrough project committed as the first commit of `main`.
    fn new() -> Self {
        let world = Self {
            root: TempDir::new().unwrap(),
        };
        std::fs::create_dir_all(world.root.path().join("scenario-tmp")).unwrap();
        git(
            world.root.path(),
            &["init", "--quiet", "--bare", "--initial-branch=main", "project.git"],
        );
        git(world.root.path(), &["clone", "--quiet", &world.project_url(), "work"]);
        let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nyl/tests/scenarios/walkthroughs/_project");
        world.copy_into_work(&project);
        git(&world.work(), &["add", "--all"]);
        git(&world.work(), &["commit", "--quiet", "--message", "Initial project"]);
        git(&world.work(), &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        world
    }

    fn project_url(&self) -> String {
        format!("file://{}", self.root.path().join("project.git").display())
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn bare(&self) -> PathBuf {
        self.root.path().join("project.git")
    }

    fn copy_into_work(&self, source: &Path) {
        for entry in walkdir::WalkDir::new(source).sort_by_file_name() {
            let entry = entry.unwrap();
            let target = self.work().join(entry.path().strip_prefix(source).unwrap());
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let text = text
                .replace(
                    "%SCENARIO_TMP%",
                    &self.root.path().join("scenario-tmp").display().to_string(),
                )
                .replace("%PROJECT_URL%", &self.project_url())
                .replace("%REGISTRY%", "registry.invalid:5000");
            std::fs::write(target, text).unwrap();
        }
    }

    fn files(&self, branch: &str) -> Vec<String> {
        git(
            &self.bare(),
            &["ls-tree", "-r", "--name-only", &format!("refs/heads/{branch}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }
}

fn spec() -> Value {
    json!({
        "target": "dev",
        "mode": "publish",
        "releases": {
            "web/web": {
                "image": "registry.invalid:5000/web@sha256:0123",
                "databaseHost": "db.vpc-walkthrough.internal",
            }
        }
    })
}

fn desired(spec: Value, key: ExecutionKey) -> DesiredUnit {
    DesiredUnit {
        api_version: StateV1,
        kind: DesiredUnitKind,
        unit: UnitIdentity {
            environment: EnvironmentName::new("dev").unwrap(),
            name: UnitName::new("kubernetes").unwrap(),
            uid: Some(Uid::new("00000000-0000-7000-8000-000000000004").unwrap()),
            api_version: "units.gitops.nyl/v1".to_owned(),
            kind: "KubernetesPublication".to_owned(),
        },
        lifecycle: Lifecycle::Active,
        source_commit: Some(CommitId::new("a".repeat(40)).unwrap()),
        source_revision: Some(CommitId::new("a".repeat(40)).unwrap()),
        spec: spec.clone(),
        resolved_spec: Some(spec),
        provenance: BTreeMap::new(),
        execution_key: Some(key),
        fingerprint: None,
        attestations: Vec::new(),
        readiness: Readiness::Ready,
    }
}

fn context<'a>(
    world: &World,
    operation: DriverOperation,
    identity: Option<&'a CommitIdentity>,
) -> ExecutionContext<'a> {
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    ExecutionContext {
        unit: "dev/kubernetes".parse::<UnitAddress>().unwrap(),
        operation,
        worktree: world.work(),
        project_dir: world.work().join("nyl"),
        temp_dir: world.root.path().join("scenario-tmp"),
        env: BTreeMap::new(),
        approved_digest: None,
        deadline: now,
        now,
        observed: None,
        tools: &NoTools,
        commit_identity: identity,
        cache_dir: world.root.path().join("cache"),
        teardown_wait: None,
    }
}

fn key(driver: &FakePublication, world: &World) -> ExecutionKey {
    let inputs = driver
        .execution_key_inputs(
            &ResolutionContext {
                environment: EnvironmentName::new("dev").unwrap(),
                worktree: world.work(),
                project_dir: world.work().join("nyl"),
                env: BTreeMap::new(),
                deadline: chrono::Utc::now(),
                now: chrono::Utc::now(),
                tools: &NoTools,
                cache_dir: world.root.path().join("cache"),
            },
            &spec(),
        )
        .unwrap();
    assert!(
        inputs.files.contains_key("@resource:DeploymentTarget/dev"),
        "{:?}",
        inputs.files
    );
    assert!(inputs.files.contains_key("/applications/web/release.yaml"));
    ExecutionKey::from_digest_of(serde_json::to_string(&inputs.files).unwrap().as_bytes())
}

#[test]
fn test_fake_publication_publishes_the_target_tree_and_records_the_tree_artifact() {
    let world = World::new();
    let driver = FakePublication::new(Script::new());
    let unit = desired(spec(), key(&driver, &world));
    let identity: CommitIdentity = "Nyl Orchestration <nyl@example.invalid>".parse().unwrap();

    let Supported::Supported(plan) = driver
        .plan(&context(&world, DriverOperation::Plan, None), &unit)
        .unwrap()
    else {
        panic!("FakePublication plans");
    };
    assert_eq!(plan.changes, Change::Create);

    let Supported::Supported(Outcome::Succeeded { artifacts, .. }) = driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&identity)), &unit)
        .unwrap()
    else {
        panic!("the publication succeeds");
    };

    let files = world.files("deploy");
    assert!(files.contains(&"dev/_nyl/index.json".to_owned()), "{files:?}");
    assert!(files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
    let deployment = files
        .iter()
        .filter(|path| path.starts_with("dev/web/"))
        .map(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]))
        .find(|text| text.contains("kind: Deployment"))
        .expect("the Deployment is published");
    assert!(deployment.contains("registry.invalid:5000/web@sha256:0123"));
    assert!(deployment.contains("db.vpc-walkthrough.internal"));

    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    assert_eq!(
        git(
            &world.bare(),
            &["log", "-1", "--format=%an <%ae> %at %s", "refs/heads/deploy"]
        ),
        "Nyl Orchestration <nyl@example.invalid> 1767323045 nyl: publish dev/kubernetes"
    );

    let [tree] = artifacts.as_slice() else {
        panic!("one artifact: {artifacts:?}");
    };
    assert_eq!(tree.kind, "PublishedTree");
    assert_eq!(tree.metadata.name, "tree");
    assert_eq!(tree.metadata.unit.name.as_str(), "kubernetes");
    assert_eq!(Some(&tree.execution_key), unit.execution_key.as_ref());
    assert_eq!(tree.spec["repository"], json!(world.project_url()));
    assert_eq!(tree.spec["branch"], json!("deploy"));
    assert_eq!(tree.spec["pathPrefix"], json!("dev"));
    assert_eq!(tree.spec["commit"], json!(head));
    assert!(tree.spec["indexDigest"].as_str().unwrap().starts_with("sha256:"));

    // Re-executing the same document converges without a new commit.
    let Supported::Supported(Outcome::Succeeded { artifacts: again, .. }) = driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&identity)), &unit)
        .unwrap()
    else {
        panic!("the repeated publication succeeds");
    };
    assert_eq!(again, artifacts);
    assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
}

#[test]
fn test_fake_publication_without_a_commit_identity_fails_before_effects() {
    let world = World::new();
    let driver = FakePublication::new(Script::new());
    let unit = desired(spec(), ExecutionKey::from_digest_of(b"key"));
    let error = driver
        .reconcile(&context(&world, DriverOperation::Reconcile, None), &unit)
        .unwrap_err();
    assert!(!error.effects_possible());
    assert!(world.files("main").iter().all(|path| !path.starts_with("dev/")));
    let branches = git(&world.bare(), &["branch", "--list", "deploy"]);
    assert!(branches.is_empty(), "{branches}");
}

fn deleting(unit: DesiredUnit) -> DesiredUnit {
    DesiredUnit {
        lifecycle: Lifecycle::Deleting {
            deletion: Deletion {
                hold: false,
                reason: DeletionReason::Omission,
                intent: DeletionIntent::Teardown,
                teardown: Some(TeardownProgress::InProgress),
                requested: None,
            },
        },
        ..unit
    }
}

/// The observed file of a unit whose teardown waits after `phase1`.
fn waiting_on(unit: &DesiredUnit, phase1: &str) -> ObservedUnit {
    ObservedUnit {
        api_version: StateV1,
        kind: ObservedUnitKind,
        unit: unit.unit.clone(),
        receipt: None,
        condition: None,
        desired: None,
        waiting: Some(TeardownWaiting {
            phase: "applications-removed".to_owned(),
            reason: "phase 1 published".to_owned(),
            since: chrono::Utc::now(),
            until: None,
            phase1_commit: Some(CommitId::new(phase1).unwrap()),
            run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
        }),
    }
}

/// Every document published beneath `dev/_nyl/catalog` on `deploy`, as
/// `(kind, source path)`.
fn published_catalog(world: &World) -> Vec<(String, Option<String>)> {
    world
        .files("deploy")
        .iter()
        .filter(|path| path.starts_with("dev/_nyl/catalog/"))
        .map(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]))
        .map(|text| {
            // One generated resource per file: its top-level kind and, for
            // an Application, its source path.
            let kind = text
                .lines()
                .find_map(|line| line.strip_prefix("kind: "))
                .unwrap()
                .to_owned();
            let path = text
                .lines()
                .find_map(|line| line.trim_start().strip_prefix("path: "))
                .map(ToOwned::to_owned);
            (kind, path)
        })
        .collect()
}

/// Commit `path` to `deploy` as another tool would.
fn commit_unowned(world: &World, path: &str) {
    let clone = world.root.path().join("other-tool");
    git(
        world.root.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            "deploy",
            &world.project_url(),
            "other-tool",
        ],
    );
    let file = clone.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, "kept: true\n").unwrap();
    git(&clone, &["add", "--all"]);
    git(&clone, &["commit", "--quiet", "--message", "Another tool"]);
    git(&clone, &["push", "--quiet", "origin", "HEAD:refs/heads/deploy"]);
}

/// Catches either teardown phase touching the wrong files: phase 1 must
/// drop only the workload Applications (keeping the catalog Application and
/// the AppProjects, so the catalog is never empty) while the workload trees
/// stay; a repeated call while waiting must publish nothing; phase 2 must
/// remove exactly the index-owned files and keep files other tools commit.
#[test]
fn test_fake_publication_tears_down_in_two_phases() {
    let world = World::new();
    let driver = FakePublication::new(Script::new());
    let identity: CommitIdentity = "Nyl Orchestration <nyl@example.invalid>".parse().unwrap();
    let unit = desired(spec(), key(&driver, &world));
    let Supported::Supported(Outcome::Succeeded { .. }) = driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&identity)), &unit)
        .unwrap()
    else {
        panic!("the publication succeeds");
    };
    let before = published_catalog(&world);
    let workload_trees = world
        .files("deploy")
        .into_iter()
        .filter(|path| path.starts_with("dev/web/"))
        .collect::<Vec<_>>();
    assert!(!workload_trees.is_empty());
    let is_catalog =
        |(kind, path): &(String, Option<String>)| kind != "Application" || path.as_deref() == Some("dev/_nyl/catalog");
    assert!(
        !before.iter().all(is_catalog),
        "a workload Application is published: {before:?}"
    );

    // Phase 1.
    let unit = deleting(unit);
    let Supported::Supported(Outcome::Waiting {
        phase,
        until,
        published: Some(phase1),
        ..
    }) = driver
        .teardown(&context(&world, DriverOperation::Teardown, Some(&identity)), &unit)
        .unwrap()
    else {
        panic!("phase 1 waits");
    };
    assert_eq!(phase, "applications-removed");
    assert_eq!(until, None);
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    assert_eq!(phase1.as_str(), head);
    let after = published_catalog(&world);
    assert_eq!(
        after,
        before
            .iter()
            .filter(|entry| is_catalog(entry))
            .cloned()
            .collect::<Vec<_>>()
    );
    assert!(after.iter().any(|(kind, _)| kind == "AppProject"), "{after:?}");
    assert!(
        after
            .iter()
            .any(|(kind, path)| kind == "Application" && path.as_deref() == Some("dev/_nyl/catalog")),
        "{after:?}"
    );
    for tree in &workload_trees {
        assert!(world.files("deploy").contains(tree), "{tree} stays in phase 1");
    }

    // While the wait is recorded, the same wait again and no new commit.
    let observed = waiting_on(&unit, phase1.as_str());
    let mut waiting_context = context(&world, DriverOperation::Teardown, Some(&identity));
    waiting_context.observed = Some(&observed);
    let Supported::Supported(Outcome::Waiting { published, .. }) = driver.teardown(&waiting_context, &unit).unwrap()
    else {
        panic!("the wait continues");
    };
    assert_eq!(published, Some(phase1.clone()));
    assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);

    // Phase 2, with a file another tool committed in the prefix.
    commit_unowned(&world, "dev/state/notes.yaml");
    let mut completed = context(&world, DriverOperation::Teardown, Some(&identity));
    completed.observed = Some(&observed);
    completed.teardown_wait = Some(WaitCompletion::Observed);
    let Supported::Supported(Outcome::Succeeded { outputs, artifacts, .. }) =
        driver.teardown(&completed, &unit).unwrap()
    else {
        panic!("phase 2 succeeds");
    };
    assert_eq!(outputs, json!({}));
    assert!(artifacts.is_empty());
    assert_eq!(world.files("deploy"), vec!["dev/state/notes.yaml".to_owned()]);
    assert_eq!(
        git(&world.bare(), &["log", "-1", "--format=%an %s", "refs/heads/deploy"]),
        "Nyl Orchestration nyl: teardown dev/kubernetes: remove the owned files"
    );
}

/// Catches a teardown writing to the publication branch without the
/// identity the command line resolved.
#[test]
fn test_fake_publication_teardown_without_a_commit_identity_fails_before_effects() {
    let world = World::new();
    let driver = FakePublication::new(Script::new());
    let unit = deleting(desired(spec(), ExecutionKey::from_digest_of(b"key")));
    let error = driver
        .teardown(&context(&world, DriverOperation::Teardown, None), &unit)
        .unwrap_err();
    assert!(!error.effects_possible());
    assert!(git(&world.bare(), &["branch", "--list", "deploy"]).is_empty());
}

/// Catches `status` or `plan` showing readiness from anything but the
/// stored document, or claiming requirements for a document that records none.
#[test]
fn test_fake_publication_teardown_settings_come_from_the_stored_document() {
    let driver = FakePublication::new(Script::new());
    let mut recorded = spec();
    recorded["teardownSettings"] = json!({
        "catalogPrunes": false,
        "applicationDeletionPolicy": "Foreground",
        "namespaceDeletePolicies": {"web": "Confirm"},
        "catalogOwnedByParent": true,
    });
    let unit = desired(recorded, ExecutionKey::from_digest_of(b"key"));
    assert_eq!(
        driver.teardown_settings(&unit).unwrap(),
        Supported::Supported(TeardownSettings {
            catalog_prunes: Some(false),
            application_deletion_policy: Some("Foreground".to_owned()),
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Confirm".to_owned())]),
            catalog_owned_by_parent: true,
        })
    );

    // A parent-owned catalog records its settings as external.
    let mut external = spec();
    external["teardownSettings"] = json!({"catalogOwnedByParent": true});
    let unit = desired(external, ExecutionKey::from_digest_of(b"key"));
    let Supported::Supported(settings) = driver.teardown_settings(&unit).unwrap() else {
        panic!("recorded settings are supported");
    };
    assert_eq!(
        (settings.catalog_prunes, settings.application_deletion_policy),
        (None, None)
    );

    let unit = desired(spec(), ExecutionKey::from_digest_of(b"key"));
    assert_eq!(driver.teardown_settings(&unit).unwrap(), Supported::Unsupported);
}

/// Catches a stored `teardownWait` that the wait rule cannot read, such
/// as a delay without a duration, reaching state.
#[test]
fn test_fake_publication_validates_teardown_wait_and_settings() {
    let driver = FakePublication::new(Script::new());
    let with = |field: &str, value: Value| {
        let mut spec = spec();
        spec[field] = value;
        driver.validate_spec(&spec)
    };
    for valid in [
        json!({"strategy": "observe"}),
        json!({"strategy": "manual"}),
        json!({"strategy": "delay", "duration": "10m"}),
    ] {
        assert!(with("teardownWait", valid.clone()).is_ok(), "{valid}");
    }
    for invalid in [
        json!({"strategy": "delay"}),
        json!({"strategy": "delay", "duration": "ten minutes"}),
        json!({"strategy": "observe", "duration": "10m"}),
        json!({"strategy": "wait"}),
        json!({"strategy": "manual", "until": "never"}),
        json!("manual"),
    ] {
        assert!(with("teardownWait", invalid.clone()).is_err(), "{invalid}");
    }
    assert!(with("teardownSettings", json!({"catalogPrunes": true})).is_ok());
    assert!(with("teardownSettings", json!({"catalogPrunes": "yes"})).is_err());
    assert!(with("teardownSettings", json!({"prune": true})).is_err());
}

/// Catches `nyl plan` showing a deleting publication as a publish.
#[test]
fn test_fake_publication_plans_a_deleting_unit_as_destroy() {
    let world = World::new();
    let driver = FakePublication::new(Script::new());
    let unit = deleting(desired(spec(), ExecutionKey::from_digest_of(b"key")));
    let Supported::Supported(plan) = driver
        .plan(&context(&world, DriverOperation::Plan, None), &unit)
        .unwrap()
    else {
        panic!("FakePublication plans");
    };
    assert_eq!(plan.changes, Change::Destroy);
    assert!(plan.summary.contains("two phases"), "{}", plan.summary);
}
