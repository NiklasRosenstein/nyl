use std::path::Path;
use std::sync::Mutex;

use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver, SourceResult};
use nyl_drivers::fake::{FakeImage, Script};
use nyl_orchestration::shell::sources::{main_checkout_paths, prepare_source_roots};
use serde_json::json;

type CheckoutCall = (SourceRepository, CommitId, Option<Vec<String>>);

#[derive(Default)]
struct Recording(Mutex<Vec<CheckoutCall>>);
impl SourceResolver for Recording {
    fn resolve_revision(&self, _: &SourceRepository, _: &str) -> SourceResult<CommitId> {
        unreachable!()
    }
    fn reachable_from(&self, _: &SourceRepository, _: &CommitId, _: &[String]) -> SourceResult<Option<String>> {
        unreachable!()
    }
    fn worktree(&self, _: &SourceRepository, _: &CommitId, _: &Path) -> SourceResult<()> {
        unreachable!()
    }
    fn reset_worktree(&self, _: &SourceRepository, _: &CommitId, _: &Path) -> SourceResult<()> {
        unreachable!()
    }
    fn default_branch(&self, _: &SourceRepository) -> SourceResult<String> {
        unreachable!()
    }
    fn reset_worktree_with_paths(
        &self,
        repo: &SourceRepository,
        commit: &CommitId,
        _: &Path,
        paths: Option<&[String]>,
    ) -> SourceResult<()> {
        self.0
            .lock()
            .unwrap()
            .push((repo.clone(), commit.clone(), paths.map(<[String]>::to_vec)));
        Ok(())
    }
}

#[test]
fn matching_sources_share_checkouts_and_union_sparse_paths() {
    let driver = FakeImage::new(Script::new());
    let first: CommitId = "a".repeat(40).parse().unwrap();
    let other: CommitId = "b".repeat(40).parse().unwrap();
    let spec = json!({"context": {"path": "/one", "commit": first}, "contexts": {
        "local": {"path": "/two", "commit": first},
        "remote-a": {"path": "/three", "commit": other, "repository": {"repoURL": "https://example.org/repo.git"}},
        "remote-b": {"path": "/four", "commit": other, "repository": {"repoURL": "https://example.org/repo.git"}}
    }});
    let paths = main_checkout_paths(&driver, &spec, &first, &first, "/nyl").unwrap();
    assert_eq!(paths.unwrap(), vec!["/nyl", "/one", "/two"]);
    assert_eq!(
        main_checkout_paths(&driver, &spec, &first, &other, "/nyl")
            .unwrap()
            .unwrap(),
        vec!["/nyl"]
    );
    let resolver = Recording::default();
    let temp = tempfile::TempDir::new().unwrap();
    let main = temp.path().join("main");
    let roots = prepare_source_roots(
        &resolver,
        &driver,
        &spec,
        &first,
        "/nyl",
        temp.path(),
        Some((&main, &first)),
    )
    .unwrap();
    assert_eq!(roots["/context"].worktree, main);
    assert_eq!(roots["/contexts/local"].worktree, main);
    assert_eq!(
        roots["/contexts/remote-a"].worktree,
        roots["/contexts/remote-b"].worktree
    );
    let calls = resolver.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].2.as_ref().unwrap(),
        &vec!["/four".to_owned(), "/three".to_owned()]
    );
}
