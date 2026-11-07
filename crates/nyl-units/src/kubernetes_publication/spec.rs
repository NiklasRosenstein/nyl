//! The declaration of a `KubernetesPublication` unit and what the driver
//! validates about it.
//!
//! The spec is `{target, observe?, teardownWait?, releases?}` plus the fields
//! every unit shares. `target` names a static DeploymentTarget, or declares
//! one inline (`{inline, name?}`), in which case the renderer fills the
//! target's `name` and `sourceFile`. `observe: {}` opts the unit into
//! observing the reconciler of its target (see [`validate_observe`]).
//! `releases` is filled by the renderer from the target's `releaseInputs`,
//! and `teardownSettings` and `reconciler` are recorded by the driver at
//! resolution; none is authored.

use nyl_core::orchestration::driver::WaitStrategy;
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::units::COMMON_UNIT_FIELDS;
use nyl_render::gitops::units::PublicationTarget;
use nyl_render::resources::release_inputs::ReleaseKey;
use serde_json::Value;

/// The fields only this kind has.
const KIND_FIELDS: [&str; 4] = ["target", "observe", "teardownWait", "releases"];

/// Validate a rendered spec, references still as expressions.
pub(super) fn validate(spec: &Value) -> Result<(), String> {
    let spec = spec.as_object().ok_or("spec must be a mapping")?;
    if spec.contains_key("reconciler") {
        return Err(
            "spec.reconciler is recorded from the target's reconciler (its ArgoCDInstance) when the unit is resolved and must not be written by hand; \
             declare the instance's access in its ArgoCDInstance spec.access instead"
                .to_owned(),
        );
    }
    for field in [OBSERVATION_SOURCES, WORKLOAD] {
        if spec.contains_key(field.trim_start_matches('/')) {
            return Err(format!(
                "spec.{} is recorded from the rendered target when the unit is resolved and must not be written by hand; \
                 declare what to read under spec.observations instead",
                field.trim_start_matches('/')
            ));
        }
    }
    if spec.contains_key("sharedCatalogReading") {
        return Err(
            "spec.sharedCatalogReading is recorded from the publication branch when the unit is resolved and must not be written by hand"
                .to_owned(),
        );
    }
    if spec.contains_key("teardownSettings") {
        return Err(
            "spec.teardownSettings is recorded from the rendered target when the unit is resolved and must not be written by hand; \
             change the target's catalogApplication or its Namespaces' deletePolicy instead"
                .to_owned(),
        );
    }
    if let Some(field) = spec
        .keys()
        .find(|field| !KIND_FIELDS.contains(&field.as_str()) && !COMMON_UNIT_FIELDS.contains(&field.as_str()))
    {
        return Err(format!(
            "spec has unknown field {field:?}; a KubernetesPublication has target, observe, and teardownWait besides the fields every unit shares"
        ));
    }
    if spec.get("target").is_none() {
        return Err(
            "spec.target is required: name a static DeploymentTarget, or declare one in spec.target.inline".to_owned(),
        );
    }
    match PublicationTarget::from_spec(&Value::Object(spec.clone())) {
        Ok(PublicationTarget::Static(_)) => {}
        Ok(PublicationTarget::Inline(inline)) => inline.validate().map_err(|error| error.to_string())?,
        Err(message) => return Err(message),
    }
    validate_observe(spec)?;
    observation_sources(&Value::Object(spec.clone()))?;
    if let Some(releases) = spec.get("releases") {
        let releases = releases
            .as_object()
            .ok_or("spec.releases must be a mapping of <group>/<release> keys")?;
        for (key, inputs) in releases {
            ReleaseKey::parse(key).map_err(|_| format!("spec.releases key {key:?} must be <group>/<release>"))?;
            if !inputs.is_object() {
                return Err(format!("spec.releases.{key} must be a mapping of input names"));
            }
        }
    }
    Ok(())
}

/// The pointer, in the resolved spec, of the Application that owns each
/// observed resource, recorded at resolution.
pub(super) const OBSERVATION_SOURCES: &str = "/observationSources";

/// The pointer, in the resolved spec, of the workload Cluster the observed
/// resources are read from, recorded at resolution.
pub(super) const WORKLOAD: &str = "/workload";

/// What one observation reads: a field of a resource the target renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservationSource {
    pub resource: nyl_render::gitops::observation::ResourceRef,
    /// A JSON Pointer into the live resource.
    pub pointer: String,
}

