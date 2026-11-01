//! Command unit declaration and validation.
use nyl_core::resources::units::{CommonUnitFields, KindSupport};
use nyl_core::resources::DenyUnknownFields;
use nyl_core::{CoreError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Kind name of the built-in command unit.
pub const KIND_COMMAND: &str = "Command";

/// Runs commands from the environment's source commit.
///
/// The command starts from an empty environment plus `PATH`, a fresh `HOME`,
/// `NYL_INPUTS` with the resolved `values`, `NYL_OUTPUTS` for its outputs,
/// `NYL_UNIT`, `NYL_ENVIRONMENT`, `NYL_OPERATION`, and what `env` admits.
/// There is no sandbox beyond the working directory and that environment.
///
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = nyl_core::resources::strict::deny_additional_properties)]
pub struct CommandSpec {
    /// Files, as local-path globs, whose content enters the execution key. Other files may be read but never trigger re-execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Values the command reads from `NYL_INPUTS`; references such as `fromUnit` may appear at any depth.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub values: serde_json::Value,
    /// Argument vector of the reconcile step, executed without a shell. A relative first element is resolved against `workingDir`.
    pub command: Vec<String>,
    /// Argument vector reporting drift: exit 0 for clean, 2 for drift, anything else for an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Vec<String>>,
    /// Argument vector removing the command's effects. Without it the kind cannot tear down and defaults to `deletionPolicy: Retain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown: Option<Vec<String>>,
    /// Argument vector whose stdout enters the execution key, such as tool versions. Runs during resolution without secrets; must be fast and deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Vec<String>>,
    /// Working directory under the local path rule, where `/` alone names the worktree root. Default the directory containing `nyl.toml`.
    #[serde(rename = "workingDir", default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// The author's claim that re-running with identical inputs is safe: failures become retryable and uncertain executions re-run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent: bool,
    #[serde(flatten)]
    pub common: CommonUnitFields,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}

impl CommandSpec {
    /// What the kind supports for this spec: declared outputs always,
    /// teardown only with a `teardown` step.
    pub fn support(&self) -> KindSupport {
        KindSupport {
            declared_outputs: true,
            teardown: self.teardown.is_some(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_argv("spec.command", &self.command)?;
        for (field, argv) in [
            ("spec.verify", &self.verify),
            ("spec.teardown", &self.teardown),
            ("spec.fingerprint", &self.fingerprint),
        ] {
            if let Some(argv) = argv {
                validate_argv(field, argv)?;
            }
        }
        for (index, pattern) in self.files.iter().enumerate() {
            let field = format!("spec.files[{index}]");
            nyl_core::local_path::validate_local_path(&field, pattern)?;
            glob::Pattern::new(pattern)
                .map_err(|error| CoreError::config(format!("Invalid {field} glob {pattern:?}: {error}")))?;
        }
        if let Some(directory) = &self.working_dir {
            if directory != "/" {
                nyl_core::local_path::validate_local_path("spec.workingDir", directory)?;
            }
        }
        if !(self.values.is_null() || self.values.is_object()) {
            return Err(CoreError::config("spec.values must be an object"));
        }
        self.common.validate(self.support())
    }
}

fn validate_argv(field: &str, argv: &[String]) -> Result<()> {
    match argv.first() {
        Some(program) if !program.is_empty() => Ok(()),
        _ => Err(CoreError::config(format!(
            "{field} must be a non-empty argument vector whose first element names the program"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::time::Duration;
    use nyl_core::resources::units::DeletionPolicy;
    use serde_json::json;
    fn command(spec: serde_json::Value) -> Result<CommandSpec> {
        let spec: CommandSpec = serde_json::from_value(spec)?;
        spec.validate()?;
        Ok(spec)
    }

    #[test]
    fn test_command_accepts_the_contract_example() {
        let spec = command(json!({
            "files": ["../scripts/seed/**"],
            "values": {"bucket": {"fromUnit": {"unit": "storage", "output": "bucket"}}},
            "command": ["./run.sh"],
            "verify": ["./verify.sh"],
            "teardown": ["./remove.sh"],
            "fingerprint": ["./tool-versions.sh"],
            "workingDir": "../scripts/seed",
            "env": {"passthrough": ["AWS_REGION", "AWS_PROFILE"], "secrets": {"DB_PASSWORD": "database-password"}},
            "idempotent": true,
            "outputs": {"seedVersion": {"type": "string"}},
            "timeout": "10m"
        }))
        .unwrap();
        assert_eq!(spec.common.timeout, Some(Duration::from_mins(10)));
        assert_eq!(
            spec.common.effective_deletion_policy(spec.support()),
            DeletionPolicy::Teardown
        );
    }

    #[test]
    fn test_command_without_teardown_retains_and_rejects_teardown_policy() {
        let spec = command(json!({"command": ["./run.sh"]})).unwrap();
        assert_eq!(
            spec.common.effective_deletion_policy(spec.support()),
            DeletionPolicy::Retain
        );
        let error = command(json!({"command": ["./run.sh"], "deletionPolicy": "Teardown"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot tear down"), "{error}");
    }

    /// Catches a timeout so large that deadline arithmetic overflows.
    #[test]
    fn test_command_rejects_a_timeout_above_seven_days() {
        assert!(command(json!({"command": ["./run.sh"], "timeout": "7d"})).is_ok());
        let error = command(json!({"command": ["./run.sh"], "timeout": "100000000d"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("spec.timeout may be at most 7d"), "{error}");
    }

    #[test]
    fn test_command_rejects_unknown_fields_next_to_flattened_common_fields() {
        let error = command(json!({"command": ["./run.sh"], "dependOn": ["network"]}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `dependOn`"), "{error}");
    }
}
