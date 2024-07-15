"""
This module implements a Kubernetes CRD that represents Nyl's own understanding of an application, which also
acts as a Kubernetes ApplySet. To read more about ApplySets, check out

    https://kubernetes.io/blog/2023/05/09/introducing-kubectl-applyset-pruning/.
"""

import base64
import hashlib
from typing import Any, Collection


GROUP = "nyl.io"
KIND = "Application"
PLURAL = "applications"
VERSION = "v1"

# note: the only purpose of this CRD is to create resources that act as a parent for ApplySets.
#       check out this GitHub issue, and specifically this comment for more information:
#       https://github.com/kubernetes/enhancements/issues/3659#issuecomment-1753091733
APPLICATION_CRD = {
    "apiVersion": "apiextensions.k8s.io/v1",
    "kind": "CustomResourceDefinition",
    "metadata": {
        "name": f"{PLURAL}.{GROUP}",
        "labels": {
            "applyset.kubernetes.io/is-parent-type": "true",
        },
    },
    "spec": {
        "group": GROUP,
        "names": {
            "kind": KIND,
            "plural": PLURAL,
        },
        "scope": "Cluster",
        "versions": [
            {
                "name": VERSION,
                "served": True,
                "storage": True,
                "schema": {
                    "openAPIV3Schema": {
                        "type": "object",
                    }
                },
            }
        ],
    },
}


def calculate_applyset_id(*, name: str, namespace: str = "", group: str) -> str:
    """
    Calculate the ID of a Kubernetes ApplySet with the specified name.
    """

    # reference: https://kubernetes.io/docs/reference/labels-annotations-taints/#applyset-kubernetes-io-id
    hash = hashlib.sha256(f"{name}.{namespace}.ApplySet.{group}".encode()).digest()
    uid = base64.b64encode(hash).decode().rstrip("=").replace("/", "_").replace("+", "-")
    return f"applyset-{uid}-v1"


def get_canonical_resource_kind_name(api_version: str, kind: str) -> str:
    """
    Given the apiVersion and kind of a Kubernetes resource, return the canonical name of the resource. This name can
    be used to identify the resource in an ApplySet's `applyset.kubernetes.io/contains-group-kinds` annotation.

    Args:
        api_version: The apiVersion of the resource.
        kind: The kind of the resource.
    """

    return (f"{kind}." + (api_version.split("/")[0] if "/" in api_version else "")).rstrip(".")


def generate_application_resource(
    name: str,
    contains_resources: Collection[str],
    kubectl_version: str,
) -> dict[str, Any]:
    """
    Generate a Kubernetes resource that represents an application in Nyl's system.

    Args:
        name: The name of the application.
        contains_resources: A list of Kubernetes resources names that are contained in this application. This is a
            required information for Kubernetes ApplySets. You can use `get_canonical_resource_kind_name` to get the
            names of the resource kinds.
        kubectl_version: The version of kubectl that was used to create this resource. This is a required information
            for the `applyset.kubernetes.io/tooling` annotation in the resource if `kubectl` is used to apply it.
    """

    return {
        "apiVersion": f"{GROUP}/{VERSION}",
        "kind": KIND,
        "metadata": {
            "name": name,
            "annotations": {
                "applyset.kubernetes.io/tooling": f"kubectl/{kubectl_version}",
                "applyset.kubernetes.io/contains-group-kinds": ",".join(sorted(contains_resources)),
            },
            "labels": {
                "applyset.kubernetes.io/id": calculate_applyset_id(name=name, group=GROUP),
            },
        },
    }
