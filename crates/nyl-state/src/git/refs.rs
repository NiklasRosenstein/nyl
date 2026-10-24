//! Reading and updating refs of a state repository, remote or local.
//!
//! A remote repository is reached through `nyl-git`: refs are listed with
//! `ls-remote`, fetched into a tracking namespace of the clone, and updated
//! with atomic pushes that carry a lease per ref. Without a remote, the
//! clone's own refs are the state, and updates take the same compare-and-swap
//! form through a libgit2 ref transaction. Objects always live in the clone's
//! repository.

use std::collections::BTreeMap;

use git2::{Oid, Repository};
use nyl_core::orchestration::ports::{StateError, StateResult};
use nyl_git::{Expect, GitAuth, RefUpdate};

/// The `repository` of a state location that is the clone itself.
pub const LOCAL_REPOSITORY: &str = nyl_core::orchestration::state::StateLocation::LOCAL_REPOSITORY;

/// One ref update, a compare-and-swap: the ref must be at `expected`, or
/// absent for `None`; `new` of `None` deletes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub name: String,
    pub new: Option<Oid>,
    pub expected: Option<Oid>,
}

/// Why an update set did not apply.
#[derive(Debug)]
pub enum UpdateError {
    /// A ref was not at its expected value; nothing changed.
    Stale,
    Other(StateError),
}

/// The refs of one state repository.
pub struct Refs<'a> {
    pub repository: &'a Repository,
    /// The read URL; `None` for the clone's own refs.
    pub url: Option<&'a str>,
    /// The write URL, such as a GitRepository's `publishURL`.
    pub push_url: Option<&'a str>,
    pub auth: &'a GitAuth,
}

pub fn unavailable(error: impl std::fmt::Display) -> StateError {
    StateError::Unavailable(error.to_string())
}

