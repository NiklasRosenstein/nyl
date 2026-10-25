//! The Git source resolver against a bare origin and its clone: revisions,
//! reachability, pinned worktrees, and the default branch.
//!
//! Contract: [Source](../../../design/orchestration-core.md#source) and
//! [Pinned commits](../../../design/orchestration-core.md#pinned-commits).

use std::path::PathBuf;

use git2::{Oid, Repository, Signature, Time};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver};
use nyl_git::GitAuth;
use nyl_state::source::GitSourceResolver;
use tempfile::TempDir;

/// A bare origin with `main` (c1, c2), `feature` (c3 on c2), and tag `v1`
/// (c1), and a clone of it whose only local branch is `main`.
struct World {
    root: TempDir,
    c1: Oid,
    c2: Oid,
    c3: Oid,
}

fn signature() -> Signature<'static> {
    Signature::new("Test", "test@example.com", &Time::new(1_700_000_000, 0)).unwrap()
}

/// Commit `files` (path, content) on top of `parent` without touching refs.
fn commit(repository: &Repository, parent: Option<Oid>, files: &[(&str, &str)], message: &str) -> Oid {
    let mut index = git2::Index::new().unwrap();
    if let Some(parent) = parent {
        index
            .read_tree(&repository.find_commit(parent).unwrap().tree().unwrap())
            .unwrap();
    }
    for (path, content) in files {
        let blob = repository.blob(content.as_bytes()).unwrap();
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100_644,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path: path.as_bytes().to_vec(),
            })
            .unwrap();
    }
    let tree = repository.find_tree(index.write_tree_to(repository).unwrap()).unwrap();
    let parents = parent
        .map(|parent| repository.find_commit(parent).unwrap())
        .into_iter()
        .collect::<Vec<_>>();
    let parents = parents.iter().collect::<Vec<_>>();
    repository
        .commit(None, &signature(), &signature(), message, &tree, &parents)
        .unwrap()
}

impl World {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let origin = Repository::init_bare(root.path().join("origin.git")).unwrap();
        let c1 = commit(&origin, None, &[("nyl/nyl.toml", ""), ("app.txt", "one")], "c1");
        let c2 = commit(&origin, Some(c1), &[("app.txt", "two")], "c2");
        let c3 = commit(&origin, Some(c2), &[("app.txt", "three")], "c3");
        origin.reference("refs/heads/main", c2, true, "main").unwrap();
        origin.reference("refs/heads/feature", c3, true, "feature").unwrap();
        origin.set_head("refs/heads/main").unwrap();
        let c1_object = origin.find_object(c1, None).unwrap();
        origin.tag("v1", &c1_object, &signature(), "v1", false).unwrap();

        git2::build::RepoBuilder::new()
            .clone(
                root.path().join("origin.git").to_str().unwrap(),
                &root.path().join("clone"),
            )
            .unwrap();
        Self { root, c1, c2, c3 }
    }

    fn clone_dir(&self) -> PathBuf {
        self.root.path().join("clone")
    }

    fn resolver(&self) -> GitSourceResolver {
        GitSourceResolver::new(self.clone_dir().join(".git"), GitAuth::none())
    }

    #[cfg(unix)]
    fn origin_url(&self) -> String {
        url(&self.root.path().join("origin.git"))
    }
}

