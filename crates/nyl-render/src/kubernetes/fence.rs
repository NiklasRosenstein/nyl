//! Who may apply, and who may purge, a release that is already in a cluster.
//!
//! Contract: [Direct reconciler](../../../../design/orchestration-core.md#fencing-and-ownership-in-the-cluster).
//! Every revision records the unit incarnation that applied it and the
//! publication destination it was applied for ([`ReleaseMeta`]). A release is
//! fenced like a published prefix: an orchestrated apply takes it over only
//! when its owner is gone, and a manual command refuses an owned release. The
//! decisions here are pure functions over the recorded revisions; asking
//! whether an incarnation still exists is the caller's ([`OwnerState`]), and
//! the compare-and-swap that makes a reservation a claim is the revision
//! create of the recorder.

use super::{ReleaseMeta, ReleasePublication, ReleaseState};
use crate::gitops::reconcile::RenderIndexOwner;
use crate::{NylError, Result};

/// The code of a refusal because another unit's release lives here.
pub const RELEASE_FENCED: &str = "NYL-KUBERNETES-RELEASE-FENCED";
/// The code of a refusal because whether the owner exists cannot be told.
pub const RELEASE_OWNER_UNKNOWN: &str = "NYL-KUBERNETES-RELEASE-OWNER-UNKNOWN";
/// The code of a manual command's refusal of an owned release.
pub const RELEASE_OWNED_BY_UNIT: &str = "NYL-RELEASE-OWNED-BY-UNIT";

/// Whether an incarnation still exists in its environment's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerState {
    Exists,
    Gone,
    /// State cannot say, with why.
    Unknown(String),
}

/// Answers whether the incarnation a release names still exists.
pub type OwnerProbe<'a> = &'a (dyn Fn(&RenderIndexOwner) -> OwnerState + Sync);

/// Who wants to change a release.
pub enum Requester<'a> {
    /// An orchestrated unit incarnation applying for a publication.
    Unit {
        owner: &'a RenderIndexOwner,
        publication: &'a ReleasePublication,
        probe: OwnerProbe<'a>,
    },
    /// A command (`nyl apply`, `release rollback`, `release delete`).
    /// `take_over` is the deliberate way past an owned release.
    Manual { take_over: bool },
}

/// What a requester records in the revision it writes, and the fence it passes.
pub struct ReleaseClaim<'a> {
    pub requester: Requester<'a>,
    /// The publication destination to record for a manual apply with a target.
    pub publication: Option<ReleasePublication>,
}

impl ReleaseClaim<'_> {
    /// An unfenced manual claim that records nothing, as `nyl apply` without a target does.
    pub fn manual() -> ReleaseClaim<'static> {
        ReleaseClaim {
            requester: Requester::Manual { take_over: false },
            publication: None,
        }
    }

    /// The metadata of the revision this claim writes.
    pub fn meta(&self) -> ReleaseMeta {
        match &self.requester {
            Requester::Unit { owner, publication, .. } => ReleaseMeta {
                owner: Some((*owner).clone()),
                publication: Some((*publication).clone()),
            },
            Requester::Manual { .. } => ReleaseMeta {
                owner: None,
                publication: self.publication.clone().map(|publication| ReleasePublication {
                    commit: None,
                    ..publication
                }),
            },
        }
    }
}

/// A refusal, with its stable code and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fenced {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for Fenced {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl From<Fenced> for NylError {
    fn from(fenced: Fenced) -> Self {
        NylError::OwnerFenced(fenced.to_string())
    }
}

/// What a claim that passed asks the writer to say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Admitted {
    /// A warning to show: the release was adopted from an older `nyl`.
    pub warning: Option<String>,
}

fn name(namespace: &str, release: &str) -> String {
    format!("release {release} in namespace {namespace}")
}