/// The full name of a ref as configured: a short name is a branch.
pub fn full_name(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

impl Refs<'_> {
    fn tracking(&self, name: &str) -> String {
        match self.url {
            Some(url) => {
                let digest = nyl_core::digest::sha256_hex(url.as_bytes());
                format!(
                    "refs/nyl-tracking/{}/{}",
                    &digest[..16],
                    name.strip_prefix("refs/").unwrap_or(name)
                )
            }
            None => name.to_owned(),
        }
    }

    /// The commits of the named refs and globs, by full ref name.
    pub fn heads(&self, patterns: &[String]) -> StateResult<BTreeMap<String, Oid>> {
        if let Some(url) = self.url {
            let patterns = patterns.iter().map(String::as_str).collect::<Vec<_>>();
            return nyl_git::ls_remote(self.repository.path(), url, &patterns, self.auth)
                .map_err(unavailable)?
                .into_iter()
                .map(|(name, oid)| Ok((name, Oid::from_str(&oid).map_err(unavailable)?)))
                .collect();
        }
        let mut heads = BTreeMap::new();
        for pattern in patterns {
            let references = if pattern.contains('*') {
                self.repository.references_glob(pattern).map_err(unavailable)?
            } else {
                match self.repository.find_reference(pattern) {
                    Ok(reference) => {
                        if let Some(target) = reference.target() {
                            heads.insert(pattern.clone(), target);
                        }
                        continue;
                    }
                    Err(error) if error.code() == git2::ErrorCode::NotFound => continue,
                    Err(error) => return Err(unavailable(error)),
                }
            };
            for reference in references {
                let reference = reference.map_err(unavailable)?;
                if let (Ok(name), Some(target)) = (reference.name(), reference.target()) {
                    heads.insert(name.to_owned(), target);
                }
            }
        }
        Ok(heads)
    }

    /// Make the objects of `heads` available locally. A tracked state ref
    /// that moved other than forward is corruption: Nyl never rewrites one.
    pub fn fetch(&self, heads: &BTreeMap<String, Oid>, fast_forward_only: bool) -> StateResult<()> {
        let Some(url) = self.url else {
            return Ok(());
        };
        let missing = heads
            .iter()
            .filter(|(name, oid)| self.repository.refname_to_id(&self.tracking(name)).ok() != Some(**oid))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        let refspecs = missing
            .iter()
            .map(|(name, _)| format!("+{name}:{}", self.tracking(name)))
            .collect::<Vec<_>>();
        let previous = missing
            .iter()
            .map(|(name, _)| (name.as_str(), self.repository.refname_to_id(&self.tracking(name)).ok()))
            .collect::<BTreeMap<_, _>>();
        let refspecs = refspecs.iter().map(String::as_str).collect::<Vec<_>>();
        nyl_git::fetch(self.repository.path(), url, &refspecs, false, self.auth).map_err(unavailable)?;
        if fast_forward_only {
            for (name, oid) in &missing {
                if let Some(Some(before)) = previous.get(name.as_str()) {
                    let forward = *before == **oid
                        || self
                            .repository
                            .graph_descendant_of(**oid, *before)
                            .map_err(unavailable)?;
                    if !forward {
                        return Err(StateError::NotFastForward {
                            reference: (*name).clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply every update or none.
    pub fn update(&self, updates: &[Update]) -> Result<(), UpdateError> {
        match self.push_url.or(self.url) {
            Some(url) => {
                let pushes = updates
                    .iter()
                    .map(|update| RefUpdate {
                        remote: update.name.clone(),
                        new: update.new.map(|oid| oid.to_string()),
                        expected: Expect::at(update.expected.map(|oid| oid.to_string()).as_deref()),
                    })
                    .collect::<Vec<_>>();
                match nyl_git::push_atomic(self.repository.path(), url, &pushes, self.auth) {
                    Ok(()) => {
                        for update in updates {
                            self.set_tracking(&update.name, update.new)
                                .map_err(UpdateError::Other)?;
                        }
                        Ok(())
                    }
                    Err(error) if error.is_stale() => Err(UpdateError::Stale),
                    Err(error) => Err(UpdateError::Other(unavailable(error))),
                }
            }
            None => self.update_local(updates),
        }
    }

    fn set_tracking(&self, name: &str, oid: Option<Oid>) -> StateResult<()> {
        let tracking = self.tracking(name);
        match oid {
            Some(oid) => self
                .repository
                .reference(&tracking, oid, true, "nyl: track state")
                .map(drop)
                .map_err(unavailable),
            None => match self.repository.find_reference(&tracking) {
                Ok(mut reference) => reference.delete().map_err(unavailable),
                Err(_) => Ok(()),
            },
        }
    }

    fn update_local(&self, updates: &[Update]) -> Result<(), UpdateError> {
        let mut transaction = self
            .repository
            .transaction()
            .map_err(|error| UpdateError::Other(unavailable(error)))?;
        for update in updates {
            transaction
                .lock_ref(&update.name)
                .map_err(|error| UpdateError::Other(unavailable(error)))?;
        }
        for update in updates {
            let current = self.repository.refname_to_id(&update.name).ok();
            let fast_forward = match (current, update.new) {
                (Some(current), Some(new)) => {
                    current == new
                        || self
                            .repository
                            .graph_descendant_of(new, current)
                            .map_err(|error| UpdateError::Other(unavailable(error)))?
                }
                _ => true,
            };
            let stale = update.expected != current || !fast_forward;
            if stale {
                return Err(UpdateError::Stale);
            }
        }
        for update in updates {
            let result = if let Some(new) = update.new {
                transaction.set_target(&update.name, new, None, "nyl: state")
            } else if self.repository.refname_to_id(&update.name).is_err() {
                continue;
            } else {
                transaction.remove(&update.name)
            };
            result.map_err(|error| UpdateError::Other(unavailable(error)))?;
        }
        transaction
            .commit()
            .map_err(|error| UpdateError::Other(unavailable(error)))
    }

    /// The local object a listed head names, after [`Self::fetch`].
    pub fn local(&self, name: &str) -> Option<Oid> {
        self.repository.refname_to_id(&self.tracking(name)).ok()
    }

    /// Fast-forward the clone's branch of a state ref to `oid`, creating it
    /// when missing. A diverged branch is left alone; no upstream is set.
    pub fn fast_forward_local_branch(&self, name: &str, oid: Oid) -> StateResult<()> {
        if self.url.is_none() || !name.starts_with("refs/heads/") {
            return Ok(());
        }
        match self.repository.refname_to_id(name) {
            Ok(current) if current == oid => Ok(()),
            Ok(current) => {
                if self.repository.graph_descendant_of(oid, current).map_err(unavailable)? {
                    self.repository
                        .reference_matching(name, oid, true, current, "nyl: fast-forward local state")
                        .map(drop)
                        .map_err(unavailable)
                } else {
                    Ok(())
                }
            }
            Err(_) => self
                .repository
                .reference(name, oid, false, "nyl: local state")
                .map(drop)
                .map_err(unavailable),
        }
    }
}