#[cfg(unix)]
fn url(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

fn id(oid: Oid) -> CommitId {
    CommitId::new(oid.to_string()).unwrap()
}

#[test]
fn test_resolve_revision_finds_branches_tags_and_commits() {
    let world = World::new();
    let resolver = world.resolver();
    let this = SourceRepository::This;
    assert_eq!(resolver.resolve_revision(&this, "main").unwrap(), id(world.c2));
    // Only origin/feature exists in the clone.
    assert_eq!(resolver.resolve_revision(&this, "feature").unwrap(), id(world.c3));
    assert_eq!(resolver.resolve_revision(&this, "v1").unwrap(), id(world.c1));
    assert_eq!(
        resolver.resolve_revision(&this, &world.c1.to_string()).unwrap(),
        id(world.c1)
    );
    let error = resolver.resolve_revision(&this, "release/prod").unwrap_err();
    assert!(error.to_string().contains("release/prod"), "{error}");
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let error = resolver.resolve_revision(&this, absent).unwrap_err();
    assert!(error.to_string().contains(absent), "{error}");
}

#[cfg(unix)] // A `file://` URL of a temporary path.
#[test]
fn test_resolve_revision_of_a_url_repository_fetches_it() {
    let world = World::new();
    let resolver = world.resolver();
    let other = SourceRepository::Url(world.origin_url());
    assert_eq!(resolver.resolve_revision(&other, "feature").unwrap(), id(world.c3));
    assert_eq!(resolver.resolve_revision(&other, "refs/tags/v1").unwrap(), id(world.c1));
    assert_eq!(
        resolver
            .reachable_from(&other, &id(world.c1), &["main".to_owned()])
            .unwrap(),
        Some("main".to_owned())
    );
    assert_eq!(resolver.default_branch(&other).unwrap(), "main");
}

#[test]
fn test_reachable_from_main_but_not_from_a_feature_only_commit() {
    let world = World::new();
    let resolver = world.resolver();
    let this = SourceRepository::This;
    let main = vec!["main".to_owned()];
    assert_eq!(
        resolver.reachable_from(&this, &id(world.c1), &main).unwrap(),
        Some("main".to_owned())
    );
    assert_eq!(
        resolver.reachable_from(&this, &id(world.c2), &main).unwrap(),
        Some("main".to_owned())
    );
    assert_eq!(resolver.reachable_from(&this, &id(world.c3), &main).unwrap(), None);
    let with_glob = vec!["main".to_owned(), "feat*".to_owned()];
    assert_eq!(
        resolver.reachable_from(&this, &id(world.c3), &with_glob).unwrap(),
        Some("feat*".to_owned())
    );
    let absent = CommitId::new("0123456789abcdef0123456789abcdef01234567").unwrap();
    assert_eq!(resolver.reachable_from(&this, &absent, &with_glob).unwrap(), None);
}

#[test]
fn test_worktree_is_a_clean_detached_checkout_of_the_commit() {
    let world = World::new();
    let resolver = world.resolver();
    // Uncommitted changes in the entry clone never reach a pinned worktree.
    std::fs::write(world.clone_dir().join("app.txt"), "dirty").unwrap();
    std::fs::write(world.clone_dir().join("untracked.txt"), "x").unwrap();

    let first = world.root.path().join("workspace/first");
    let second = world.root.path().join("workspace/second");
    resolver
        .worktree(&SourceRepository::This, &id(world.c1), &first)
        .unwrap();
    resolver
        .worktree(&SourceRepository::This, &id(world.c3), &second)
        .unwrap();

    assert_eq!(std::fs::read_to_string(first.join("app.txt")).unwrap(), "one");
    assert!(first.join("nyl/nyl.toml").is_file());
    assert!(!first.join("untracked.txt").exists());
    assert_eq!(std::fs::read_to_string(second.join("app.txt")).unwrap(), "three");

    let checkout = Repository::open(&first).unwrap();
    assert!(checkout.head_detached().unwrap());
    assert_eq!(checkout.head().unwrap().target(), Some(world.c1));
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).include_ignored(false);
    assert!(checkout.statuses(Some(&mut options)).unwrap().is_empty());
    // Discovery from inside the checkout finds the checkout, not the clone.
    let discovered = Repository::discover(first.join("nyl")).unwrap();
    assert_eq!(
        discovered.workdir().unwrap().canonicalize().unwrap(),
        first.canonicalize().unwrap()
    );

    // Each call is independent.
    std::fs::write(second.join("app.txt"), "changed").unwrap();
    assert_eq!(std::fs::read_to_string(first.join("app.txt")).unwrap(), "one");

    GitSourceResolver::remove_worktree(&first).unwrap();
    GitSourceResolver::remove_worktree(&second).unwrap();
    assert!(!first.exists() && !second.exists());
    let clone = Repository::open(world.clone_dir()).unwrap();
    assert!(clone.worktrees().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(world.clone_dir().join("app.txt")).unwrap(),
        "dirty"
    );
}

