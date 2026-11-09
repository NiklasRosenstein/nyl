//! Argo CD resource hooks: which documents of a Release are hooks, when each
//! one runs, and what happens to it afterwards.
//!
//! Contract: [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler),
//! applying. `nyl apply`, `nyl release rollback`, `nyl diff`, and the direct
//! reconciler read hooks through this module alone, so a Release is applied
//! the way Argo CD syncs the same manifests, whichever reconciler runs it. The
//! rules follow Argo CD's `argocd.argoproj.io/hook` handling (gitops-engine
//! `pkg/sync/hook`, `pkg/sync/sync_tasks.go`, `pkg/sync/syncwaves`):
//!
//! - **Phase.** `argocd.argoproj.io/hook` is a comma-separated list of `PreSync`,
//!   `Sync`, `PostSync`, `SyncFail`, and `Skip`; a document runs once in each phase
//!   it names. Without a recognised Argo CD hook type, Helm's `helm.sh/hook`
//!   decides: `pre-install` and `pre-upgrade` run in `PreSync`, `post-install` and
//!   `post-upgrade` in `PostSync`. An Argo CD hook annotation wins over the Helm
//!   one, which is then ignored whole. `crd-install` is no hook: Argo CD applies
//!   such a document like any other. `Skip` alone is never applied. A hook with no
//!   sync phase (Helm's `test`, `pre-delete`, `post-delete`, `pre-rollback`,
//!   `post-rollback`, Argo CD's `PreDelete` and `PostDelete`) is left out of the
//!   apply and reported in [`ReleasePlan::ignored`].
//! - **Order.** Phases run `PreSync`, `Sync`, `PostSync`; `SyncFail` only after a
//!   failure. Within a phase, resources and hooks run by wave
//!   (`argocd.argoproj.io/sync-wave`, else `helm.sh/hook-weight`, else 0), then kind
//!   ([`ResourceOrdering`]), then name.
//! - **Dependencies.** A Namespace, and a CustomResourceDefinition, applies no later than
//!   the first document that needs it, hooks included ([`resource_slots`]), as
//!   Argo CD's `adjustDeps` moves one to the phase and wave of its first dependent.
//! - **Delete policy.** `argocd.argoproj.io/hook-delete-policy` and
//!   `helm.sh/hook-delete-policy` are both read and merged: `HookSucceeded` /
//!   `hook-succeeded`, `HookFailed` / `hook-failed`, `BeforeHookCreation` /
//!   `before-hook-creation`. With none, `BeforeHookCreation` applies.
//!
//! This module is pure: it decides from documents. [`super::lifecycle`] performs
//! the effects.

use std::cmp::Ordering;

use serde_json::Value;

use super::{ResourceKey, ResourceOrdering};
use crate::Result;

pub(crate) const SYNC_WAVE: &str = "argocd.argoproj.io/sync-wave";
pub(crate) const HOOK: &str = "argocd.argoproj.io/hook";
pub(crate) const HOOK_DELETE_POLICY: &str = "argocd.argoproj.io/hook-delete-policy";
pub(crate) const HELM_HOOK: &str = "helm.sh/hook";
pub(crate) const HELM_HOOK_WEIGHT: &str = "helm.sh/hook-weight";
pub(crate) const HELM_HOOK_DELETE_POLICY: &str = "helm.sh/hook-delete-policy";
/// The finalizer that keeps a hook object until the sync has seen how it ended, as
/// Argo CD's `argocd.argoproj.io/hook-finalizer` does: a Job a TTL controller deletes
/// the moment it finishes is still readable until the sync removes the finalizer.
pub const HOOK_FINALIZER: &str = "argocd.argoproj.io/hook-finalizer";

/// A sync phase a hook runs in. Variants sort in the order the phases run, with
/// `SyncFail` last because it runs only after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookPhase {
    PreSync,
    Sync,
    PostSync,
    SyncFail,
}

impl HookPhase {
    /// The name Argo CD gives the phase.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreSync => "PreSync",
            Self::Sync => "Sync",
            Self::PostSync => "PostSync",
            Self::SyncFail => "SyncFail",
        }
    }
}

impl std::fmt::Display for HookPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// When a hook object is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeletePolicy {
    /// After the sync succeeded.
    HookSucceeded,
    /// After the sync failed.
    HookFailed,
    /// Before the hook is created again, which waits until it is gone.
    BeforeHookCreation,
}

impl DeletePolicy {
    /// The name Argo CD gives the policy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HookSucceeded => "HookSucceeded",
            Self::HookFailed => "HookFailed",
            Self::BeforeHookCreation => "BeforeHookCreation",
        }
    }
}

