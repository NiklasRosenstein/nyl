#[cfg(unix)]
mod tests {
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    #[ignore = "Requires kube-client version that forwards exec-plugin stderr to parent stderr"]
    fn test_exec_auth_stderr_is_visible_in_cli_output() {
        let temp = TempDir::new().unwrap();

        let script_path = temp.path().join("exec-auth.sh");
        fs::write(
            &script_path,
            r#"#!/usr/bin/env sh
echo "NYL_EXEC_AUTH_INSTRUCTIONS" >&2
printf '{"apiVersion":"client.authentication.k8s.io/v1beta1","kind":"ExecCredential","status":{"token":"fake-token"}}'
"#,
        )
        .unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let kubeconfig_path = temp.path().join("config");
        let kubeconfig = format!(
            r#"apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: https://127.0.0.1:9
    insecure-skip-tls-verify: true
contexts:
- name: test
  context:
    cluster: test
    user: test-user
current-context: test
users:
- name: test-user
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: "{}"
"#,
            script_path.display()
        );
        fs::write(&kubeconfig_path, kubeconfig).unwrap();

        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        cmd.arg("release")
            .arg("list")
            .arg("--all")
            .env("KUBECONFIG", &kubeconfig_path);

        cmd.assert()
            .failure()
            .stderr(predicate::str::contains("NYL_EXEC_AUTH_INSTRUCTIONS"));
    }
}