#[test]
fn test_worktree_rejects_a_symbolic_link_leaving_the_tree() {
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let base = clone.find_commit(world.c2).unwrap().tree().unwrap();
    let mut builder = clone.treebuilder(Some(&base)).unwrap();
    let target = clone.blob(b"../../outside").unwrap();
    builder.insert("escape", target, 0o120_000).unwrap();
    let tree = clone.find_tree(builder.write().unwrap()).unwrap();
    let parent = clone.find_commit(world.c2).unwrap();
    let escaping = clone
        .commit(None, &signature(), &signature(), "escape", &tree, &[&parent])
        .unwrap();

    let path = world.root.path().join("workspace/escape");
    let error = world
        .resolver()
        .worktree(&SourceRepository::This, &id(escaping), &path)
        .unwrap_err();
    assert!(error.to_string().contains("escape"), "{error}");
}

#[test]
fn test_default_branch_follows_origin_head_then_local_head() {
    let world = World::new();
    assert_eq!(
        world.resolver().default_branch(&SourceRepository::This).unwrap(),
        "main"
    );

    let local = world.root.path().join("local");
    let repository = Repository::init(&local).unwrap();
    repository.set_head("refs/heads/trunk").unwrap();
    let resolver = GitSourceResolver::new(local.join(".git"), GitAuth::none());
    assert_eq!(resolver.default_branch(&SourceRepository::This).unwrap(), "trunk");
}

/// A commit with an escaping symbolic link on top of `parent`, in `repository`.
fn escaping_commit(repository: &Repository, parent: Oid) -> Oid {
    let parent = repository.find_commit(parent).unwrap();
    let mut builder = repository.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    let target = repository.blob(b"../../outside").unwrap();
    builder.insert("escape", target, 0o120_000).unwrap();
    let tree = repository.find_tree(builder.write().unwrap()).unwrap();
    repository
        .commit(None, &signature(), &signature(), "escape", &tree, &[&parent])
        .unwrap()
}

