"""
This module implements Nyl's Kubernetes support. Nyl allows you to generate Kubernetes manifests from various sources,
such as raw manifests with optional minimalistic templating support (e.g. to inject secret values), Helm charts, or
Kustomize overlays. The generated manifests can be applied to a Kubernetes cluster using `kubectl` or a similar tool,
like ArgoCD; however in order to fit into a full Nyl-managed deployment pipeline, the manifests must be applied by
Nyl itself.

Nyl provides a diffing feature when applying Kubernetes manifests, allowing you to see the changes that will be applied
to the cluster before actually applying them. This is useful for verifying that the changes are correct before applying
them to the cluster. This diffing feature relies on the `kubectl diff` command.
"""

from .apply_manifests import ApplyManifests
from .manifest_source import ManifestSource, ManifestSourceError, HelmManifestSource, RawManifestSource

__all__ = [
    "ApplyManifests",
    "HelmManifestSource",
    "ManifestSource",
    "ManifestSourceError",
    "RawManifestSource"
]
