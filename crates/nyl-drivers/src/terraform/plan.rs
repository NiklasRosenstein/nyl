//! The reviewed native change set, including moves, imports and output-only changes.
//!
//! Contract: [Change digest](../../../../design/infrastructure-units.md#change-digest).

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{Change, PlanReport};
use nyl_core::orchestration::ids::ChangeDigest;
use nyl_core::resources::units::OutputDeclaration;
use serde_json::{json, Map, Value};

/// Mask the tool's sensitivity tree and known credential values, preserving shape.
fn masked(value: &Value, sensitivity: &Value, secrets: &[String]) -> Value {
    if sensitivity == &Value::Bool(true) {
        return json!("***");
    }
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), masked(value, &sensitivity[key], secrets)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| masked(value, &sensitivity[index], secrets))
                .collect(),
        ),
        Value::String(value) => {
            let mut value = value.clone();
            for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
                value = value.replace(secret, "***");
            }
            Value::String(value)
        }
        value
            if secrets
                .iter()
                .any(|secret| !secret.is_empty() && secret == &value.to_string()) =>
        {
            json!("***")
        }
        value => value.clone(),
    }
}

fn change(value: &Value, secrets: &[String]) -> Value {
    json!({
        "actions": value["actions"],
        "before": masked(&value["before"], &value["before_sensitive"], secrets),
        "after": masked(&value["after"], &value["after_sensitive"], secrets),
        "afterUnknown": value["after_unknown"],
        "importing": masked(&value["importing"], &Value::Null, secrets),
    })
}

/// Outputs whose sensitivity declaration would allow a secret into a receipt fail
/// before apply, including outputs known to be sensitive only in planned values.
pub fn check_outputs(plan: &Value, declarations: &BTreeMap<String, OutputDeclaration>) -> Result<(), String> {
    for (name, declaration) in declarations {
        let output = &plan["planned_values"]["outputs"][name];
        let changes = &plan["output_changes"][name];
        if output.is_null() && changes.is_null() {
            return Err(format!("declared output {name:?} is absent from the native plan"));
        }
        if !declaration.sensitive
            && (output["sensitive"] == true
                || changes["after_sensitive"] == true
                || changes["before_sensitive"] == true)
        {
            return Err(format!(
                "output {name:?} is sensitive in the native plan; declare outputs.{name}.sensitive: true"
            ));
        }
        if let Some(value) = output.get("value") {
            if changes["after_unknown"] != true && !declaration.output_type.accepts(value) {
                return Err(format!(
                    "output {name:?} in the native plan does not match its declared type"
                ));
            }
        }
    }
    Ok(())
}

/// Canonical SHA-256 of exactly the effects reviewed, without volatile plan metadata.
pub fn report(plan: &Value, has_changes: bool, destroy: bool, secrets: &[String]) -> Result<PlanReport, String> {
    if !plan["format_version"]
        .as_str()
        .is_some_and(|version| version.starts_with("1."))
    {
        return Err("unsupported or missing native plan JSON format_version".to_owned());
    }
    if plan.get("errored") == Some(&Value::Bool(true)) {
        return Err("native plan contains errors".to_owned());
    }
    let mut resources = Vec::new();
    for resource in plan["resource_changes"].as_array().into_iter().flatten() {
        let native = &resource["change"];
        if native["actions"] != json!(["no-op"])
            || resource.get("previous_address").is_some()
            || native.get("importing").is_some()
        {
            resources.push(json!({"address": resource["address"], "previousAddress": resource["previous_address"], "deposed": resource["deposed"], "change": change(native, secrets)}));
        }
    }
    resources.sort_by_key(|resource| (resource["address"].to_string(), resource["deposed"].to_string()));
    let mut outputs = Map::new();
    for (name, value) in plan["output_changes"].as_object().into_iter().flatten() {
        if value["actions"] != json!(["no-op"]) {
            outputs.insert(name.clone(), change(value, secrets));
        }
    }
    if has_changes && resources.is_empty() && outputs.is_empty() {
        return Err("native tool reports changes but the reviewed JSON contains no supported change".to_owned());
    }
    let changes = json!({"resources": resources, "outputs": outputs});
    let digest = ChangeDigest::new(nyl_core::digest::sha256_prefixed(
        &nyl_core::digest::canonical_json_bytes(&changes).map_err(|error| error.to_string())?,
    ))
    .map_err(|error| error.to_string())?;
    Ok(PlanReport {
        changes: if !has_changes {
            Change::None
        } else if destroy {
            Change::Destroy
        } else if plan["prior_state"].is_null() {
            Change::Create
        } else {
            Change::Update
        },
        summary: if has_changes {
            serde_json::to_string(&changes).map_err(|error| error.to_string())?
        } else {
            "no changes".to_owned()
        },
        change_digest: Some(digest),
        blocked_by: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(plan: &Value, secrets: &[String]) -> ChangeDigest {
        let mut plan = plan.clone();
        plan["format_version"] = json!("1.0");
        report(&plan, false, false, secrets).unwrap().change_digest.unwrap()
    }

    #[test]
    fn test_digest_covers_moves_imports_and_output_only_changes_but_masks_sensitive_values() {
        let base = json!({"resource_changes": [{"address": "example.a", "change": {"actions": ["no-op"], "before": {"id": "a"}, "after": {"id": "a"}}}]});
        let unchanged = digest(&base, &[]);
        let mut moved = base.clone();
        moved["resource_changes"][0]["previous_address"] = json!("example.before");
        assert_ne!(unchanged, digest(&moved, &[]));
        let mut imported = base.clone();
        imported["resource_changes"][0]["change"]["importing"] = json!({"id": "native-id"});
        assert_ne!(unchanged, digest(&imported, &[]));
        let mut output = base;
        output["output_changes"] = json!({"id": {"actions": ["update"], "before": "a", "after": "b"}});
        assert_ne!(unchanged, digest(&output, &[]));
        let mut secret = json!({"format_version": "1.0", "resource_changes": [{"address": "example.a", "change": {"actions": ["create"], "after": {"password": "secret-one", "token": "known"}, "after_sensitive": {"password": true}}}]});
        let first = report(
            &{
                let mut plan = secret.clone();
                plan["format_version"] = json!("1.0");
                plan
            },
            true,
            false,
            &["known".to_owned()],
        )
        .unwrap();
        secret["resource_changes"][0]["change"]["after"]["password"] = json!("secret-two");
        secret["resource_changes"][0]["change"]["after"]["token"] = json!("rotated");
        assert_eq!(
            first.change_digest,
            report(&secret, true, false, &["rotated".to_owned()])
                .unwrap()
                .change_digest
        );
        let summary: Value = serde_json::from_str(&first.summary).unwrap();
        assert_eq!(summary["resources"][0]["change"]["after"]["password"], "***");
        assert_eq!(summary["resources"][0]["change"]["after"]["token"], "***");
    }

    #[test]
    fn test_preapply_output_admission_rejects_native_sensitive_output() {
        let declarations = serde_json::from_value(json!({"password": {"type": "string"}})).unwrap();
        assert!(check_outputs(
            &json!({"planned_values": {"outputs": {"password": {"sensitive": true, "value": "never-print"}}}}),
            &declarations
        )
        .unwrap_err()
        .contains("sensitive"));
        let declarations = serde_json::from_value(json!({"password": {"type": "string", "sensitive": true}})).unwrap();
        assert!(check_outputs(
            &json!({"output_changes": {"password": {"after_sensitive": true}}}),
            &declarations
        )
        .is_ok());
    }
}
