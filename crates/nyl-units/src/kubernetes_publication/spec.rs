//! The declaration of a `KubernetesPublication` unit and what the driver
//! validates about it.
//!
//! The spec is `{target, mode?, teardownWait?, releases?}` plus the fields
//! every unit shares. `releases` is filled by the renderer from the target's
//! `releaseInputs`, and `teardownSettings` is recorded by the driver at
//! resolution; neither is authored.

use nyl_core::orchestration::time::Duration;
use nyl_render::resources::release_inputs::ReleaseKey;
use serde_json::Value;

/// Fields every unit kind shares, which the common validation checks.
const COMMON_FIELDS: [&str; 8] = [
    "enabled",
    "outputs",
    "dependsOn",
    "approval",
    "deletionPolicy",
    "timeout",
    "env",
    "attestations",
];

/// The fields only this kind has.
const KIND_FIELDS: [&str; 4] = ["target", "mode", "teardownWait", "releases"];

/// Validate a rendered spec, references still as expressions.
pub(super) fn validate(spec: &Value) -> Result<(), String> {
    let spec = spec.as_object().ok_or("spec must be a mapping")?;
    for field in ["observations", "observationWait"] {
        if spec.contains_key(field) {
            return Err(format!(
                "spec.{field} needs mode: observe, which is not available yet; remove it and publish with mode: publish"
            ));
        }
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
        .find(|field| !KIND_FIELDS.contains(&field.as_str()) && !COMMON_FIELDS.contains(&field.as_str()))
    {
        return Err(format!(
            "spec has unknown field {field:?}; a KubernetesPublication has target, mode, and teardownWait besides the fields every unit shares"
        ));
    }
    match spec.get("target") {
        Some(Value::String(name)) if !name.is_empty() => {}
        Some(Value::Object(target)) if target.contains_key("inline") => {
            return Err("spec.target.inline is not supported yet; name a static DeploymentTarget".to_owned())
        }
        _ => return Err("spec.target is required and must name a static DeploymentTarget".to_owned()),
    }
    match spec.get("mode") {
        None => {}
        Some(Value::String(mode)) if mode == "publish" => {}
        Some(Value::String(mode)) if mode == "observe" => {
            return Err(
                "NYL-KUBERNETES-MODE-UNSUPPORTED: spec.mode observe is not available yet, so Argo CD cannot be read; \
                 use mode: publish, which publishes the tree and records the PublishedTree artifact"
                    .to_owned(),
            )
        }
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
    fn test_validate_requires_a_static_target() {
        assert!(invalid(&json!({"mode": "publish"})).contains("spec.target is required"));
        assert!(invalid(&json!({"target": 1})).contains("spec.target is required"));
        assert!(invalid(&json!({"target": ""})).contains("spec.target is required"));
        assert!(invalid(&json!({"target": {"inline": {}}})).contains("spec.target.inline is not supported yet"));
    }

    /// Catches observe mode being accepted before anything can read Argo CD,
    /// which would publish and then promise health it never checks.
    #[test]
    fn test_validate_rejects_observe_mode_with_its_own_code() {
        let message = invalid(&json!({"target": "dev", "mode": "observe"}));
        assert!(message.starts_with("NYL-KUBERNETES-MODE-UNSUPPORTED:"), "{message}");
        assert!(message.contains("mode: publish"), "{message}");
        assert!(invalid(&json!({"target": "dev", "mode": "apply"})).contains("must be publish or observe"));
        assert!(invalid(&json!({"target": "dev", "observations": {}})).contains("needs mode: observe"));
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
