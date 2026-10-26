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
    Change, Driver, DriverOperation, DriverResult, ExecutionContext, Outcome, ResolutionContext, Supported, ToolExit,
    ToolInvocation, ToolRunner,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, Uid, UnitAddress, UnitName};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{DesiredUnit, DesiredUnitKind, Lifecycle, Readiness, StateV1, UnitIdentity};
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
