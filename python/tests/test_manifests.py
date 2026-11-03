import pytest

from nyl import (
    AmbiguousResourceError,
    Container,
    DeclarationError,
    InvalidOutputError,
    ManifestSet,
    Resource,
    ResourceNotFoundError,
)


def resource(kind="Deployment", name="web", namespace=None, api_version="apps/v1", **extra):
    return Resource(
        {
            "apiVersion": api_version,
            "kind": kind,
            "metadata": {"name": name, **({"namespace": namespace} if namespace else {})},
            **extra,
        }
    )


def test_selection_identity_namespace_and_order():
    resources = [
        resource(namespace="a"),
        resource(namespace="b"),
        resource(api_version="example.com/v1"),
        resource(kind="Service", name="api"),
    ]
    manifests = ManifestSet(resources)
    assert list(manifests.select("Deployment")) == resources[:3]
    assert manifests.one("Deployment", namespace="a") is resources[0]
    assert manifests.one("Deployment", namespace=None) is resources[2]
    assert manifests.one("Deployment", api_version="example.com/v1") is resources[2]
    assert list(manifests.select(name="web", api_version="apps/v1")) == resources[:2]
    assert list(manifests.select(namespace="missing")) == []
    assert manifests.select("Service").one("Service", "api") is resources[3]
    with pytest.raises(AmbiguousResourceError, match="3 matches.*a/web.*b/web.*<none>/web"):
        manifests.one("Deployment", "web")
    with pytest.raises(ResourceNotFoundError, match="Available:.*apps/v1.*Service.*api"):
        manifests.one("Secret")


def test_safe_yaml_documents_and_scalars():
    manifests = ManifestSet.from_yaml("""
---
# Empty document
---
apiVersion: v1
kind: ConfigMap
metadata: {name: test}
data:
  enabled: "on"
  affirmative: 'yes'
  code: "001"
  unquoted: on
  boolean: true
  integer: 3
  floating: 1.5
  date: 2024-01-01
---
null
---
apiVersion: v1
kind: Namespace
metadata: {name: test}
""")
    assert len(manifests) == 2
    assert manifests.one("ConfigMap").raw["data"] == {
        "enabled": "on",
        "affirmative": "yes",
        "code": "001",
        "unquoted": "on",
        "boolean": True,
        "integer": 3,
        "floating": 1.5,
        "date": "2024-01-01",
    }
    assert manifests.one("Namespace").namespace is None
    assert len(ManifestSet.from_yaml("")) == 0


def test_yaml_11_directive_does_not_change_scalar_semantics():
    with pytest.raises(InvalidOutputError, match="YAML 1.2"):
        ManifestSet.from_yaml(
            "%YAML 1.1\n---\napiVersion: v1\nkind: ConfigMap\n"
            "metadata: {name: test}\ndata: {enabled: on}\n"
        )


@pytest.mark.parametrize(
    "stream",
    [
        "[broken",
        "!!python/object/apply:os.system ['exit 1']",
        "42",
        "[]",
        "kind: ConfigMap\n",
        "apiVersion: v1\nkind: Secret\nmetadata: {name: 12}",
        "apiVersion: v1\nkind: Secret\nmetadata: {name: x, namespace: []}",
        "apiVersion: v1\napiVersion: apps/v1\nkind: Secret\nmetadata: {name: x}",
        b"\xff",
    ],
)
def test_invalid_output(stream):
    with pytest.raises(InvalidOutputError):
        ManifestSet.from_yaml(stream)


@pytest.mark.parametrize(
    "kind,path",
    [
        ("Pod", ["spec"]),
        *[
            (kind, ["spec", "template", "spec"])
            for kind in (
                "Deployment",
                "StatefulSet",
                "DaemonSet",
                "Job",
                "ReplicaSet",
                "ReplicationController",
            )
        ],
        ("CronJob", ["spec", "jobTemplate", "spec", "template", "spec"]),
    ],
)
def test_container_templates(kind, path):
    raw = {
        "containers": [{"name": "app", "securityContext": {"allowPrivilegeEscalation": False}}],
        "initContainers": [{"name": "init"}],
    }
    for key in reversed(path):
        raw = {key: raw}
    workload = resource(kind, **raw)
    assert [container.name for container in workload.containers()] == ["app"]
    containers = list(workload.containers(include_init=True))
    assert [container.name for container in containers] == ["app", "init"]
    assert containers[0].security_context == {"allowPrivilegeEscalation": False}
    assert containers[1].security_context == {}


def test_unsupported_and_invalid_containers():
    with pytest.raises(DeclarationError, match="does not support"):
        list(resource("Service").containers())
    with pytest.raises(DeclarationError, match="Pod template"):
        list(resource().containers())
    with pytest.raises(DeclarationError, match="must be a list"):
        list(resource("Pod", spec={"containers": {}}).containers())
    with pytest.raises(DeclarationError, match="string name"):
        list(resource("Pod", spec={"containers": [42]}).containers())
    with pytest.raises(DeclarationError, match="securityContext"):
        assert Container({"name": "app", "securityContext": []}).security_context == {}


def test_environment_helpers_and_concise_representations():
    raw = {
        "name": "app",
        "env": [
            {"name": "PGHOST", "value": "db-rw"},
            {"name": "EMPTY", "value": ""},
            {
                "name": "PGPASSWORD",
                "valueFrom": {"secretKeyRef": {"name": "db-app", "key": "password"}},
            },
        ],
    }
    container = Container(raw)
    assert container.raw is raw
    assert container.env.value("PGHOST") == "db-rw"
    assert container.env.value("EMPTY") == ""
    assert container.env.secret_ref("PGPASSWORD") == ("db-app", "password")
    assert repr(container) == "Container('app')"
    workload = resource(spec={"private": "SECRET"})
    assert repr(workload) == "Resource(apps/v1, Deployment, <none>/web)"
    assert repr(ManifestSet([workload])) == "ManifestSet(1 resources)"


@pytest.mark.parametrize(
    "entries,method",
    [
        ([], "value"),
        ([{"name": "X", "value": "a"}] * 2, "value"),
        ([{"name": "X"}], "value"),
        ([{"name": "X", "value": False}], "value"),
        ([{"name": "X", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}], "value"),
        ([{"name": "X", "value": "a"}], "secret_ref"),
        (
            [{"name": "X", "valueFrom": {"configMapKeyRef": {"name": "s", "key": "k"}}}],
            "secret_ref",
        ),
        ([{"name": "X", "valueFrom": {"secretKeyRef": {"name": "s"}}}], "secret_ref"),
        (
            [{"name": "X", "value": "a", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}],
            "secret_ref",
        ),
        (
            [
                {
                    "name": "X",
                    "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}, "fieldRef": {}},
                }
            ],
            "secret_ref",
        ),
        ({}, "value"),
        (["bad entry"], "value"),
    ],
)
def test_incompatible_environment_declarations(entries, method):
    container = Container(
        {"name": "app", "env": entries, "envFrom": [{"secretRef": {"name": "s"}}]}
    )
    with pytest.raises(DeclarationError, match="Container.*env"):
        getattr(container.env, method)("X")
