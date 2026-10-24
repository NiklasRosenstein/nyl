//! Whether `nyl apply` may prune a resource, and `nyl release delete` delete
//! it, by the Argo CD sync options it carries, so a direct command deletes what
//! an Argo CD sync or Application deletion deletes.
//!
//! Managed Namespaces carry `Prune=confirm` and `Delete=confirm` by default
//! (their group's `namespace.prunePolicy` and `deletePolicy`), because deleting a
//! Namespace deletes everything in it.

use std::collections::BTreeMap;

/// The annotation holding a resource's Argo CD sync options.
pub const SYNC_OPTIONS_ANNOTATION: &str = "argocd.argoproj.io/sync-options";

/// What a command does with a resource it would delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneDecision {
    /// Delete it.
    Prune,
    /// `false`: leave it in the cluster and stop tracking it.
    Retain,
    /// `confirm` without confirmation: leave it and keep tracking it, so a
    /// confirmed run deletes it.
    AwaitConfirmation,
}

/// Decide pruning from the live resource's annotations by its `Prune` sync
/// option; `confirmed` is `--confirm-prune`.
pub fn prune_decision(annotations: Option<&BTreeMap<String, String>>, confirmed: bool) -> PruneDecision {
    decision(annotations, "Prune", confirmed)
}

/// Decide deletion with the release from the live resource's annotations by its
/// `Delete` sync option; `confirmed` is `--confirm-delete`.
pub fn delete_decision(annotations: Option<&BTreeMap<String, String>>, confirmed: bool) -> PruneDecision {
    decision(annotations, "Delete", confirmed)
}

fn decision(annotations: Option<&BTreeMap<String, String>>, option: &str, confirmed: bool) -> PruneDecision {
    let value = annotations
        .and_then(|annotations| annotations.get(SYNC_OPTIONS_ANNOTATION))
        .into_iter()
        .flat_map(|options| options.split(','))
        .filter_map(|entry| entry.trim().split_once('='))
        .find(|(name, _)| name.trim() == option)
        .map(|(_, value)| value.trim().to_ascii_lowercase());
    match value.as_deref() {
        Some("false") => PruneDecision::Retain,
        Some("confirm") if !confirmed => PruneDecision::AwaitConfirmation,
        _ => PruneDecision::Prune,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotations(options: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(SYNC_OPTIONS_ANNOTATION.to_owned(), options.to_owned())])
    }

    #[test]
    fn test_prune_decision_follows_argocd_prune_sync_option() {
        assert_eq!(prune_decision(None, false), PruneDecision::Prune);
        assert_eq!(
            prune_decision(Some(&annotations("ServerSideApply=true")), false),
            PruneDecision::Prune
        );
        assert_eq!(
            prune_decision(Some(&annotations("Delete=confirm, Prune=false")), true),
            PruneDecision::Retain
        );
        assert_eq!(
            prune_decision(Some(&annotations("Prune=confirm")), false),
            PruneDecision::AwaitConfirmation
        );
        assert_eq!(
            prune_decision(Some(&annotations("Prune=confirm")), true),
            PruneDecision::Prune
        );
    }

    #[test]
    fn test_delete_decision_reads_the_delete_sync_option() {
        let options = annotations("Prune=false,Delete=confirm");
        assert_eq!(delete_decision(Some(&options), false), PruneDecision::AwaitConfirmation);
        assert_eq!(delete_decision(Some(&options), true), PruneDecision::Prune);
    }
}