/// The `from` sources of `spec.observations`, by observation name:
/// `{resource: {apiVersion, kind, namespace?, name}, pointer}`. Nothing here
/// needs the rendered target; whether the target renders the resource is
/// checked when the unit is resolved. Secrets are never read.
pub(super) fn observation_sources(
    spec: &Value,
) -> Result<std::collections::BTreeMap<String, ObservationSource>, String> {
    let mut sources = std::collections::BTreeMap::new();
    let Some(observations) = spec.get("observations") else {
        return Ok(sources);
    };
    let observations = observations
        .as_object()
        .ok_or("spec.observations must be a mapping of observation names")?;
    for (name, declaration) in observations {
        let field = format!("spec.observations.{name}.from");
        let from = declaration.get("from").and_then(Value::as_object).ok_or_else(|| {
            format!("{field} is required: {{resource: {{apiVersion, kind, namespace, name}}, pointer}}")
        })?;
        if let Some(key) = from.keys().find(|key| !matches!(key.as_str(), "resource" | "pointer")) {
            return Err(format!(
                "{field} has unknown field {key:?}; it has resource and pointer"
            ));
        }
        let resource = from
            .get("resource")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{field}.resource is required: {{apiVersion, kind, namespace, name}}"))?;
        if let Some(key) = resource
            .keys()
            .find(|key| !matches!(key.as_str(), "apiVersion" | "kind" | "namespace" | "name"))
        {
            return Err(format!(
                "{field}.resource has unknown field {key:?}; it has apiVersion, kind, namespace, and name"
            ));
        }
        let text = |key: &str| -> Result<Option<String>, String> {
            match resource.get(key) {
                None => Ok(None),
                Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
                Some(_) => Err(format!("{field}.resource.{key} must be a non-empty string")),
            }
        };
        let require = |key: &str| text(key)?.ok_or_else(|| format!("{field}.resource.{key} is required"));
        let resource = nyl_render::gitops::observation::ResourceRef {
            api_version: require("apiVersion")?,
            kind: require("kind")?,
            namespace: text("namespace")?,
            name: require("name")?,
        };
        if resource.kind == "Secret" && resource.api_version == "v1" {
            return Err(format!(
                "{field}.resource is a Secret, which an observation never reads: observed values are recorded in state and referenced by other units"
            ));
        }
        let pointer = match from.get("pointer") {
            Some(Value::String(pointer)) => pointer.clone(),
            _ => return Err(format!("{field}.pointer is required: a JSON Pointer into the resource, such as /status/loadBalancer/ingress/0/ip")),
        };
        nyl_core::json_pointer::validate(&format!("{field}.pointer"), &pointer).map_err(|error| error.to_string())?;
        if pointer.is_empty() {
            return Err(format!(
                "{field}.pointer selects the whole resource, which an observation never reads: observed values are committed in plain text to the state repository, so select one non-secret field, such as /status/loadBalancer/ingress/0/ip"
            ));
        }
        sources.insert(name.clone(), ObservationSource { resource, pointer });
    }
    Ok(sources)
}