/// Decide whether `claim` may write the next revision of a release, given
/// its `latest` recorded revision and the owner recorded by the newest
/// earlier revision that has one (`earlier_owner`).
pub fn check_claim(
    namespace: &str,
    release: &str,
    latest: Option<&ReleaseState>,
    earlier_owner: Option<&RenderIndexOwner>,
    claim: &ReleaseClaim<'_>,
) -> std::result::Result<Admitted, Fenced> {
    let Some(latest) = latest else {
        return Ok(Admitted::default());
    };
    let what = name(namespace, release);
    match &claim.requester {
        Requester::Manual { take_over } => match &latest.meta.owner {
            Some(owner) if !take_over => Err(Fenced {
                code: RELEASE_OWNED_BY_UNIT,
                message: format!(
                    "{what} is owned by {owner}, which this command cannot tell is gone; changing it by hand would be \
                     reverted by that unit's next reconcile. Pass --take-over to record no owner and change it anyway, \
                     or tear the unit down or release it with `nyl state forget`"
                ),
            }),
            _ => Ok(Admitted::default()),
        },
        Requester::Unit {
            owner,
            publication,
            probe,
        } => match &latest.meta.owner {
            Some(recorded) if recorded == *owner => Ok(Admitted::default()),
            Some(recorded) => match probe(recorded) {
                OwnerState::Gone => Ok(Admitted::default()),
                OwnerState::Exists => Err(Fenced {
                    code: RELEASE_FENCED,
                    message: format!(
                        "{what} is owned by {recorded}, which still exists in its environment's state; it stays fenced \
                         until that unit's teardown completes or it is released with `nyl state forget`"
                    ),
                }),
                OwnerState::Unknown(reason) => Err(Fenced {
                    code: RELEASE_OWNER_UNKNOWN,
                    message: format!(
                        "{what} is owned by {recorded}, and whether it still exists cannot be checked: {reason}. If the \
                         environment was renamed or removed, declare it again and tear the unit down, or release the \
                         unit with `nyl state forget`"
                    ),
                }),
            },
            None => match &latest.meta.publication {
                Some(recorded) if recorded.same_destination(publication) => Ok(Admitted::default()),
                Some(recorded) => Err(Fenced {
                    code: RELEASE_FENCED,
                    message: format!(
                        "{what} was applied for another publication ({}@{} {:?}), which no unit owns; apply it from \
                         there, or remove it with `nyl release delete` if it is stale",
                        recorded.repository, recorded.branch, recorded.path_prefix
                    ),
                }),
                None if latest.meta.is_empty() && earlier_owner == Some(*owner) => Ok(Admitted {
                    warning: Some(format!(
                        "{what} has a revision that records no owner after revisions this unit owned: an older nyl \
                         applied it, and it is adopted"
                    )),
                }),
                None => Err(Fenced {
                    code: RELEASE_FENCED,
                    message: format!(
                        "{what} was applied without a publication this unit can match, so it may belong to another \
                         target; apply it with `nyl apply --target <target>` first, or remove it with `nyl release delete` \
                         if it is stale"
                    ),
                }),
            },
        },
    }
}

/// Whether a unit may purge a vanished release whose latest revision records
/// `recorded`: it owns it, or the owner is an earlier incarnation of the same
/// unit that is gone.
pub fn may_purge(recorded: Option<&RenderIndexOwner>, requester: &RenderIndexOwner, probe: OwnerProbe<'_>) -> bool {
    match recorded {
        Some(recorded) if recorded == requester => true,
        Some(RenderIndexOwner::Unit { environment, unit, .. }) => match requester {
            RenderIndexOwner::Unit {
                environment: own_environment,
                unit: own_unit,
                ..
            } => environment == own_environment && unit == own_unit && probe(recorded.unwrap()) == OwnerState::Gone,
            RenderIndexOwner::Catalog { .. } => false,
        },
        _ => false,
    }
}

/// The owner the newest of `revisions` that records one names, newest first.
pub fn earlier_owner(revisions: &[ReleaseState]) -> Option<RenderIndexOwner> {
    revisions.iter().rev().find_map(|revision| revision.meta.owner.clone())
}

