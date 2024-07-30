from typing import Any, NewType


Manifest = NewType("Manifest", dict[str, Any])
""" Represents a Kubernetes manifest. """

Manifests = NewType("Manifests", list[Manifest])
""" Represents a list of Kubernetes manifests. """