impl std::fmt::Display for DeletePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a document is to the apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// An ordinary resource: applied, tracked, pruned, and compared.
    Resource,
    /// A hook that runs in these phases.
    Hook(HookSpec),
    /// `argocd.argoproj.io/hook: Skip`: never applied.
    Skip,
    /// A hook with no sync phase; the text says what it declares.
    Ignored(String),
}

/// A hook's phases and delete policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    /// The phases it runs in, each once, in phase order.
    pub phases: Vec<HookPhase>,
    /// The policies in effect: never empty, `BeforeHookCreation` when the
    /// document declares none.
    pub delete_policies: Vec<DeletePolicy>,
}

/// The annotations of a document that are strings.
fn annotation<'a>(manifest: &'a Value, key: &str) -> Option<&'a str> {
    manifest.pointer("/metadata/annotations")?.get(key)?.as_str()
}

/// The values of a comma-separated annotation: trimmed, without blanks and
/// repeats, as Argo CD reads them.
fn csv<'a>(manifest: &'a Value, key: &str) -> Vec<&'a str> {
    let mut values: Vec<&str> = Vec::new();
    for item in annotation(manifest, key).unwrap_or_default().split(',') {
        let item = item.trim();
        if !item.is_empty() && !values.contains(&item) {
            values.push(item);
        }
    }
    values
}

/// One value of `argocd.argoproj.io/hook`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgoType {
    Phase(HookPhase),
    Skip,
}

fn argo_type(value: &str) -> Option<ArgoType> {
    Some(match value {
        "PreSync" => ArgoType::Phase(HookPhase::PreSync),
        "Sync" => ArgoType::Phase(HookPhase::Sync),
        "PostSync" => ArgoType::Phase(HookPhase::PostSync),
        "SyncFail" => ArgoType::Phase(HookPhase::SyncFail),
        "Skip" => ArgoType::Skip,
        _ => return None,
    })
}

fn helm_phase(value: &str) -> Option<HookPhase> {
    match value {
        "pre-install" | "pre-upgrade" => Some(HookPhase::PreSync),
        "post-install" | "post-upgrade" => Some(HookPhase::PostSync),
        _ => None,
    }
}

fn argo_policy(value: &str) -> Option<DeletePolicy> {
    match value {
        "HookSucceeded" => Some(DeletePolicy::HookSucceeded),
        "HookFailed" => Some(DeletePolicy::HookFailed),
        "BeforeHookCreation" => Some(DeletePolicy::BeforeHookCreation),
        _ => None,
    }
}

fn helm_policy(value: &str) -> Option<DeletePolicy> {
    match value {
        "hook-succeeded" => Some(DeletePolicy::HookSucceeded),
        "hook-failed" => Some(DeletePolicy::HookFailed),
        "before-hook-creation" => Some(DeletePolicy::BeforeHookCreation),
        _ => None,
    }
}

/// The delete policies of a hook: the Argo CD ones, then the Helm ones, none
/// twice; `BeforeHookCreation` when there are none.
fn delete_policies(manifest: &Value) -> Vec<DeletePolicy> {
    let mut policies: Vec<DeletePolicy> = Vec::new();
    let declared = csv(manifest, HOOK_DELETE_POLICY)
        .into_iter()
        .filter_map(argo_policy)
        .chain(
            csv(manifest, HELM_HOOK_DELETE_POLICY)
                .into_iter()
                .filter_map(helm_policy),
        );
    for policy in declared {
        if !policies.contains(&policy) {
            policies.push(policy);
        }
    }
    if policies.is_empty() {
        policies.push(DeletePolicy::BeforeHookCreation);
    }
    policies
}

/// Whether `manifest` declares a hook that runs when a Release is removed:
/// Argo CD's `PreDelete` and `PostDelete`, Helm's `pre-delete` and `post-delete`.
fn is_delete_hook(manifest: &Value) -> bool {
    csv(manifest, HOOK)
        .iter()
        .any(|value| matches!(*value, "PreDelete" | "PostDelete"))
        || csv(manifest, HELM_HOOK)
            .iter()
            .any(|value| matches!(*value, "pre-delete" | "post-delete"))
}

/// The delete hooks among `manifests`: documents Argo CD runs when it removes an
/// Application, which Nyl does not run when it removes a Release.
pub fn delete_hooks(manifests: &[Value]) -> Result<Vec<IgnoredHook>> {
    let mut hooks = Vec::new();
    for manifest in manifests {
        if let Classification::Ignored(reason) = classify(manifest) {
            if is_delete_hook(manifest) {
                hooks.push(IgnoredHook {
                    key: ResourceKey::from_json_value(manifest)?,
                    reason,
                    delete: true,
                });
            }
        }
    }
    Ok(hooks)
}

