/// Comprehensive error handling tests
use nyl::{NylError, Result};

#[test]
fn test_error_variants() {
    // Test all error constructor methods
    let config_err = NylError::config("test config error");
    assert!(config_err.is_config_error());
    assert!(format!("{config_err}").contains("Hint:"));

    let helm_err = NylError::helm_chart("test helm error");
    assert!(format!("{helm_err}").contains("explicit Helm values"));

    let validation_err = NylError::validation("test validation");
    let validation_display = format!("{validation_err}");
    assert!(validation_display.contains("rerun the command"));

    let k8s_err = NylError::kubernetes("test k8s error");
    assert!(k8s_err.is_kubernetes_error());
}

#[test]
fn test_error_classification() {
    // Config errors
    assert!(NylError::Config("test".into()).is_config_error());
    assert!(NylError::ConfigNotFound("test".into()).is_config_error());

    // Kubernetes errors
    assert!(NylError::Kubernetes("test".into()).is_kubernetes_error());

    // Negative cases
    assert!(!NylError::Process("test".into()).is_config_error());
    assert!(!NylError::Process("test".into()).is_kubernetes_error());
}

#[test]
fn test_error_messages_have_context() {
    let errors = vec![
        NylError::config("test"),
        NylError::helm_chart("test"),
        NylError::validation("test"),
        NylError::process("test"),
    ];

    for error in errors {
        let msg = format!("{error}");
        assert!(msg.contains("Hint:"), "Error message should contain a hint: {msg}");
    }
}

#[test]
fn test_error_conversion_from_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let nyl_err: NylError = io_err.into();
    assert!(matches!(nyl_err, NylError::Io(_)));
}

#[test]
fn test_error_conversion_from_json() {
    let json_str = "{invalid json}";
    let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
    let nyl_err: NylError = json_err.into();
    assert!(matches!(nyl_err, NylError::Json(_)));
}

#[test]
fn test_result_type_alias() {
    fn returns_result() -> Result<String> {
        Ok("success".to_string())
    }

    assert!(returns_result().is_ok());
}

#[test]
fn test_error_propagation() {
    fn inner() -> Result<()> {
        Err(NylError::validation("inner error"))
    }

    fn outer() -> Result<()> {
        inner()?;
        Ok(())
    }

    let result = outer();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("inner error"));
}