/// A run's concurrency slot reuses one checkout across units: catches a
/// unit seeing files an earlier unit left behind, changed, or deleted, and
/// a reused checkout that no longer behaves as its own repository.
#[test]
fn test_reset_worktree_reuses_a_slot_as_an_exact_fresh_checkout() {
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let first = commit(
        &clone,
        None,
        &[
            ("nyl/nyl.toml", ""),
            ("app.txt", "one"),
            ("gone.txt", "only in the first commit"),
            ("dir/keep.txt", "kept"),
            (".gitignore", "*.log\nbuild/\n"),
        ],
        "first",
    );
    let second = commit(
        &clone,
        None,
        &[
            ("nyl/nyl.toml", ""),
            ("app.txt", "three"),
            ("dir/keep.txt", "kept"),
            (".gitignore", "*.log\nbuild/\n"),
        ],
        "second",
    );
    let resolver = world.resolver();
    let slot = world.root.path().join("session/slots/slot-0");
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(slot.join("gone.txt")).unwrap(),
        "only in the first commit"
    );

    // What a unit may leave behind.
    std::fs::write(slot.join("app.txt"), "tampered").unwrap();
    std::fs::remove_file(slot.join("dir/keep.txt")).unwrap();
    std::fs::write(slot.join("untracked.txt"), "x").unwrap();
    std::fs::write(slot.join("debug.log"), "ignored").unwrap();
    std::fs::create_dir_all(slot.join("build")).unwrap();
    std::fs::write(slot.join("build/out.bin"), "ignored").unwrap();
    std::fs::create_dir_all(slot.join("empty/nested")).unwrap();
    // A file only the reused objects directory has, to tell reuse from a
    // fresh checkout.
    std::fs::write(slot.join(".git/objects/reused"), "").unwrap();

    resolver
        .reset_worktree(&SourceRepository::This, &id(second), &slot)
        .unwrap();
    assert_eq!(std::fs::read_to_string(slot.join("app.txt")).unwrap(), "three");
    assert_eq!(std::fs::read_to_string(slot.join("dir/keep.txt")).unwrap(), "kept");
    for leftover in ["gone.txt", "untracked.txt", "debug.log", "build", "empty"] {
        assert!(!slot.join(leftover).exists(), "{leftover} survived the reset");
    }
    assert!(
        slot.join(".git/objects/reused").exists(),
        "the slot's checkout was not reused"
    );
    let checkout = Repository::open(&slot).unwrap();
    assert!(checkout.head_detached().unwrap());
    assert_eq!(checkout.head().unwrap().target(), Some(second));
    let discovered = Repository::discover(slot.join("nyl")).unwrap();
    assert_eq!(
        discovered.workdir().unwrap().canonicalize().unwrap(),
        slot.canonicalize().unwrap()
    );

    // A nested repository, which a checkout skips, forces a fresh checkout.
    Repository::init(slot.join("nested")).unwrap();
    std::fs::write(slot.join("nested/file.txt"), "x").unwrap();
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    assert!(!slot.join("nested").exists());
    assert_eq!(std::fs::read_to_string(slot.join("app.txt")).unwrap(), "one");
    assert_eq!(Repository::open(&slot).unwrap().head().unwrap().target(), Some(first));
}

/// Two commits for slot tests: `first` with `dir/keep.txt` and a
/// multi-line `app.txt`, and `second` that adds `dir/new.txt`.
fn slot_commits(clone: &Repository) -> (Oid, Oid) {
    let first = commit(
        clone,
        None,
        &[
            ("nyl/nyl.toml", ""),
            ("app.txt", "one\ntwo\n"),
            ("dir/keep.txt", "kept"),
        ],
        "first",
    );
    let second = commit(clone, Some(first), &[("dir/new.txt", "new")], "second");
    (first, second)
}

/// A unit may replace a tracked directory in its slot with a symbolic link
/// leaving the slot: catches the next reset writing through it, outside
/// the checkout, instead of restoring a real directory.
#[cfg(unix)]
#[test]
fn test_reset_worktree_does_not_write_through_a_link_a_unit_left() {
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let (first, second) = slot_commits(&clone);
    let resolver = world.resolver();
    let slot = world.root.path().join("slot");
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::remove_dir_all(slot.join("dir")).unwrap();
    std::os::unix::fs::symlink(outside.path(), slot.join("dir")).unwrap();

    resolver
        .reset_worktree(&SourceRepository::This, &id(second), &slot)
        .unwrap();
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(std::fs::symlink_metadata(slot.join("dir")).unwrap().is_dir());
    assert_eq!(std::fs::read_to_string(slot.join("dir/keep.txt")).unwrap(), "kept");
    assert_eq!(std::fs::read_to_string(slot.join("dir/new.txt")).unwrap(), "new");
}

/// Catches a reused slot keeping a file mode a unit changed.
#[cfg(unix)]
#[test]
fn test_reset_worktree_restores_a_changed_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let (first, second) = slot_commits(&clone);
    let resolver = world.resolver();
    let slot = world.root.path().join("slot");
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    std::fs::set_permissions(slot.join("app.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();

    resolver
        .reset_worktree(&SourceRepository::This, &id(second), &slot)
        .unwrap();
    let mode = std::fs::metadata(slot.join("app.txt")).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0, "app.txt kept the executable bits: {mode:o}");
}

