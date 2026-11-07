import subprocess

import pytest

from nyl import NylClient, RenderError


def test_target_release_uses_checkout_cli_and_refreshes_inputs(tmp_path, cli):
    subprocess.run(["git", "init", "--quiet", str(tmp_path)], check=True, timeout=10)
    (tmp_path / "nyl.toml").write_text("")
    (tmp_path / "gitops.yaml").write_text("""
apiVersion: gitops.nyl/v1
kind: GitRepository
metadata: {name: deploy}
spec: {repoURL: 'https://example.invalid/deploy.git'}
---
apiVersion: k8s.nyl/v1
kind: Cluster
metadata: {name: local}
spec:
  destination: {server: 'https://kubernetes.invalid'}
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions: [v1, apps/v1, v1/Namespace, v1/ConfigMap, apps/v1/Deployment]
    clusterScopedKinds: [core/Namespace]
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: development}
spec:
  reconciler: {argocd: {}}
  clusterRef: {name: local}
  applicationGroupSelector: {matchLabels: {group: workloads}}
  publication:
    repositoryRef: {name: deploy}
    revision: deploy
    pathPrefix: development
  values: {host: db-rw}
---
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata: {name: workloads, labels: {group: workloads}}
spec:
  applicationNamespace: argocd
  source: {path: applications/workloads}
""")
    source = tmp_path / "applications/workloads/web.yaml"
    source.parent.mkdir(parents=True)
    template = """
apiVersion: k8s.nyl/v1
kind: Release
metadata: {name: web, namespace: web}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: web}
spec:
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: web
        image: web:1
        securityContext: {allowPrivilegeEscalation: false}
        env:
        - {name: PGHOST, value: '{{ values.host }}'}
        - name: PGPASSWORD
          valueFrom: {secretKeyRef: {name: db-app, key: password}}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: scalars, namespace: web}
data: {enabled: "on", affirmative: "yes", code: "001", date: "2024-01-01"}
"""
    source.write_text(template)
    client = NylClient(tmp_path, executable=cli)
    manifests = client.render(source.relative_to(tmp_path), target="development", offline=True)
    deployment = manifests.one("Deployment", "web", namespace="web")
    assert deployment.namespace == "web"
    container = next(deployment.containers())
    assert container.security_context["allowPrivilegeEscalation"] is False
    assert container.env.value("PGHOST") == "db-rw"
    assert container.env.secret_ref("PGPASSWORD") == ("db-app", "password")
    assert manifests.one("Namespace", "web").namespace is None
    assert manifests.one("ConfigMap").raw["data"] == {
        "enabled": "on",
        "affirmative": "yes",
        "code": "001",
        "date": "2024-01-01",
    }
    source.write_text(template.replace("web:1", "web:2"))
    refreshed = client.render(source, target="development", offline=True)
    assert next(refreshed.one("Deployment").containers()).raw["image"] == "web:2"
    assert container.raw["image"] == "web:1"


def test_project_validation_defaults_are_respected(tmp_path, cli):
    (tmp_path / "nyl.toml").write_text("[validation]\nenabled = true\n")
    (tmp_path / "web.yaml").write_text(
        "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: web}\ndata: {bad: false}\n"
    )
    # The project requires a validator but declares none: fail rather than bypassing it.
    with pytest.raises(RenderError, match="Validation requested but no validators configured"):
        NylClient(tmp_path, executable=cli).render("web.yaml", offline=True)
