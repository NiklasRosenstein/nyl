//! Deleting resources in reverse apply order, the one deletion path behind
//! `nyl apply` pruning and `nyl argocd purge`.

use std::future::Future;
use std::time::Duration;

use futures::stream::{self, StreamExt};

use super::{DeleteOptions, KubeClient, ResourceKey, ResourceOrdering};

/// How long one deletion run waits in total for deleted resources to terminate
/// before deleting the resources they may depend on.
pub const PRUNE_TERMINATION_TIMEOUT: Duration = Duration::from_mins(2);
const PRUNE_TERMINATION_POLL: Duration = Duration::from_secs(1);

/// Progress of [`delete_in_prune_order`].
#[derive(Debug)]
pub enum PruneEvent<'a> {
    /// Deletion of `count` resources starts.
    Started { count: usize },
    /// One resource's deletion request completed.
    Deleted {
        key: &'a ResourceKey,
        result: &'a crate::Result<()>,
    },
    /// Deleted resources are still terminating; the next batch waits for them.
    Waiting { count: usize },
    /// The termination wait ran out; deletion continues without waiting.
    WaitTimedOut { keys: &'a [&'a ResourceKey] },
    /// Every batch was handled.
    Finished,
}

/// Run `op` for every item, at most `concurrency` at a time, reporting each result
/// to `on_done` as it completes.
pub async fn run_concurrently<T, R, Fut>(
    items: impl IntoIterator<Item = T>,
    concurrency: usize,
    op: impl Fn(T) -> Fut,
    mut on_done: impl FnMut(T, R),
) where
    T: Copy,
    Fut: Future<Output = R>,
{
    let op = &op;
    let mut running = stream::iter(items)
        .map(|item| async move { (item, op(item).await) })
        .buffer_unordered(concurrency.max(1));
    while let Some((item, result)) = running.next().await {
        on_done(item, result);
    }
}

/// Delete resources, at most `concurrency` at a time, and return the keys that
/// failed to delete.
///
/// Resources are deleted in reverse apply order ([`ResourceOrdering::prune_batches`]),
/// dependents first; the resources of one kind are deleted concurrently. Before the
/// next batch, deletion waits until the deleted resources are gone, so a resource
/// whose finalizer needs its controller, CRD, or webhook is finalized before those
/// are deleted. Once [`PRUNE_TERMINATION_TIMEOUT`] has passed, resources still
/// terminating are reported and deletion continues without waiting. A resource whose
/// version is no longer served is deleted through another served version; one whose
/// whole kind is gone ([`KubeClient::is_kind_removed`]), such as a custom resource
/// whose CRD was deleted, is already deleted.
pub async fn delete_in_prune_order<'k>(
    client: &dyn KubeClient,
    mut keys: Vec<&'k ResourceKey>,
    options: &DeleteOptions,
    concurrency: usize,
    on_event: &mut (dyn FnMut(PruneEvent<'_>) + Send),
) -> Vec<&'k ResourceKey> {
    let mut failed = Vec::new();
    if keys.is_empty() {
        return failed;
    }
    on_event(PruneEvent::Started { count: keys.len() });
    // One deadline for the whole run, however many kinds have slow finalizers.
    let deadline = tokio::time::Instant::now() + PRUNE_TERMINATION_TIMEOUT;
    let batches = ResourceOrdering::prune_batches(&mut keys);
    let last = batches.len() - 1;
    for (position, batch) in batches.into_iter().enumerate() {
        let mut deleted = Vec::new();
        run_concurrently(
            keys[batch].iter().copied(),
            concurrency,
            |key: &ResourceKey| async move {
                match client
                    .delete_resource_with(&key.gvk, key.namespace.as_deref(), &key.name, options)
                    .await
                {
                    // Objects of a removed kind are gone with it; anything else, such
                    // as an aggregated API that is down, stays recorded and is retried.
                    Err(err) if err.is_api_resource_not_found_error() => match client.is_kind_removed(&key.gvk).await {
                        Ok(true) => Ok(()),
                        _ => Err(err),
                    },
                    result => result,
                }
            },
            |key, result| {
                on_event(PruneEvent::Deleted { key, result: &result });
                if result.is_ok() {
                    deleted.push(key);
                } else {
                    failed.push(key);
                }
            },
        )
        .await;
        if position < last {
            wait_until_terminated(client, deleted, concurrency, deadline, on_event).await;
        }
    }
    on_event(PruneEvent::Finished);
    failed
}

/// Wait until every deleted resource is gone from the cluster, or the deadline passes.
async fn wait_until_terminated(
    client: &dyn KubeClient,
    mut remaining: Vec<&ResourceKey>,
    concurrency: usize,
    deadline: tokio::time::Instant,
    on_event: &mut (dyn FnMut(PruneEvent<'_>) + Send),
) {
    let mut announced = false;
    loop {
        let mut terminating = Vec::new();
        run_concurrently(
            remaining.iter().copied(),
            concurrency,
            |key: &ResourceKey| async move {
                match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
                    Ok(found) => Ok(found.is_some()),
                    // Gone with its kind, or unreachable while its aggregated API is down.
                    Err(err) if err.is_api_resource_not_found_error() => {
                        client.is_kind_removed(&key.gvk).await.map(|removed| !removed)
                    }
                    Err(err) => Err(err),
                }
            },
            |key, result| match result {
                Ok(true) => terminating.push(key),
                Ok(false) => {}
                // Keep waiting on transient errors; the deadline bounds the wait.
                Err(err) => {
                    tracing::debug!(resource = %key, error = %err, "Checking deleted resource failed");
                    terminating.push(key);
                }
            },
        )
        .await;
        remaining = terminating;
        if remaining.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            on_event(PruneEvent::WaitTimedOut { keys: &remaining });
            return;
        }
        if !announced {
            on_event(PruneEvent::Waiting { count: remaining.len() });
            announced = true;
        }
        tokio::time::sleep(PRUNE_TERMINATION_POLL).await;
    }
}