/// Read the state `claim` needs from `storage` and decide, before anything is
/// written: the latest revision, and the newest earlier owner.
pub async fn admit(
    storage: &dyn super::ReleaseStorage,
    namespace: &str,
    release: &str,
    claim: &ReleaseClaim<'_>,
) -> Result<Admitted> {
    let revisions = storage.list_revisions(release, namespace).await?;
    let mut recorded = Vec::new();
    for revision in &revisions {
        if let Some(state) = storage.get_release(release, namespace, *revision).await? {
            recorded.push(state);
        }
    }
    recorded.sort_by_key(|state| state.revision);
    let latest = recorded.last();
    let earlier = latest.map(|latest| {
        recorded
            .iter()
            .filter(|state| state.revision < latest.revision)
            .cloned()
            .collect::<Vec<_>>()
    });
    let earlier_owner = earlier.as_deref().and_then(earlier_owner);
    Ok(check_claim(namespace, release, latest, earlier_owner.as_ref(), claim)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::memory::sample_release;
    use crate::kubernetes::ReleaseStatus;

    fn unit(uid: &str) -> RenderIndexOwner {
        RenderIndexOwner::unit("dev", "kubernetes", uid)
    }

    fn publication(prefix: &str) -> ReleasePublication {
        ReleasePublication {
            repository: "https://example.invalid/deploy.git".to_owned(),
            branch: "deploy".to_owned(),
            path_prefix: prefix.to_owned(),
            commit: None,
        }
    }

    fn revision(owner: Option<RenderIndexOwner>, publication: Option<ReleasePublication>) -> ReleaseState {
        let mut state = sample_release("web", "web", 1, ReleaseStatus::Deployed);
        state.meta = ReleaseMeta { owner, publication };
        state
    }

    fn check(
        latest: Option<&ReleaseState>,
        earlier: Option<&RenderIndexOwner>,
        own: &RenderIndexOwner,
        state: OwnerState,
    ) -> std::result::Result<Admitted, Fenced> {
        let publication = publication("targets/dev");
        let probe = move |_: &RenderIndexOwner| state.clone();
        let claim = ReleaseClaim {
            requester: Requester::Unit {
                owner: own,
                publication: &publication,
                probe: &probe,
            },
            publication: None,
        };
        check_claim("web", "web", latest, earlier, &claim)
    }

    /// Catches a unit being refused a release it already owns, and one being
    /// refused its first revision.
    #[test]
    fn test_a_unit_creates_and_continues_its_own_release() {
        let own = unit("a");
        assert!(check(None, None, &own, OwnerState::Exists).is_ok());
        let latest = revision(Some(own.clone()), Some(publication("targets/dev")));
        assert!(check(Some(&latest), None, &own, OwnerState::Exists).is_ok());
    }

    /// Catches two units sharing one release: another existing owner fences,
    /// an unreadable state fails closed, and a gone owner is taken over.
    #[test]
    fn test_another_owner_fences_until_it_is_gone() {
        let own = unit("a");
        let latest = revision(Some(unit("b")), Some(publication("targets/dev")));
        let fenced = check(Some(&latest), None, &own, OwnerState::Exists).unwrap_err();
        assert_eq!(fenced.code, RELEASE_FENCED);
        let unknown = check(
            Some(&latest),
            None,
            &own,
            OwnerState::Unknown("state unreadable".into()),
        )
        .unwrap_err();
        assert_eq!(unknown.code, RELEASE_OWNER_UNKNOWN);
        assert!(check(Some(&latest), None, &own, OwnerState::Gone).is_ok());
    }

    /// Catches adoption by a bare name: an unowned release is adopted only
    /// when it was applied for this publication's destination.
    #[test]
    fn test_adoption_keys_on_the_recorded_publication() {
        let own = unit("a");
        let same = revision(None, Some(publication("targets/dev")));
        assert!(check(Some(&same), None, &own, OwnerState::Exists).is_ok());
        let other = revision(None, Some(publication("targets/prod")));
        assert_eq!(
            check(Some(&other), None, &own, OwnerState::Exists).unwrap_err().code,
            RELEASE_FENCED
        );
        let bare = revision(None, None);
        assert_eq!(
            check(Some(&bare), None, &own, OwnerState::Exists).unwrap_err().code,
            RELEASE_FENCED
        );
    }

    /// Catches a release an older nyl touched being refused: after an owned
    /// revision, a revision without metadata is adopted with a warning.
    #[test]
    fn test_a_metadata_less_revision_after_an_owned_one_is_adopted_with_a_warning() {
        let own = unit("a");
        let bare = revision(None, None);
        let admitted = check(Some(&bare), Some(&own), &own, OwnerState::Exists).unwrap();
        assert!(admitted.warning.is_some());
    }

    /// Catches a manual command changing a release an environment owns, and
    /// `--take-over` as the one deliberate way past.
    #[test]
    fn test_a_manual_command_refuses_an_owned_release_unless_it_takes_over() {
        let latest = revision(Some(unit("a")), None);
        let refused = |take_over| {
            check_claim(
                "web",
                "web",
                Some(&latest),
                None,
                &ReleaseClaim {
                    requester: Requester::Manual { take_over },
                    publication: None,
                },
            )
        };
        assert_eq!(refused(false).unwrap_err().code, RELEASE_OWNED_BY_UNIT);
        assert!(refused(true).is_ok());
        let unowned = revision(None, None);
        assert!(check_claim("web", "web", Some(&unowned), None, &ReleaseClaim::manual()).is_ok());
    }

    /// Catches a purge of another unit's release, and the purge of an earlier
    /// incarnation of the same unit.
    #[test]
    fn test_purge_requires_the_owner_or_a_gone_incarnation_of_the_same_unit() {
        let own = unit("a");
        let gone = |_: &RenderIndexOwner| OwnerState::Gone;
        let exists = |_: &RenderIndexOwner| OwnerState::Exists;
        assert!(may_purge(Some(&own), &own, &exists));
        assert!(may_purge(Some(&unit("old")), &own, &gone));
        assert!(!may_purge(Some(&unit("old")), &own, &exists));
        assert!(!may_purge(
            Some(&RenderIndexOwner::unit("prod", "kubernetes", "z")),
            &own,
            &gone
        ));
        assert!(!may_purge(None, &own, &gone));
    }
}
