//! `nyl build` for the real `KubernetesPublication` driver, against the shared
//! publication world: the render, `--diff` against the published tree, and
//! `--push` for a target no environment owns.
//!
//! Contract: [Direct builds](../../../design/orchestration-core.md#direct-builds)
//! and the [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Direct builds of a publication.

mod publication_world;

use std::ffi::OsString;
use std::path::Path;

use nyl_core::orchestration::driver::{Driver, DriverOperation, DriverResult, ExecutionContext, Supported};
use nyl_core::orchestration::ids::ExecutionKey;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit};
use nyl_render::gitops::publication::ensure_git_worktree;
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{context, desired, git, spec, World};
use serde_json::{json, Value};

fn real() -> KubernetesPublicationDriver {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    KubernetesPublicationDriver::new(nyl_render::config::ResourceCatalogue(descriptors))
}

fn identity() -> CommitIdentity {
    "Direct Builder <builder@example.invalid>".parse().unwrap()
}

fn unit() -> DesiredUnit {
    desired(spec(), ExecutionKey::from_digest_of(b"key"))
}

fn arguments(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn build(ctx: &ExecutionContext<'_>, args: &[&str], unit: &DesiredUnit) -> DriverResult<Vec<ArtifactDocument>> {
    match real().build(ctx, unit, &arguments(args))? {
        Supported::Supported(documents) => Ok(documents),
        Supported::Unsupported => panic!("the publication driver builds"),
    }
}

/// The only document a build printed.
fn only(documents: Vec<ArtifactDocument>) -> ArtifactDocument {
    let [document] = <[ArtifactDocument; 1]>::try_from(documents).expect("one document");
    document
}

/// The world whose `kubernetes` unit no Environment selects: the target
/// `dev` is then placed by no environment, so a direct build may publish it.
fn standalone() -> World {
    let world = World::new();
    let units = world.work().join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap();
    let marker = "kind: KubernetesPublication\nmetadata:\n  name: kubernetes\n  labels: {dev: 'true'}";
    assert!(text.contains(marker));
    std::fs::write(
        &units,
        text.replace(marker, &marker.replace("dev: 'true'", "standalone: 'true'")),
    )
    .unwrap();
    world
}

fn deploy_exists(world: &World) -> bool {
    !git(&world.bare(), &["branch", "--list", "deploy"]).is_empty()
}

/// Publish `unit` as an environment's run does, so the prefix names an owner.
fn reconcile(world: &World, unit: &DesiredUnit) {
    let id: CommitIdentity = "Nyl Orchestration <nyl@example.invalid>".parse().unwrap();
    real()
        .reconcile(&context(world, DriverOperation::Reconcile, Some(&id)), unit)
        .unwrap();
}

fn copy_without_git(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
    {
        let entry = entry.unwrap();
        let target = to.join(entry.path().strip_prefix(from).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn changes(document: &ArtifactDocument) -> Vec<(String, String)> {
    document.spec["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_owned(),
                file["change"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Catches a plain build that writes to the publication branch, or whose
/// document does not say what would be published: where the tree goes and
/// the digest of every file beneath the prefix.
#[test]
fn test_build_renders_the_tree_and_publishes_nothing() {
    let world = World::new();
    let ctx = context(&world, DriverOperation::Build, None);
    let document = only(build(&ctx, &[], &unit()).unwrap());
    assert_eq!(document.api_version, "artifacts.gitops.nyl/v1");
    assert_eq!(document.kind, "RenderedTree");
    assert_eq!(document.metadata.unit.name.as_str(), "kubernetes");
    assert_eq!(document.spec["repository"], json!(world.project_url()));
    assert_eq!(document.spec["branch"], "deploy");
    assert_eq!(document.spec["pathPrefix"], "dev");
    assert_eq!(document.spec["target"], "dev");
    let files = document.spec["files"].as_object().unwrap();
    assert!(
        files.keys().any(|path| path.starts_with("dev/web/")),
        "{:?}",
        files.keys()
    );
    assert!(files.keys().all(|path| path.starts_with("dev/")));
    assert!(files
        .values()
        .all(|digest| digest.as_str().unwrap().starts_with("sha256:")));
    assert!(document.spec["indexDigest"].as_str().unwrap().starts_with("sha256:"));
    assert!(!deploy_exists(&world), "a plain build publishes nothing");
}

/// Catches a direct build rendering from the files it is given any
/// differently from a checkout: the shell hands over a snapshot of the
/// worktree's files made a Git worktree with one fixed commit, which must
/// discover and render the same tree, and the ownership index must name the unit's source commit,
/// not the snapshot's own.
#[test]
fn test_build_renders_a_snapshot_of_files_like_a_checkout() {
    let world = World::new();
    let from_checkout = only(build(&context(&world, DriverOperation::Build, None), &[], &unit()).unwrap());
    let snapshot = world.root.path().join("snapshot");
    copy_without_git(&world.work(), &snapshot);
    assert!(!snapshot.join(".git").exists());
    ensure_git_worktree(&snapshot).unwrap();
    let mut ctx = context(&world, DriverOperation::Build, None);
    ctx.worktree = snapshot.clone();
    ctx.project_dir = snapshot.join("nyl");
    let from_snapshot = only(build(&ctx, &[], &unit()).unwrap());
    assert_eq!(from_snapshot.spec, from_checkout.spec);

    // Published from the snapshot, the index records the unit's source commit.
    let standalone = standalone();
    let snapshot = standalone.root.path().join("snapshot");
    copy_without_git(&standalone.work(), &snapshot);
    ensure_git_worktree(&snapshot).unwrap();
    let id = identity();
    let mut ctx = context(&standalone, DriverOperation::Build, Some(&id));
    ctx.worktree = snapshot.clone();
    ctx.project_dir = snapshot.join("nyl");
    build(&ctx, &["--push"], &unit()).unwrap();
    let index: Value = serde_json::from_str(&git(&standalone.bare(), &["show", "deploy:dev/_nyl/index.json"])).unwrap();
    assert_eq!(index["sourceCommit"], json!("a".repeat(40)));
}

/// Catches `--diff` reporting the wrong comparison: a destination with
/// nothing published is a `create` of every file; a published tree that is
/// current is `none`; a changed render lists exactly the files that differ,
/// and a file the render dropped is `removed`. Nothing is ever written.
#[test]
fn test_diff_compares_the_render_with_the_published_tree() {
    let world = World::new();
    let ctx = context(&world, DriverOperation::Build, None);
    let before = only(build(&ctx, &["--diff"], &unit()).unwrap());
    assert_eq!(before.kind, "TreeDiff");
    assert_eq!(before.spec["changes"], "create");
    assert_eq!(before.spec["base"], Value::Null);
    assert_eq!(before.spec["owner"], Value::Null);
    let created = changes(&before);
    assert!(!created.is_empty());
    assert!(created
        .iter()
        .all(|(path, change)| path.starts_with("dev/") && change == "added"));
    assert!(!deploy_exists(&world));

    reconcile(&world, &unit());
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    let current = only(build(&ctx, &["--diff"], &unit()).unwrap());
    assert_eq!(current.spec["changes"], "none");
    assert_eq!(current.spec["files"], json!([]));
    assert_eq!(current.spec["base"], json!(head));
    // The published index names the unit that published it, even though the build owns nothing.
    assert_eq!(
        current.spec["owner"],
        json!({"environment": "dev", "unit": "kubernetes", "uid": "00000000-0000-7000-8000-000000000004"})
    );

    // Another image changes the Deployment only.
    let mut rebuilt = spec();
    rebuilt["releases"]["web/web"]["image"] = json!("registry.invalid:5000/web@sha256:4567");
    let rebuilt = desired(rebuilt, ExecutionKey::from_digest_of(b"key"));
    let updated = only(build(&ctx, &["--diff"], &rebuilt).unwrap());
    assert_eq!(updated.spec["changes"], "update");
    let modified = changes(&updated);
    assert!(
        !modified.is_empty() && modified.iter().all(|(_, change)| change == "modified"),
        "{modified:?}"
    );

    // A file the previous index owns and the render no longer produces is removed.
    let release = world.work().join("applications/web/release.yaml");
    let text = std::fs::read_to_string(&release).unwrap();
    let renamed = text.replace(
        "  name: web\n  namespace: web\nspec:\n  replicas",
        "  name: web2\n  namespace: web\nspec:\n  replicas",
    );
    assert_ne!(text, renamed);
    std::fs::write(&release, renamed).unwrap();
    let moved = changes(&only(build(&ctx, &["--diff"], &unit()).unwrap()));
    assert!(moved.iter().any(|(_, change)| change == "removed"), "{moved:?}");
    assert!(moved.iter().any(|(_, change)| change == "added"), "{moved:?}");
    assert_eq!(
        git(&world.bare(), &["rev-parse", "refs/heads/deploy"]),
        head,
        "a diff publishes nothing"
    );
}

/// Catches `--push` publishing from a target an environment owns. The
/// declaration decides first, before anything is rendered or written: the
/// unit is selected by Environment `dev`, so `reconcile -e dev` is the way to
/// publish it, and the refusal says so.
#[test]
fn test_push_refuses_a_target_an_environment_places_by_declaration() {
    let world = World::new();
    let id = identity();
    let ctx = context(&world, DriverOperation::Build, Some(&id));
    let error = build(&ctx, &["--push"], &unit()).unwrap_err();
    assert!(!error.effects_possible());
    let message = error.to_string();
    assert!(message.starts_with("NYL-KUBERNETES-BUILD-TARGET-OWNED:"), "{message}");
    assert!(message.contains("Environment dev"), "{message}");
    assert!(message.contains("nyl reconcile -e dev"), "{message}");
    assert!(!deploy_exists(&world));
}

/// Catches `--push` publishing over a prefix whose published index names a
/// unit incarnation, which no declaration in this worktree shows (the unit
/// may live on another branch or in an environment since renamed): the
/// publication records no owner, so it cannot tell whether that unit still
/// exists, and refuses with the contract's code and the way out.
#[test]
fn test_push_refuses_a_prefix_the_published_index_names_an_owner_for() {
    let world = standalone();
    reconcile(&world, &unit());
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    let id = identity();
    let ctx = context(&world, DriverOperation::Build, Some(&id));
    let error = build(&ctx, &["--push"], &unit()).unwrap_err();
    let message = error.to_string();
    assert!(message.starts_with("NYL-TREE-OWNED-BY-UNIT:"), "{message}");
    assert!(message.contains("remove the `owner` member"), "{message}");
    assert!(message.contains("unit dev/kubernetes"), "{message}");
    assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
}

/// Catches `--push` skipping the other safety conditions: a source with
/// uncommitted changes cannot be attributed to its commit, and a commit
/// needs an author. Both fail before anything is rendered or written.
#[test]
fn test_push_needs_a_committed_source_and_a_commit_identity() {
    let world = standalone();
    let id = identity();
    let mut dirty = context(&world, DriverOperation::Build, Some(&id));
    dirty.dirty_source = true;
    let error = build(&dirty, &["--push"], &unit()).unwrap_err();
    let message = error.to_string();
    assert!(message.starts_with("NYL-KUBERNETES-BUILD-DIRTY-SOURCE:"), "{message}");
    assert!(message.contains("--source <revision>"), "{message}");

    let anonymous = context(&world, DriverOperation::Build, None);
    let message = build(&anonymous, &["--push"], &unit()).unwrap_err().to_string();
    assert!(message.starts_with("NYL-KUBERNETES-BUILD-IDENTITY:"), "{message}");
    assert!(!deploy_exists(&world));

    // A dirty source still renders and compares: only publishing is refused.
    assert!(build(&dirty, &[], &unit()).is_ok());
    assert!(build(&dirty, &["--diff"], &unit()).is_ok());
}

/// Catches a direct publication that differs from a run's: the unit's target,
/// published once as the build's author, in a commit that names the build,
/// with an index that records no owner (so a later environment adopts it), the
/// same `PublishedTree` document a run records, and no new commit for a tree
/// that is published already.
#[test]
fn test_push_publishes_a_target_no_environment_owns() {
    let world = standalone();
    let id = identity();
    let ctx = context(&world, DriverOperation::Build, Some(&id));
    let document = only(build(&ctx, &["--push"], &unit()).unwrap());
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    assert_eq!(document.kind, "PublishedTree");
    assert_eq!(document.metadata.name, "tree");
    assert_eq!(document.spec["commit"], json!(head));
    assert_eq!(document.spec["target"], "dev");
    assert_eq!(document.spec["pathPrefix"], "dev");
    assert_eq!(
        git(
            &world.bare(),
            &["log", "-1", "--format=%an <%ae> %s", "refs/heads/deploy"]
        ),
        "Direct Builder <builder@example.invalid> nyl: build --push kubernetes"
    );
    let index: Value = serde_json::from_str(&git(&world.bare(), &["show", "deploy:dev/_nyl/index.json"])).unwrap();
    assert_eq!(index["version"], 3);
    assert_eq!(index.get("owner"), None);

    // The document a plain build prints names the digest this publication records.
    let rendered = only(build(&context(&world, DriverOperation::Build, None), &[], &unit()).unwrap());
    assert_eq!(rendered.spec["indexDigest"], document.spec["indexDigest"]);

    // The published tree is what the diff compares against: nothing differs.
    let diff = only(build(&ctx, &["--diff"], &unit()).unwrap());
    assert_eq!(diff.spec["changes"], "none");

    let again = only(build(&ctx, &["--push"], &unit()).unwrap());
    assert_eq!(again.spec, document.spec);
    assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
}

/// Catches a publication reading what a cluster holds today: `--lookup=live`
/// is refused with its code before anything is rendered or published, and the
/// default source can be named.
#[test]
fn test_build_refuses_live_lookups() {
    let world = World::new();
    let id = identity();
    let ctx = context(&world, DriverOperation::Build, Some(&id));
    for args in [vec!["--push", "--lookup=live"], vec!["--lookup", "live"]] {
        let error = build(&ctx, &args, &unit()).unwrap_err();
        assert!(!error.effects_possible());
        assert!(
            error.to_string().starts_with("NYL-HELM-LOOKUP-LIVE-REFUSED:"),
            "{error}"
        );
    }
    assert!(!deploy_exists(&world));
    build(&ctx, &["--lookup=snapshot"], &unit()).unwrap();
    // The option takes its value as a separate argument too, as `nyl render` accepts it.
    build(&ctx, &["--lookup", "snapshot"], &unit()).unwrap();
    let error = build(&ctx, &["--lookup"], &unit()).unwrap_err();
    assert!(error.to_string().contains("--lookup takes a value"), "{error}");
}

/// Catches arguments the driver does not own being passed over, or two modes
/// combined into a publication nobody compared first.
#[test]
fn test_build_rejects_arguments_it_does_not_define() {
    let world = World::new();
    let id = identity();
    let ctx = context(&world, DriverOperation::Build, Some(&id));
    for args in [
        vec!["--load"],
        vec!["--diff", "--push"],
        vec!["--tag", "x"],
        vec!["diff"],
    ] {
        let error = build(&ctx, &args, &unit()).unwrap_err();
        assert!(!error.effects_possible());
        assert!(
            error.to_string().starts_with("NYL-KUBERNETES-BUILD-ARGUMENT:"),
            "{error}"
        );
    }
    assert!(!deploy_exists(&world));
}