/// The keys of the hooks among `manifests` that a sync creates, each once. No
/// revision tracks them, so a purge reads them from the revision's manifest.
pub fn hook_keys(manifests: &[Value]) -> Result<Vec<ResourceKey>> {
    let mut keys: Vec<ResourceKey> = Vec::new();
    for manifest in manifests {
        if matches!(classify(manifest), Classification::Hook(_)) {
            let key = ResourceKey::from_json_value(manifest)?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    Ok(keys)
}

/// What `manifest` is to the apply. Pure; the one reading of the hook
/// annotations.
pub fn classify(manifest: &Value) -> Classification {
    let has_argo = annotation(manifest, HOOK).is_some();
    let mut types: Vec<ArgoType> = csv(manifest, HOOK).into_iter().filter_map(argo_type).collect();
    // An Argo CD hook annotation with a recognised value wins; Helm's is read otherwise.
    if types.is_empty() {
        types = csv(manifest, HELM_HOOK)
            .into_iter()
            .filter_map(helm_phase)
            .map(ArgoType::Phase)
            .collect();
    }
    if types == [ArgoType::Skip] {
        return Classification::Skip;
    }
    let helm = annotation(manifest, HELM_HOOK);
    // Helm marks CRDs with the same annotation and Argo CD applies them as resources.
    let is_hook = has_argo || helm.is_some_and(|value| value != "crd-install");
    if !is_hook {
        return Classification::Resource;
    }
    let mut phases: Vec<HookPhase> = Vec::new();
    for argo in types {
        if let ArgoType::Phase(phase) = argo {
            if !phases.contains(&phase) {
                phases.push(phase);
            }
        }
    }
    phases.sort_unstable();
    if phases.is_empty() {
        let declares = match (annotation(manifest, HOOK), helm) {
            (Some(value), _) => format!("{HOOK}: {value}"),
            (None, Some(value)) => format!("{HELM_HOOK}: {value}"),
            (None, None) => unreachable!("a hook declares an annotation"),
        };
        let reason = if is_delete_hook(manifest) {
            format!("it declares {declares}, a delete hook, and Nyl runs no hooks when it removes a Release")
        } else {
            format!("it declares {declares}, which names no sync phase")
        };
        return Classification::Ignored(reason);
    }
    Classification::Hook(HookSpec {
        phases,
        delete_policies: delete_policies(manifest),
    })
}

/// The sync wave of a document: `argocd.argoproj.io/sync-wave` when it is an
/// integer, else `helm.sh/hook-weight` when it is one, else 0. Argo CD reads the
/// Helm weight of any document, hook or not.
pub fn wave(manifest: &Value) -> i64 {
    let parse = |key| annotation(manifest, key).and_then(|text| text.parse::<i64>().ok());
    parse(SYNC_WAVE).or_else(|| parse(HELM_HOOK_WEIGHT)).unwrap_or(0)
}

/// One run of a hook: a hook that names several phases has one task per phase.
#[derive(Debug, Clone, PartialEq)]
pub struct HookTask {
    pub manifest: Value,
    pub key: ResourceKey,
    pub phase: HookPhase,
    pub wave: i64,
    pub delete_policies: Vec<DeletePolicy>,
}

impl HookTask {
    /// Whether the object is deleted by `policy`.
    pub fn has_policy(&self, policy: DeletePolicy) -> bool {
        self.delete_policies.contains(&policy)
    }
}

/// A hook left out of the apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredHook {
    pub key: ResourceKey,
    pub reason: String,
    /// Whether it runs when Argo CD removes an Application (a delete hook), which
    /// Nyl does not do for a Release. Only those are worth a warning: the rest (Helm's
    /// `test`, the rollback hooks) have no effect in a sync either way.
    pub delete: bool,
}

/// The stable code of the warning for delete hooks that Nyl does not run.
pub const IGNORED_CODE: nyl_core::orchestration::codes::Code =
    nyl_core::orchestration::codes::Code::KubernetesHookIgnored;

/// How to fix [`IGNORED_CODE`].
pub const IGNORED_FIX: &str =
    "Nyl never runs delete hooks, so do what the hook does yourself before the Release is removed \
     (for example `kubectl create -f` of its Job, then wait for it); to stop the warning, drop the hook from the \
     Release, or mark it `argocd.argoproj.io/hook: Skip`, which leaves it out of Argo CD's sync as well";

/// The warning for the delete hooks among `ignored`, once for the Release: `None`
/// when there is none. Hooks without a sync phase that are no delete hook (Helm's
/// `test`, `pre-rollback`, `post-rollback`) are left out silently, as Argo CD leaves
/// them out.
pub fn delete_hook_finding(ignored: &[IgnoredHook]) -> Option<nyl_core::orchestration::codes::Finding> {
    let delete: Vec<&IgnoredHook> = ignored.iter().filter(|hook| hook.delete).collect();
    if delete.is_empty() {
        return None;
    }
    let listed = delete
        .iter()
        .map(|hook| format!("{} ({})", hook.key, hook.reason))
        .collect::<Vec<_>>()
        .join("; ");
    Some(nyl_core::orchestration::codes::Finding::new(
        IGNORED_CODE,
        format!(
            "{} delete hook(s) are not run: Nyl runs no hooks when it removes a Release, and applies none: {listed}",
            delete.len()
        ),
        IGNORED_FIX,
    ))
}

/// A Release's documents divided into what the apply does with each.
#[derive(Debug, Clone, Default)]
pub struct ReleasePlan {
    /// Documents applied, tracked, pruned, and compared as ordinary resources,
    /// in the order given.
    pub resources: Vec<Value>,
    /// Every run of every hook, in the order they run: phase, wave, kind, name.
    pub hooks: Vec<HookTask>,
    /// Hooks with no sync phase.
    pub ignored: Vec<IgnoredHook>,
    /// Documents marked `Skip`.
    pub skipped: Vec<ResourceKey>,
}

impl ReleasePlan {
    /// Divide `manifests`.
    pub fn of(manifests: &[Value]) -> Result<Self> {
        let mut plan = Self::default();
        for manifest in manifests {
            match classify(manifest) {
                Classification::Resource => plan.resources.push(manifest.clone()),
                Classification::Skip => plan.skipped.push(ResourceKey::from_json_value(manifest)?),
                Classification::Ignored(reason) => plan.ignored.push(IgnoredHook {
                    key: ResourceKey::from_json_value(manifest)?,
                    reason,
                    delete: is_delete_hook(manifest),
                }),
                Classification::Hook(spec) => {
                    let key = ResourceKey::from_json_value(manifest)?;
                    let wave = wave(manifest);
                    for phase in spec.phases {
                        plan.hooks.push(HookTask {
                            manifest: manifest.clone(),
                            key: key.clone(),
                            phase,
                            wave,
                            delete_policies: spec.delete_policies.clone(),
                        });
                    }
                }
            }
        }
        plan.hooks.sort_by(compare_hooks);
        Ok(plan)
    }

    /// Keys of the documents the release does not track: hooks, hooks left out,
    /// and skipped documents. An earlier revision may have tracked one as an
    /// ordinary resource; none is pruned for that.
    pub fn untracked_keys(&self) -> Vec<ResourceKey> {
        let mut keys: Vec<ResourceKey> = Vec::new();
        let all = self
            .hooks
            .iter()
            .map(|hook| &hook.key)
            .chain(self.ignored.iter().map(|hook| &hook.key))
            .chain(&self.skipped);
        for key in all {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        keys
    }

    /// The hooks that run in `phase`.
    pub fn hooks_in(&self, phase: HookPhase) -> impl Iterator<Item = &HookTask> {
        self.hooks.iter().filter(move |hook| hook.phase == phase)
    }
}

/// Phase, wave, kind, then name (and namespace, so the order is total).
fn compare_hooks(a: &HookTask, b: &HookTask) -> Ordering {
    (a.phase, a.wave)
        .cmp(&(b.phase, b.wave))
        .then_with(|| ResourceOrdering::compare_kinds(&a.key.gvk, &b.key.gvk))
        .then_with(|| (&a.key.name, &a.key.namespace).cmp(&(&b.key.name, &b.key.namespace)))
}

/// The `(phase, wave)` step each of `resources` applies in, in the order given.
///
/// An ordinary resource applies in the `Sync` phase at its own wave, except a
/// Namespace and a CustomResourceDefinition, which move to the step of their first
/// dependent when it is earlier, whether the dependent is a resource or a hook: a
/// document in the Namespace (`metadata.namespace`), or one of the kind the CRD
/// defines (`spec.group` and `spec.names.kind`). That is Argo CD's `adjustDeps`
/// (`sync_tasks.go`), and what keeps a custom resource of an early wave from
/// applying before its CRD, or a `PreSync` hook before its Namespace. A step is
/// never moved later, and a `SyncFail` hook, which runs only after a failure,
/// needs nothing applied before it.
pub fn resource_slots(resources: &[Value], hooks: &[HookTask]) -> Vec<(HookPhase, i64)> {
    let own: Vec<(HookPhase, i64)> = resources
        .iter()
        .map(|manifest| (HookPhase::Sync, wave(manifest)))
        .collect();
    // Every document that can depend on another, with the step it runs in.
    let documents: Vec<(&Value, (HookPhase, i64))> = resources
        .iter()
        .zip(&own)
        .map(|(manifest, step)| (manifest, *step))
        .chain(
            hooks
                .iter()
                .filter(|hook| hook.phase != HookPhase::SyncFail)
                .map(|hook| (&hook.manifest, (hook.phase, hook.wave))),
        )
        .collect();
    resources
        .iter()
        .zip(own)
        .map(|(manifest, step)| {
            let needed_by = |depends: &dyn Fn(&Value) -> bool| {
                documents
                    .iter()
                    .filter(|(document, _)| !std::ptr::eq(*document, manifest) && depends(document))
                    .map(|(_, step)| *step)
                    .min()
            };
            let earliest = match dependency(manifest) {
                Some(Dependency::Namespace(name)) => needed_by(&|document| {
                    document.pointer("/metadata/namespace").and_then(Value::as_str) == Some(name.as_str())
                }),
                Some(Dependency::Kind { group, kind }) => needed_by(&|document| {
                    api_group(document) == Some(group.as_str())
                        && document.get("kind").and_then(Value::as_str) == Some(kind.as_str())
                }),
                None => None,
            };
            earliest.map_or(step, |earliest| earliest.min(step))
        })
        .collect()
}

/// What a document provides that others depend on.
enum Dependency {
    /// A Namespace, by name.
    Namespace(String),
    /// A CustomResourceDefinition's kind.
    Kind { group: String, kind: String },
}

fn api_group(manifest: &Value) -> Option<&str> {
    let api_version = manifest.get("apiVersion")?.as_str()?;
    Some(api_version.split_once('/').map_or("", |(group, _)| group))
}

fn dependency(manifest: &Value) -> Option<Dependency> {
    let kind = manifest.get("kind")?.as_str()?;
    match (api_group(manifest)?, kind) {
        ("", "Namespace") => Some(Dependency::Namespace(
            manifest.pointer("/metadata/name")?.as_str()?.to_owned(),
        )),
        ("apiextensions.k8s.io", "CustomResourceDefinition") => Some(Dependency::Kind {
            group: manifest.pointer("/spec/group")?.as_str()?.to_owned(),
            kind: manifest.pointer("/spec/names/kind")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

/// The documents of `manifests` the apply tracks, prunes, and compares: every
/// one that is not a hook (or skipped). A document without an identity is
/// reported by the caller's own key parsing.
pub fn is_resource(manifest: &Value) -> bool {
    matches!(classify(manifest), Classification::Resource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(kind: &str, name: &str, annotations: Value) -> Value {
        json!({
            "apiVersion": if kind == "Job" { "batch/v1" } else { "v1" },
            "kind": kind,
            "metadata": {"name": name, "namespace": "app", "annotations": annotations},
        })
    }

    fn hook(annotations: Value) -> Classification {
        classify(&document("Job", "job", annotations))
    }

    fn phases(annotations: Value) -> Vec<HookPhase> {
        match hook(annotations) {
            Classification::Hook(spec) => spec.phases,
            other => panic!("not a hook: {other:?}"),
        }
    }

    /// Catches a hook phase being read wrongly, which would run a PreSync check
    /// after the workload it guards.
    #[test]
    fn test_classify_reads_argo_cd_hook_phases() {
        assert_eq!(phases(json!({HOOK: "PreSync"})), [HookPhase::PreSync]);
        assert_eq!(
            phases(json!({HOOK: "PostSync, PreSync ,Sync,PreSync"})),
            [HookPhase::PreSync, HookPhase::Sync, HookPhase::PostSync],
            "comma separated, each phase once, in phase order"
        );
        assert_eq!(phases(json!({HOOK: "SyncFail"})), [HookPhase::SyncFail]);
        assert_eq!(phases(json!({HOOK: "Skip,PostSync"})), [HookPhase::PostSync]);
    }

    /// Catches Helm hooks being applied as ordinary resources, which was the
    /// behavior before hooks had phases.
    #[test]
    fn test_classify_maps_helm_hooks_onto_sync_phases() {
        for (helm, expected) in [
            ("pre-install", HookPhase::PreSync),
            ("pre-upgrade", HookPhase::PreSync),
            ("post-install", HookPhase::PostSync),
            ("post-upgrade", HookPhase::PostSync),
        ] {
            assert_eq!(phases(json!({HELM_HOOK: helm})), [expected], "{helm}");
        }
        assert_eq!(
            phases(json!({HELM_HOOK: "pre-install, pre-upgrade, post-install"})),
            [HookPhase::PreSync, HookPhase::PostSync],
            "a phase named twice runs once"
        );
    }

    /// Catches a Helm annotation overriding an Argo CD one, which Argo CD ignores
    /// once an Argo CD hook type is recognised, and the Helm one being ignored
    /// when the Argo CD value names nothing it knows.
    #[test]
    fn test_classify_prefers_the_argo_cd_hook_annotation() {
        assert_eq!(
            phases(json!({HOOK: "PostSync", HELM_HOOK: "pre-install"})),
            [HookPhase::PostSync]
        );
        assert_eq!(
            phases(json!({HOOK: "Nonsense", HELM_HOOK: "pre-install"})),
            [HookPhase::PreSync]
        );
    }

    /// Catches `Skip` applying a document, a CRD hook being held back, and a
    /// plain resource being taken for a hook.
    #[test]
    fn test_classify_skip_crd_install_and_plain_resources() {
        assert_eq!(hook(json!({HOOK: "Skip"})), Classification::Skip);
        assert_eq!(hook(json!({HELM_HOOK: "crd-install"})), Classification::Resource);
        assert_eq!(hook(json!({"team": "web"})), Classification::Resource);
        assert_eq!(classify(&json!({"kind": "ConfigMap"})), Classification::Resource);
    }

    /// Catches hooks without a sync phase being applied during a sync, which
    /// would run a Helm test against a half-applied Release, and a reason that
    /// misleads: a delete hook says Nyl runs none when it removes a Release.
    #[test]
    fn test_classify_ignores_hooks_without_a_sync_phase() {
        let no_phase = |declares: &str| format!("it declares {declares}, which names no sync phase");
        let delete = |declares: &str| {
            format!("it declares {declares}, a delete hook, and Nyl runs no hooks when it removes a Release")
        };
        for (annotations, expected) in [
            (json!({HELM_HOOK: "test"}), no_phase("helm.sh/hook: test")),
            (
                json!({HELM_HOOK: "pre-rollback"}),
                no_phase("helm.sh/hook: pre-rollback"),
            ),
            (
                json!({HELM_HOOK: "post-rollback"}),
                no_phase("helm.sh/hook: post-rollback"),
            ),
            (json!({HOOK: "Nonsense"}), no_phase("argocd.argoproj.io/hook: Nonsense")),
            (json!({HELM_HOOK: "pre-delete"}), delete("helm.sh/hook: pre-delete")),
            (json!({HELM_HOOK: "post-delete"}), delete("helm.sh/hook: post-delete")),
            (json!({HOOK: "PreDelete"}), delete("argocd.argoproj.io/hook: PreDelete")),
            (
                json!({HOOK: "PostDelete"}),
                delete("argocd.argoproj.io/hook: PostDelete"),
            ),
        ] {
            assert_eq!(
                hook(annotations.clone()),
                Classification::Ignored(expected),
                "{annotations}"
            );
        }
        // One phase Argo CD knows is enough; the rest is dropped silently.
        assert_eq!(phases(json!({HELM_HOOK: "pre-install,test"})), [HookPhase::PreSync]);
    }

    /// Catches the delete-hook warning naming a Helm test hook, which every
    /// `helm create` chart declares and no fix can remove from a vendor chart, and
    /// a delete hook going unreported.
    #[test]
    fn test_only_delete_hooks_are_warned_about() {
        let documents = [
            document("Job", "post-delete", json!({HELM_HOOK: "post-delete"})),
            document("Job", "pre-delete", json!({HOOK: "PreDelete"})),
            document("Job", "test", json!({HELM_HOOK: "test"})),
            document("Job", "plain", json!({})),
        ];
        let names: Vec<String> = delete_hooks(&documents)
            .unwrap()
            .into_iter()
            .map(|hook| hook.key.name)
            .collect();
        assert_eq!(names, ["post-delete", "pre-delete"]);

        let plan = ReleasePlan::of(&documents).unwrap();
        let finding = delete_hook_finding(&plan.ignored).expect("delete hooks are warned about");
        assert_eq!(finding.code, IGNORED_CODE);
        assert!(
            finding.message.starts_with("2 delete hook(s) are not run")
                && finding.message.contains("Job app/post-delete")
                && finding.message.contains("Job app/pre-delete")
                && !finding.message.contains("Job app/test"),
            "{}",
            finding.message
        );
        assert_eq!(finding.fix, IGNORED_FIX);

        let only_test = ReleasePlan::of(&documents[2..]).unwrap();
        assert_eq!(only_test.ignored.len(), 1, "a test hook is still left out of the apply");
        assert!(delete_hook_finding(&only_test.ignored).is_none(), "but silently");
    }

    /// Catches a purge missing the hooks of a Release, which no revision tracks,
    /// and one deleting a hook that never ran.
    #[test]
    fn test_hook_keys_are_the_hooks_a_sync_creates() {
        let documents = [
            document("Job", "migrate", json!({HOOK: "PreSync,PostSync"})),
            document("Job", "install", json!({HELM_HOOK: "pre-install"})),
            document("Job", "test", json!({HELM_HOOK: "test"})),
            document("Job", "skipped", json!({HOOK: "Skip"})),
            document("ConfigMap", "plain", json!({})),
        ];

        let names: Vec<String> = hook_keys(&documents).unwrap().into_iter().map(|key| key.name).collect();

        assert_eq!(names, ["migrate", "install"], "each once, in manifest order");
    }

    fn step_of(documents: &[Value], hooks: &[Value]) -> Vec<(HookPhase, i64)> {
        let plan = ReleasePlan::of(&[documents, hooks].concat()).unwrap();
        resource_slots(&plan.resources, &plan.hooks)
    }

    fn namespace(name: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name}})
    }

    fn widget_crd() -> Value {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.example.com"},
            "spec": {"group": "example.com", "names": {"kind": "Widget"}}
        })
    }

    fn widget(wave: &str) -> Value {
        json!({"apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "w", "namespace": "app", "annotations": {SYNC_WAVE: wave}}})
    }

    /// Catches a Namespace or CRD staying in a later wave than a document that
    /// needs it, which fails that document and breaks the CRD, Namespace, rest
    /// order: Argo CD moves each to the step of its first dependent.
    #[test]
    fn test_a_namespace_and_a_crd_move_to_the_step_of_their_first_dependent() {
        let documents = [
            namespace("app"),
            widget_crd(),
            document("ConfigMap", "cfg", json!({})),
            widget("-1"),
        ];

        let steps = step_of(&documents, &[]);

        assert_eq!(
            steps,
            [
                (HookPhase::Sync, -1),
                (HookPhase::Sync, -1),
                (HookPhase::Sync, 0),
                (HookPhase::Sync, -1)
            ],
            "the Namespace and the CRD follow the custom resource of wave -1"
        );
    }

    /// Catches a dependency moving a Namespace or CRD later, and one that does
    /// not exist moving it at all: Argo CD only ever moves one earlier.
    #[test]
    fn test_a_namespace_and_a_crd_never_move_later_or_for_a_stranger() {
        let late = json!({"apiVersion": "v1", "kind": "Namespace",
            "metadata": {"name": "app", "annotations": {SYNC_WAVE: "-3"}}});
        let documents = [
            late,
            widget_crd(),
            widget("5"),
            document("ConfigMap", "elsewhere", json!({})),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "c", "namespace": "other"}}),
        ];

        let steps = step_of(&documents, &[]);

        assert_eq!(steps[0], (HookPhase::Sync, -3), "its own earlier wave stays");
        assert_eq!(
            steps[1],
            (HookPhase::Sync, 0),
            "the custom resource of wave 5 needs it later"
        );
        assert_eq!(steps[2], (HookPhase::Sync, 5));
    }

    /// Catches a `PreSync` hook running before the Namespace it runs in: the
    /// Namespace moves into the hook's phase, and a `SyncFail` hook, which runs
    /// only after a failure, moves nothing.
    #[test]
    fn test_a_namespace_moves_ahead_of_a_hook_that_needs_it() {
        let hooks = [document("Job", "migrate", json!({HOOK: "PreSync", SYNC_WAVE: "2"}))];
        assert_eq!(
            step_of(&[namespace("app")], &hooks),
            [(HookPhase::PreSync, 2)],
            "the Namespace applies with the PreSync hook that runs in it"
        );

        let failure = [document("Job", "cleanup", json!({HOOK: "SyncFail"}))];
        assert_eq!(step_of(&[namespace("app")], &failure), [(HookPhase::Sync, 0)]);

        let after = [document("Job", "notify", json!({HOOK: "PostSync"}))];
        assert_eq!(
            step_of(&[namespace("app")], &after),
            [(HookPhase::Sync, 0)],
            "a later phase is no reason to move"
        );
    }

    fn policies(mut annotations: Value) -> Vec<DeletePolicy> {
        annotations[HOOK] = json!("PreSync");
        match hook(annotations) {
            Classification::Hook(spec) => spec.delete_policies,
            other => panic!("not a hook: {other:?}"),
        }
    }

    /// Catches a hook without a policy being kept after it ran, which would let
    /// a completed Job block its own recreation, and a Helm policy being ignored.
    #[test]
    fn test_delete_policies_default_to_before_hook_creation_and_merge_both_vocabularies() {
        assert_eq!(policies(json!({})), [DeletePolicy::BeforeHookCreation]);
        assert_eq!(
            policies(json!({HOOK_DELETE_POLICY: "HookSucceeded, HookFailed"})),
            [DeletePolicy::HookSucceeded, DeletePolicy::HookFailed]
        );
        assert_eq!(
            policies(json!({HELM_HOOK_DELETE_POLICY: "hook-succeeded,before-hook-creation"})),
            [DeletePolicy::HookSucceeded, DeletePolicy::BeforeHookCreation]
        );
        assert_eq!(
            policies(json!({HOOK_DELETE_POLICY: "HookFailed", HELM_HOOK_DELETE_POLICY: "hook-failed"})),
            [DeletePolicy::HookFailed],
            "both vocabularies are read, a policy once"
        );
        assert_eq!(
            policies(json!({HOOK_DELETE_POLICY: "Nonsense"})),
            [DeletePolicy::BeforeHookCreation],
            "an unknown policy is no policy"
        );
    }

    /// Catches a wave being read from the wrong annotation: the Helm weight only
    /// stands in for a missing or invalid Argo CD wave.
    #[test]
    fn test_wave_prefers_the_argo_cd_wave_over_the_helm_weight() {
        let wave_of = |annotations| wave(&document("Job", "job", annotations));
        assert_eq!(wave_of(json!({})), 0);
        assert_eq!(wave_of(json!({SYNC_WAVE: "-3"})), -3);
        assert_eq!(wave_of(json!({SYNC_WAVE: "2", HELM_HOOK_WEIGHT: "9"})), 2);
        assert_eq!(wave_of(json!({HELM_HOOK_WEIGHT: "5"})), 5);
        assert_eq!(wave_of(json!({SYNC_WAVE: "soon", HELM_HOOK_WEIGHT: "5"})), 5);
        assert_eq!(wave_of(json!({SYNC_WAVE: "soon"})), 0);
    }

    /// Catches hooks running out of order: phase, then wave, then kind, then name.
    #[test]
    fn test_plan_orders_hooks_by_phase_wave_kind_and_name() {
        let manifests = vec![
            document("Job", "z-post", json!({HOOK: "PostSync"})),
            document("Job", "b-pre", json!({HOOK: "PreSync", SYNC_WAVE: "1"})),
            document("Job", "a-pre", json!({HOOK: "PreSync", SYNC_WAVE: "1"})),
            document("ConfigMap", "cfg-pre", json!({HOOK: "PreSync", SYNC_WAVE: "1"})),
            document(
                "Job",
                "early-pre",
                json!({HELM_HOOK: "pre-install", HELM_HOOK_WEIGHT: "-1"}),
            ),
            document("Job", "both", json!({HOOK: "PostSync,PreSync", SYNC_WAVE: "7"})),
            document("Job", "fail", json!({HOOK: "SyncFail"})),
            document("ConfigMap", "plain", json!({})),
        ];

        let plan = ReleasePlan::of(&manifests).unwrap();

        let order: Vec<(HookPhase, i64, &str)> = plan
            .hooks
            .iter()
            .map(|task| (task.phase, task.wave, task.key.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                (HookPhase::PreSync, -1, "early-pre"),
                (HookPhase::PreSync, 1, "cfg-pre"),
                (HookPhase::PreSync, 1, "a-pre"),
                (HookPhase::PreSync, 1, "b-pre"),
                (HookPhase::PreSync, 7, "both"),
                (HookPhase::PostSync, 0, "z-post"),
                (HookPhase::PostSync, 7, "both"),
                (HookPhase::SyncFail, 0, "fail"),
            ]
        );
        assert_eq!(plan.resources.len(), 1);
    }

    /// Catches skipped and ignored hooks leaking into the apply, and a hook with
    /// no identity being dropped silently.
    #[test]
    fn test_plan_leaves_out_skipped_and_ignored_hooks() {
        let manifests = vec![
            document("Job", "skipped", json!({HOOK: "Skip"})),
            document("Job", "test", json!({HELM_HOOK: "test"})),
            document("ConfigMap", "plain", json!({})),
        ];

        let plan = ReleasePlan::of(&manifests).unwrap();

        assert_eq!(plan.resources.len(), 1);
        assert!(plan.hooks.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.ignored.len(), 1);
        assert!(
            delete_hook_finding(&plan.ignored).is_none(),
            "a hook without a sync phase that is no delete hook is left out without a warning"
        );
        assert!(ReleasePlan::of(&[json!({"metadata": {"annotations": {HOOK: "PreSync"}}})]).is_err());
    }
}