/// The opt-in to observing the reconciler, and what depends on it:
///
/// - `observe: {}` makes the unit declare the attestations `accepted` and
///   `healthy`, lets it use the `observe` `teardownWait` strategy, and makes
///   `observe` its default. It is an object so that options can be added
///   later; only `{}` is accepted now. It says nothing about which
///   reconciler runs: that follows the reconciler of the unit's target.
/// - `teardownWait: {strategy: observe}` reads the reconciler, so it needs
///   `observe: {}`. So do `observations`, which are read once the reconciler
///   shows the owning Application running the current publication: the unit
///   declares the `observe` capability only with the flag, and the engine
///   rejects `observations` on a unit without it (`NYL-OBSERVATIONS-UNSUPPORTED`).
///
/// Shared with the test kind `FakePublication`.
pub(crate) fn validate_observe(spec: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(observe) = spec.get("observe") {
        let options = observe
            .as_object()
            .ok_or_else(|| "spec.observe must be a mapping, written `observe: {}`".to_owned())?;
        if let Some(key) = options.keys().find(|key| key.as_str() != "waitHealthy") {
            return Err(format!(
                "spec.observe has unknown field {key:?}; it has waitHealthy, or write `observe: {{}}`"
            ));
        }
        if let Some(wait) = options.get("waitHealthy") {
            validate_wait_healthy(wait)?;
        }
    }
    if let Some(wait) = spec.get("teardownWait") {
        validate_teardown_wait(wait)?;
        if wait.get("strategy").and_then(Value::as_str) == Some("observe") && !spec.contains_key("observe") {
            return Err(
                "spec.teardownWait.strategy observe reads the reconciler of the target, which a unit opts into with \
                 `observe: {}`: add it, or choose strategy delay or manual"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// `observe.waitHealthy`: `{timeout: <duration>}`, how long a reconcile polls
/// the reconciler until the target is healthy, such as `5m`.
fn validate_wait_healthy(value: &Value) -> Result<(), String> {
    let wait = value
        .as_object()
        .ok_or_else(|| "spec.observe.waitHealthy must be a mapping, written `waitHealthy: {timeout: 5m}`".to_owned())?;
    if let Some(key) = wait.keys().find(|key| key.as_str() != "timeout") {
        return Err(format!(
            "spec.observe.waitHealthy has unknown field {key:?}; it has timeout"
        ));
    }
    match wait.get("timeout") {
        Some(Value::String(timeout)) => {
            let timeout = timeout
                .parse::<Duration>()
                .map_err(|error| format!("spec.observe.waitHealthy.timeout: {error}"))?;
            if timeout.as_secs() == 0 {
                return Err("spec.observe.waitHealthy.timeout must be longer than zero, such as 5m".to_owned());
            }
            Ok(())
        }
        _ => Err("spec.observe.waitHealthy.timeout is required, such as 5m".to_owned()),
    }
}

/// How a unit's teardown waits after phase 1, from its stored spec: the
/// declared `teardownWait`, else `observe` for a unit that declares `observe`
/// and `manual` for one that does not. Validation rejects an unknown strategy and a `delay`
/// without a valid `duration` before a document is stored; a stored document
/// that still carries one reads as `manual`, the only strategy that never ends
/// a wait without an operator.
pub(crate) fn teardown_wait(spec: &Value) -> WaitStrategy {
    match spec.pointer("/teardownWait/strategy").and_then(Value::as_str) {
        Some("observe") => WaitStrategy::Observe,
        Some("delay") => spec
            .pointer("/teardownWait/duration")
            .and_then(Value::as_str)
            .and_then(|duration| duration.parse::<Duration>().ok())
            .map_or(WaitStrategy::Manual, |duration| WaitStrategy::Delay { duration }),
        Some(_) => WaitStrategy::Manual,
        None => {
            if spec.get("observe").is_some() {
                WaitStrategy::Observe
            } else {
                WaitStrategy::Manual
            }
        }
    }
}

/// `teardownWait`: `{strategy: observe|delay|manual, duration?}`, where
/// `delay` requires a `duration` like `10m` and no other strategy takes one.
pub(crate) fn validate_teardown_wait(value: &Value) -> Result<(), String> {
    let wait = value
        .as_object()
        .ok_or_else(|| "spec.teardownWait must be a mapping".to_owned())?;
    if let Some(key) = wait.keys().find(|key| !matches!(key.as_str(), "strategy" | "duration")) {
        return Err(format!(
            "spec.teardownWait has unknown field {key:?}; it has strategy and duration"
        ));
    }
    let strategy = wait.get("strategy").and_then(Value::as_str);
    match (strategy, wait.get("duration")) {
        (Some("delay"), Some(Value::String(duration))) => duration
            .parse::<Duration>()
            .map(drop)
            .map_err(|error| format!("spec.teardownWait.duration: {error}")),
        (Some("delay"), _) => Err("spec.teardownWait.duration is required for strategy delay, such as 10m".to_owned()),
        (Some("observe" | "manual"), None) => Ok(()),
        (Some(strategy @ ("observe" | "manual")), Some(_)) => Err(format!(
            "spec.teardownWait.duration applies only to strategy delay, not {strategy}"
        )),
        _ => Err("spec.teardownWait.strategy must be observe, delay, or manual".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn invalid(spec: &Value) -> String {
        validate(spec).expect_err("the spec is invalid")
    }

    #[test]
    fn test_validate_accepts_the_documented_spec() {
        assert!(validate(&json!({
            "target": "dev",
            "observe": {},
            "teardownWait": {"strategy": "delay", "duration": "10m"},
            "timeout": "10m",
            "env": {"passthrough": ["GIT_TOKEN"]},
            "releases": {"web/web": {"image": {"fromUnit": {"unit": "web-image", "artifact": "image"}}}}
        }))
        .is_ok());
        assert!(validate(&json!({"target": "dev"})).is_ok());
    }

    /// Catches a misspelled or unsupported field silently ignored by the
    /// driver, such as `teardownwait`.
    #[test]
    fn test_validate_rejects_unknown_and_driver_owned_fields() {
        assert!(invalid(&json!({"target": "dev", "teardownwait": {}})).contains("unknown field \"teardownwait\""));
        assert!(invalid(&json!({"target": "dev", "teardownSettings": {}})).contains("must not be written by hand"));
    }

    /// Catches a wait for health accepted with no timeout, a zero or malformed
    /// one, or an option the unit does not have.
    #[test]
    fn test_validate_checks_the_wait_for_health() {
        let spec = |wait: Value| json!({"target": "dev", "observe": {"waitHealthy": wait}});
        assert!(validate(&spec(json!({"timeout": "5m"}))).is_ok());
        assert!(invalid(&spec(json!({}))).contains("timeout is required"));
        assert!(invalid(&spec(json!({"timeout": "0s"}))).contains("longer than zero"));
        assert!(invalid(&spec(json!({"timeout": "soon"}))).contains("waitHealthy.timeout"));
        assert!(invalid(&spec(json!({"timeout": "5m", "interval": "1s"}))).contains("unknown field \"interval\""));
        assert!(invalid(&json!({"target": "dev", "observe": {"wait": {}}})).contains("unknown field \"wait\""));
    }

    #[test]
    fn test_validate_requires_a_named_or_inline_target() {
        assert!(invalid(&json!({"observe": {}})).contains("spec.target is required"));
        assert!(invalid(&json!({"target": 1})).contains("spec.target must name"));
        assert!(invalid(&json!({"target": ""})).contains("spec.target must name"));
        assert!(invalid(&json!({"target": {"name": "dev"}})).contains("spec.target must name"));
        assert!(invalid(&json!({"target": {"inline": {}, "other": 1}})).contains("unknown field \"other\""));
    }

    fn inline(spec: &Value) -> Value {
        json!({"target": {"name": "dev-kubernetes", "sourceFile": "nyl/units.yaml", "inline": spec}})
    }

    /// Catches an inline target accepted although it is no DeploymentTarget,
    /// which would fail only when the unit renders, and one the renderer has
    /// not given its name and source file.
    #[test]
    fn test_validate_checks_an_inline_target_like_a_discovered_one() {
        let publication =
            json!({"repository": {"repoURL": "https://example.invalid/deploy.git"}, "revision": "deploy"});
        let reconciler = json!({"argocd": {}});
        assert!(validate(&inline(&json!({"reconciler": reconciler, "publication": publication}))).is_ok());
        assert!(invalid(&inline(
            &json!({"reconciler": reconciler, "publication": publication, "bogus": 1})
        ))
        .contains("bogus"));
        assert!(invalid(&inline(&json!({"reconciler": reconciler}))).contains("publication"));
        // An inline target declares its reconciler like a discovered one.
        assert!(invalid(&inline(&json!({"publication": publication}))).contains("NYL-TARGET-RECONCILER-MISSING"));
        assert!(invalid(&json!({"target": {"inline": {"publication": publication}}})).contains("spec.target.name"));
    }

    /// Catches the observing declarations being rejected (`observe: {}` and
    /// the `observe` strategy it allows work), the opt-in being a flag with
    /// options nobody can use yet, the strategy that reads the reconciler
    /// working without the opt-in, and the fields only the driver records or a
    /// later milestone reads being accepted from an author.
    #[test]
    fn test_validate_accepts_observation_and_rejects_recorded_and_unavailable_fields() {
        assert!(validate(&json!({"target": "dev", "observe": {}})).is_ok());
        assert!(validate(&json!({"target": "dev", "observe": {}, "teardownWait": {"strategy": "observe"}})).is_ok());
        assert!(validate(&json!({"target": "dev", "observe": {}, "teardownWait": {"strategy": "manual"}})).is_ok());
        let without = invalid(&json!({"target": "dev", "teardownWait": {"strategy": "observe"}}));
        assert!(without.contains("`observe: {}`"), "{without}");
        for observe in [json!(true), json!(null), json!("yes"), json!([])] {
            let message = invalid(&json!({"target": "dev", "observe": observe}));
            assert!(
                message.contains("spec.observe must be a mapping"),
                "{observe}: {message}"
            );
        }
        let option = invalid(&json!({"target": "dev", "observe": {"interval": "5m"}}));
        assert!(option.contains("unknown field \"interval\""), "{option}");
        assert!(invalid(&json!({"target": "dev", "mode": "observe"})).contains("unknown field \"mode\""));
        assert!(invalid(&json!({"target": "dev", "reconciler": {}})).contains("must not be written by hand"));
        assert!(invalid(&json!({"target": "dev", "observationSources": {}})).contains("must not be written by hand"));
        assert!(invalid(&json!({"target": "dev", "workload": {}})).contains("must not be written by hand"));
    }

    fn observing(from: &Value) -> Value {
        json!({"target": "dev", "observe": {}, "observations": {"ip": {"type": "string", "from": from}}})
    }

    /// Catches a source the driver would only fail on at read time, or a
    /// Secret that reaches state through an observed value: the declaration
    /// is checked before a unit is stored.
    #[test]
    fn test_validate_checks_what_an_observation_reads() {
        let service = json!({"apiVersion": "v1", "kind": "Service", "namespace": "ingress", "name": "ingress-nginx"});
        assert!(validate(&observing(
            &json!({"resource": service, "pointer": "/status/loadBalancer/ingress/0/ip"})
        ))
        .is_ok());
        let sources = observation_sources(&observing(&json!({"resource": service, "pointer": "/status/x"}))).unwrap();
        assert_eq!(sources["ip"].resource.name, "ingress-nginx");
        assert_eq!(sources["ip"].resource.namespace.as_deref(), Some("ingress"));
        let cluster_scoped = json!({"apiVersion": "v1", "kind": "Namespace", "name": "ingress"});
        assert!(validate(&observing(
            &json!({"resource": cluster_scoped, "pointer": "/status/phase"})
        ))
        .is_ok());
        for (from, expected) in [
            (json!({"pointer": "/a"}), "resource is required"),
            (json!({"resource": service}), "pointer is required"),
            (json!({"resource": service, "pointer": "a"}), "JSON Pointer"),
            (
                json!({"resource": service, "pointer": ""}),
                "selects the whole resource",
            ),
            (
                json!({"resource": service, "pointer": "/a", "other": 1}),
                "unknown field \"other\"",
            ),
            (
                json!({"resource": {"apiVersion": "v1", "kind": "Service"}, "pointer": "/a"}),
                "name is required",
            ),
            (
                json!({"resource": {"apiVersion": "v1", "kind": "Service", "name": "x", "extra": 1}, "pointer": "/a"}),
                "unknown field \"extra\"",
            ),
            (
                json!({"resource": {"apiVersion": "v1", "kind": "Secret", "namespace": "a", "name": "x"}, "pointer": "/data"}),
                "never reads",
            ),
        ] {
            let message = invalid(&observing(&from));
            assert!(message.contains(expected), "{from}: {message}");
        }
    }

    /// Catches a publication waiting for the wrong thing: a unit that declares
    /// `observe` must observe and one that does not must wait for an operator
    /// unless `teardownWait` says otherwise, and a stored delay without a
    /// usable duration must never complete by itself.
    #[test]
    fn test_teardown_wait_reads_the_declaration_with_defaults_by_observe() {
        assert_eq!(teardown_wait(&json!({"observe": {}})), WaitStrategy::Observe);
        assert_eq!(teardown_wait(&json!({})), WaitStrategy::Manual);
        assert_eq!(
            teardown_wait(&json!({"observe": {}, "teardownWait": {"strategy": "delay", "duration": "10m"}})),
            WaitStrategy::Delay {
                duration: Duration::from_mins(10)
            }
        );
        assert_eq!(
            teardown_wait(&json!({"observe": {}, "teardownWait": {"strategy": "manual"}})),
            WaitStrategy::Manual
        );
        assert_eq!(
            teardown_wait(&json!({"teardownWait": {"strategy": "delay"}})),
            WaitStrategy::Manual
        );
    }

    #[test]
    fn test_validate_checks_teardown_wait_and_releases() {
        for wait in [
            json!({"strategy": "delay"}),
            json!({"strategy": "delay", "duration": "soon"}),
            json!({"strategy": "manual", "duration": "10m"}),
            json!({"strategy": "never"}),
            json!({"strategy": "manual", "extra": 1}),
            json!("manual"),
        ] {
            assert!(
                validate(&json!({"target": "dev", "teardownWait": wait})).is_err(),
                "{wait}"
            );
        }
        for releases in [json!([]), json!({"web": {}}), json!({"web/web": "x"})] {
            assert!(
                validate(&json!({"target": "dev", "releases": releases})).is_err(),
                "{releases}"
            );
        }
    }
}
