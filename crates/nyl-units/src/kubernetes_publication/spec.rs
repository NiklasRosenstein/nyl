//! The declaration of a `KubernetesPublication` unit and what the driver
//! validates about it.
//!
//! The spec is `{target, mode?, teardownWait?, releases?}` plus the fields
//! every unit shares. `target` names a static DeploymentTarget, or declares
//! one inline (`{inline, name?}`), in which case the renderer fills the
//! target's `name` and `sourceFile`. `releases` is filled by the renderer
//! from the target's `releaseInputs`, and `teardownSettings` and `argocd`
//! are recorded by the driver at resolution; none is authored.

use nyl_core::orchestration::driver::WaitStrategy;
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::units::COMMON_UNIT_FIELDS;
use nyl_render::gitops::units::PublicationTarget;
use nyl_render::resources::release_inputs::ReleaseKey;
use serde_json::Value;

/// The fields only this kind has.
const KIND_FIELDS: [&str; 4] = ["target", "mode", "teardownWait", "releases"];

/// Validate a rendered spec, references still as expressions.
pub(super) fn validate(spec: &Value) -> Result<(), String> {
    let spec = spec.as_object().ok_or("spec must be a mapping")?;
    for field in ["observations", "observationWait"] {
        if spec.contains_key(field) {
            return Err(format!(
                "spec.{field} (observed values) is not available yet: mode: observe attests `accepted` and `healthy` \
                 from Argo CD, but reading resource fields arrives later; remove it"
            ));
        }
    }
    if spec.contains_key("argocd") {
        return Err(
            "spec.argocd is recorded from the target's ArgoCDInstance when the unit is resolved and must not be written by hand; \
             declare the instance's access in its ArgoCDInstance spec.access instead"
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
            "spec has unknown field {field:?}; a KubernetesPublication has target, mode, and teardownWait besides the fields every unit shares"
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
    match spec.get("mode") {
        None => {}
        Some(Value::String(mode)) if mode == "publish" || mode == "observe" => {}
        Some(other) => return Err(format!("spec.mode must be publish or observe, got {other}")),
    }
    if let Some(wait) = spec.get("teardownWait") {
        validate_teardown_wait(wait)?;
    }
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

/// How a unit's teardown waits after phase 1, from its stored spec: the
/// declared `teardownWait`, else `observe` for `mode: observe` and `manual`
/// for `mode: publish`. Validation rejects an unknown strategy and a `delay`
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
        None => match spec.get("mode").and_then(Value::as_str) {
            Some("observe") => WaitStrategy::Observe,
            _ => WaitStrategy::Manual,
        },
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
            "mode": "publish",
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

    #[test]
    fn test_validate_requires_a_named_or_inline_target() {
        assert!(invalid(&json!({"mode": "publish"})).contains("spec.target is required"));
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
        assert!(validate(&inline(&json!({"publication": publication}))).is_ok());
        assert!(invalid(&inline(&json!({"publication": publication, "bogus": 1}))).contains("bogus"));
        assert!(invalid(&inline(&json!({}))).contains("publication"));
        assert!(invalid(&json!({"target": {"inline": {"publication": publication}}})).contains("spec.target.name"));
    }

    /// Catches the observing declarations being rejected (the `observe`
    /// strategy and mode work), and the fields only the driver records or a
    /// later milestone reads being accepted from an author.
    #[test]
    fn test_validate_accepts_observation_and_rejects_recorded_and_unavailable_fields() {
        assert!(validate(&json!({"target": "dev", "teardownWait": {"strategy": "observe"}})).is_ok());
        assert!(validate(&json!({"target": "dev", "mode": "observe"})).is_ok());
        assert!(validate(&json!({"target": "dev", "mode": "observe", "teardownWait": {"strategy": "manual"}})).is_ok());
        assert!(invalid(&json!({"target": "dev", "mode": "apply"})).contains("must be publish or observe"));
        assert!(invalid(&json!({"target": "dev", "argocd": {}})).contains("must not be written by hand"));
        let observations = invalid(&json!({"target": "dev", "mode": "observe", "observations": {}}));
        assert!(
            observations.contains("observed values") && observations.contains("not available yet"),
            "{observations}"
        );
    }

    /// Catches a publication waiting for the wrong thing: `mode: observe`
    /// must observe and `mode: publish` must wait for an operator unless
    /// `teardownWait` says otherwise, and a stored delay without a usable
    /// duration must never complete by itself.
    #[test]
    fn test_teardown_wait_reads_the_declaration_with_defaults_by_mode() {
        assert_eq!(teardown_wait(&json!({"mode": "observe"})), WaitStrategy::Observe);
        assert_eq!(teardown_wait(&json!({"mode": "publish"})), WaitStrategy::Manual);
        assert_eq!(teardown_wait(&json!({})), WaitStrategy::Manual);
        assert_eq!(
            teardown_wait(&json!({"mode": "observe", "teardownWait": {"strategy": "delay", "duration": "10m"}})),
            WaitStrategy::Delay {
                duration: Duration::from_mins(10)
            }
        );
        assert_eq!(
            teardown_wait(&json!({"mode": "publish", "teardownWait": {"strategy": "observe"}})),
            WaitStrategy::Observe
        );
        assert_eq!(
            teardown_wait(&json!({"mode": "observe", "teardownWait": {"strategy": "manual"}})),
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
