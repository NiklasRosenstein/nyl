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

impl UpdateError {
    /// The state error for this failure, `stale` for a ref that was not at
    /// its expected value.
    pub fn or_stale(self, stale: StateError) -> StateError {
        match self {
            Self::Stale => stale,
            Self::Other(error) => error,
        }
    }
}

impl From<git2::Error> for UpdateError {
    fn from(error: git2::Error) -> Self {
        Self::Other(unavailable(error))
    }
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

    pub fn baseline(&self, name: &str) -> String {
        self.tracking(name)
            .replacen("refs/nyl-tracking/", "refs/nyl-baseline/", 1)
    }

    pub fn baseline_oid(&self, name: &str) -> Option<Oid> {
        self.repository.refname_to_id(&self.baseline(name)).ok()
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
                        // Keep tracking what Nyl last saw, so every later
                        // snapshot refuses the rewrite too.
                        for (name, before) in &previous {
                            self.set_tracking(name, *before)?;
                        }
                        return Err(StateError::NotFastForward {
                            reference: (*name).clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Refuse to discard coordination records written offline.
    pub fn check_local_coordination(&self, prefix: &str) -> StateResult<()> {
        let pattern = format!("{prefix}*");
        let mut names = std::collections::BTreeSet::new();
        for reference in self.repository.references_glob(&pattern).map_err(unavailable)? {
            let reference = reference.map_err(unavailable)?;
            if let Ok(name) = reference.name() {
                names.insert(name.to_owned());
            }
        }
        let tracked = self.baseline(&pattern);
        let tracking_prefix = self.baseline("refs/");
        for reference in self.repository.references_glob(&tracked).map_err(unavailable)? {
            let reference = reference.map_err(unavailable)?;
            if let Ok(name) = reference.name() {
                if let Some(suffix) = name.strip_prefix(&tracking_prefix) {
                    names.insert(format!("refs/{suffix}"));
                }
            }
        }
        for name in names {
            if self.repository.refname_to_id(&name).ok() != self.baseline_oid(&name) {
                return Err(StateError::Unavailable(format!("local coordination record {name} contains unpublished work; recover it with --local before resetting state")));
            }
        }
        Ok(())
    }

    /// Synchronize the local view only when it still matches the last remote view.
    #[allow(clippy::too_many_lines)] // Keep the ref validation and atomic synchronization together.
    pub fn sync_local(&self, names: &[String], heads: &BTreeMap<String, Oid>) -> StateResult<()> {
        if self.url.is_none() {
            return Ok(());
        }
        let mut all = names
            .iter()
            .filter(|name| !name.contains('*'))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        all.extend(heads.keys().cloned());
        for pattern in names.iter().filter(|name| name.contains('*')) {
            for reference in self.repository.references_glob(pattern).map_err(unavailable)? {
                let reference = reference.map_err(unavailable)?;
                if let Ok(name) = reference.name() {
                    all.insert(name.to_owned());
                }
            }
            let tracking_pattern = self.baseline(pattern);
            let prefix = self.baseline("refs/");
            for reference in self
                .repository
                .references_glob(&tracking_pattern)
                .map_err(unavailable)?
            {
                let reference = reference.map_err(unavailable)?;
                if let Ok(name) = reference.name() {
                    if let Some(suffix) = name.strip_prefix(&prefix) {
                        all.insert(format!("refs/{suffix}"));
                    }
                }
            }
        }
        let mut expected = BTreeMap::new();
        let mut baselines = BTreeMap::new();
        if !self.repository.is_bare() {
            if let Ok(head) = self.repository.head() {
                if head.name().is_ok_and(|name| all.contains(name)) {
                    return Err(StateError::Unavailable(
                        "a configured state ref is checked out; use a separate branch for state".to_owned(),
                    ));
                }
            }
        }
        for name in &all {
            let local = self.repository.refname_to_id(name).ok();
            let baseline = self.baseline_oid(name);
            let remote = heads.get(name).copied();
            if local != baseline && !(baseline.is_none() && local == remote) {
                return Err(StateError::Unavailable(format!(
                    "local state at {} contains unpublished changes to {name}; inspect with --local and use `nyl state reset --local` with the required discard options before using the remote",
                    self.repository.path().display()
                )));
            }
            expected.insert(name.clone(), local);
            baselines.insert(name.clone(), baseline);
        }
        self.fetch(heads, false)?;
        for name in names.iter().filter(|name| !name.contains('*')) {
            if let (Some(Some(before)), Some(after)) = (expected.get(name), heads.get(name)) {
                if before != after
                    && !self
                        .repository
                        .graph_descendant_of(*after, *before)
                        .map_err(unavailable)?
                {
                    for (name, old) in &expected {
                        self.set_tracking(name, *old)?;
                    }
                    return Err(StateError::NotFastForward {
                        reference: name.clone(),
                    });
                }
            }
        }
        let mut transaction = self.repository.transaction().map_err(unavailable)?;
        for name in &all {
            transaction.lock_ref(name).map_err(unavailable)?;
            transaction.lock_ref(&self.baseline(name)).map_err(unavailable)?;
        }
        for name in &all {
            if self.repository.refname_to_id(name).ok() != expected[name] || self.baseline_oid(name) != baselines[name]
            {
                return Err(StateError::conflict(name.clone(), Some("a concurrent local change")));
            }
            match heads.get(name) {
                Some(oid) => transaction
                    .set_target(name, *oid, None, "nyl: synchronize local state")
                    .map_err(unavailable)?,
                None if expected[name].is_some() => transaction.remove(name).map_err(unavailable)?,
                None => {}
            }
            let baseline = self.baseline(name);
            match heads.get(name) {
                Some(oid) => transaction
                    .set_target(&baseline, *oid, None, "nyl: acknowledge state")
                    .map_err(unavailable)?,
                None if baselines[name].is_some() => transaction.remove(&baseline).map_err(unavailable)?,
                None => {}
            }
        }
        transaction.commit().map_err(unavailable)?;
        for name in &all {
            self.set_tracking(name, heads.get(name).copied())?;
        }
        Ok(())
    }

    fn update_mirror(&self, update: &Update) -> Result<(), UpdateError> {
        let baseline = self.baseline(&update.name);
        let mut transaction = self.repository.transaction()?;
        transaction.lock_ref(&update.name)?;
        transaction.lock_ref(&baseline)?;
        if self.repository.refname_to_id(&update.name).ok() != update.expected
            || self.baseline_oid(&update.name) != update.expected
        {
            return Err(UpdateError::Stale);
        }
        match update.new {
            Some(oid) => {
                transaction.set_target(&update.name, oid, None, "nyl: synchronize local state")?;
                transaction.set_target(&baseline, oid, None, "nyl: acknowledge state")?;
            }
            None if update.expected.is_some() => {
                transaction.remove(&update.name)?;
                transaction.remove(&baseline)?;
            }
            None => {}
        }
        transaction.commit()?;
        Ok(())
    }

    /// Apply every update or none.
    pub fn update(&self, updates: &[Update]) -> Result<(), UpdateError> {
        if !self.repository.is_bare() {
            if let Ok(head) = self.repository.head() {
                if head
                    .name()
                    .is_ok_and(|name| updates.iter().any(|update| update.name == name))
                {
                    return Err(UpdateError::Other(StateError::Unavailable(
                        "a configured state ref is checked out; use a separate branch for state".to_owned(),
                    )));
                }
            }
        }
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
                            let baseline = self.baseline_oid(&update.name);
                            let local = self.repository.refname_to_id(&update.name).ok();
                            self.set_tracking(&update.name, update.new)
                                .map_err(UpdateError::Other)?;
                            if local == baseline {
                                let mirror = Update {
                                    name: update.name.clone(),
                                    new: update.new,
                                    expected: baseline,
                                };
                                match self.update_mirror(&mirror) {
                                    Ok(()) | Err(UpdateError::Stale) => {}
                                    Err(error) => return Err(error),
                                }
                            }
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
        if !self.repository.is_bare() {
            if let Ok(head) = self.repository.head() {
                if head
                    .name()
                    .is_ok_and(|name| updates.iter().any(|update| update.name == name))
                {
                    return Err(UpdateError::Other(StateError::Unavailable(
                        "a configured state ref is checked out; use a separate branch for state".to_owned(),
                    )));
                }
            }
        }
        let mut transaction = self.repository.transaction()?;
        for update in updates {
            transaction.lock_ref(&update.name)?;
        }
        for update in updates {
            let current = self.repository.refname_to_id(&update.name).ok();
            let fast_forward = match (current, update.new) {
                (Some(current), Some(new)) => current == new || self.repository.graph_descendant_of(new, current)?,
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
            result?;
        }
        Ok(transaction.commit()?)
    }

    /// The local object a listed head names, after [`Self::fetch`].
    pub fn local(&self, name: &str) -> Option<Oid> {
        self.repository.refname_to_id(&self.tracking(name)).ok()
    }

    /// The local object of `name` when `heads` lists it, after [`Self::fetch`].
    pub fn fetched(&self, heads: &BTreeMap<String, Oid>, name: &str) -> Option<Oid> {
        heads.get(name).and_then(|_| self.local(name))
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
