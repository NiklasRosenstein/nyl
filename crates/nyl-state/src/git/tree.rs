//! Commits as maps from path to bytes.
//!
//! State is small, so a commit is read as the complete map of its files and
//! written back from a changed map. Paths use `/`, relative to the tree root.

use std::collections::BTreeMap;

use git2::{Oid, Repository, Signature};
use nyl_core::orchestration::ports::StateResult;

use super::refs::unavailable;

pub type Files = BTreeMap<String, Vec<u8>>;

/// Every file of `commit`'s tree, or nothing for no commit.
pub fn read(repository: &Repository, commit: Option<Oid>) -> StateResult<Files> {
    let mut files = Files::new();
    let Some(commit) = commit else {
        return Ok(files);
    };
    let tree = repository
        .find_commit(commit)
        .and_then(|commit| commit.tree())
        .map_err(unavailable)?;
    let mut failure = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |directory, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            match repository.find_blob(entry.id()) {
                Ok(blob) => {
                    files.insert(
                        format!("{directory}{}", entry.name().unwrap_or_default()),
                        blob.content().to_vec(),
                    );
                }
                Err(error) => {
                    failure = Some(error);
                    return git2::TreeWalkResult::Abort;
                }
            }
        }
        git2::TreeWalkResult::Ok
    })
    .map_err(unavailable)?;
    match failure {
        Some(error) => Err(unavailable(error)),
        None => Ok(files),
    }
}

/// The files beneath `directory` (empty for the root), with the directory
/// prefix removed.
pub fn beneath(files: &Files, directory: &str) -> Files {
    let prefix = prefix(directory);
    files
        .iter()
        .filter_map(|(path, bytes)| {
            path.strip_prefix(&prefix)
                .map(|relative| (relative.to_owned(), bytes.clone()))
        })
        .collect()
}

/// `directory/` for a non-empty directory, else the empty string.
pub fn prefix(directory: &str) -> String {
    let directory = directory.trim_matches('/');
    if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/")
    }
}

/// Write `files` as a tree and commit it with `parents`. Returns `None`
/// when the tree equals the first parent's, so an operation that changes
/// nothing writes nothing.
pub fn commit(
    repository: &Repository,
    signer: Option<&super::CommitSigner>,
    files: &Files,
    message: &str,
    parents: &[Oid],
    allow_empty: bool,
) -> StateResult<Option<Oid>> {
    let tree = write_tree(repository, files)?;
    if !allow_empty {
        if let Some(parent) = parents.first() {
            let parent_tree = repository.find_commit(*parent).map_err(unavailable)?.tree_id();
            if parent_tree == tree {
                return Ok(None);
            }
        }
    }
    let tree = repository.find_tree(tree).map_err(unavailable)?;
    let signature = signature(signer)?;
    let parents = parents
        .iter()
        .map(|parent| repository.find_commit(*parent).map_err(unavailable))
        .collect::<StateResult<Vec<_>>>()?;
    let parents = parents.iter().collect::<Vec<_>>();
    repository
        .commit(None, &signature, &signature, message, &tree, &parents)
        .map(Some)
        .map_err(unavailable)
}

/// The signer's identity at the injected clock's time, in UTC. A read-only
/// store has no signer and refuses to commit.
fn signature(signer: Option<&super::CommitSigner>) -> StateResult<Signature<'static>> {
    let Some(signer) = signer else {
        return Err(nyl_core::orchestration::ports::StateError::Unavailable(
            "the state store was opened read-only, without a commit identity".to_owned(),
        ));
    };
    let time = git2::Time::new(signer.clock.now().timestamp(), 0);
    Signature::new(&signer.identity.name, &signer.identity.email, &time)
        .map(|signature| signature.to_owned())
        .map_err(unavailable)
}

fn write_tree(repository: &Repository, files: &Files) -> StateResult<Oid> {
    #[derive(Default)]
    struct Directory {
        files: BTreeMap<String, Oid>,
        directories: BTreeMap<String, Directory>,
    }
    fn insert(directory: &mut Directory, path: &str, blob: Oid) {
        match path.split_once('/') {
            Some((name, rest)) => insert(directory.directories.entry(name.to_owned()).or_default(), rest, blob),
            None => {
                directory.files.insert(path.to_owned(), blob);
            }
        }
    }
    fn write(repository: &Repository, directory: &Directory) -> StateResult<Oid> {
        let mut builder = repository.treebuilder(None).map_err(unavailable)?;
        for (name, blob) in &directory.files {
            builder.insert(name, *blob, 0o100_644).map_err(unavailable)?;
        }
        for (name, child) in &directory.directories {
            let tree = write(repository, child)?;
            builder.insert(name, tree, 0o040_000).map_err(unavailable)?;
        }
        builder.write().map_err(unavailable)
    }
    let mut root = Directory::default();
    for (path, bytes) in files {
        let blob = repository.blob(bytes).map_err(unavailable)?;
        insert(&mut root, path, blob);
    }
    write(repository, &root)
}