/// A unit may change the slot's Git metadata (tools in units often run
/// `git config`): catches the next reset applying it on the reuse path,
/// writing other bytes than the commit holds while the status check still
/// reports a clean tree, or writing outside the slot.
#[cfg(unix)]
#[test]
fn test_reset_worktree_ignores_git_metadata_a_unit_changed() {
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let (first, second) = slot_commits(&clone);
    let resolver = world.resolver();
    let slot = world.root.path().join("slot");
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    let set_config = |configure: &dyn Fn(&mut git2::Config)| {
        let checkout = Repository::open(&slot).unwrap();
        let mut config = checkout.config().unwrap().open_level(git2::ConfigLevel::Local).unwrap();
        configure(&mut config);
    };

    set_config(&|config| {
        config.set_bool("core.autocrlf", true).unwrap();
        config.set_bool("core.filemode", false).unwrap();
    });
    std::fs::create_dir_all(slot.join(".git/info")).unwrap();
    std::fs::write(slot.join(".git/info/attributes"), "* text eol=crlf\n").unwrap();
    std::fs::write(slot.join(".git/info/sparse-checkout"), "/nyl/\n").unwrap();
    std::fs::write(slot.join("app.txt"), "tampered").unwrap();
    std::fs::write(slot.join(".git/objects/reused"), "").unwrap();
    resolver
        .reset_worktree(&SourceRepository::This, &id(second), &slot)
        .unwrap();
    assert!(
        slot.join(".git/objects/reused").exists(),
        "the slot's checkout was not reused"
    );
    assert_eq!(std::fs::read(slot.join("app.txt")).unwrap(), b"one\ntwo\n");
    assert_eq!(std::fs::read_to_string(slot.join("dir/new.txt")).unwrap(), "new");

    let outside = TempDir::new().unwrap();
    set_config(&|config| {
        config
            .set_str("core.worktree", outside.path().to_str().unwrap())
            .unwrap();
    });
    resolver
        .reset_worktree(&SourceRepository::This, &id(first), &slot)
        .unwrap();
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(!slot.join("dir/new.txt").exists());
    let checkout = Repository::open(&slot).unwrap();
    assert_eq!(
        checkout.workdir().unwrap().canonicalize().unwrap(),
        slot.canonicalize().unwrap()
    );
    assert_eq!(checkout.head().unwrap().target(), Some(first));
}

/// Catches the per-tree symbolic link cache admitting a rejected tree, or
/// a cached pass of one tree admitting another.
#[test]
fn test_reset_worktree_rejects_an_escaping_link_after_a_cached_pass() {
    let world = World::new();
    let clone = Repository::open(world.clone_dir()).unwrap();
    let escaping = escaping_commit(&clone, world.c2);
    let resolver = world.resolver();
    let slot = world.root.path().join("slot");
    resolver
        .reset_worktree(&SourceRepository::This, &id(world.c2), &slot)
        .unwrap();
    resolver
        .reset_worktree(&SourceRepository::This, &id(world.c2), &slot)
        .unwrap();
    for _ in 0..2 {
        let error = resolver
            .reset_worktree(&SourceRepository::This, &id(escaping), &slot)
            .unwrap_err();
        assert!(error.to_string().contains("escape"), "{error}");
    }
    let error = resolver
        .worktree(&SourceRepository::This, &id(escaping), &world.root.path().join("other"))
        .unwrap_err();
    assert!(error.to_string().contains("escape"), "{error}");
    // The slot still holds the last commit that passed.
    assert_eq!(std::fs::read_to_string(slot.join("app.txt")).unwrap(), "two");
}

/// Catches a slot reset deleting a directory Nyl did not create.
#[test]
fn test_reset_worktree_refuses_a_foreign_directory() {
    let world = World::new();
    let foreign = world.root.path().join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("precious.txt"), "keep").unwrap();
    let error = world
        .resolver()
        .reset_worktree(&SourceRepository::This, &id(world.c1), &foreign)
        .unwrap_err();
    assert!(error.to_string().contains("not empty"), "{error}");
    assert_eq!(std::fs::read_to_string(foreign.join("precious.txt")).unwrap(), "keep");
}
