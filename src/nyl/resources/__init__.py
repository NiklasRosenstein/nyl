"""
This package contains Nyl's own Kubernetes-esque resources.
"""

from abc import ABC
from dataclasses import dataclass
from typing import ClassVar
from databind.json import load as deser

from nyl.tools.types import Manifest


REGISTRY = {
    "HelmChart",
    "StatefulSecret",
}
""" Collection of custom resources that Nyl supports. This is used to lookup the corresponding resource class."""


class NylResource(ABC):
    """
    Base class for Nyl custom resources.
    """

    API_VERSION: ClassVar[str] = "nyl.io/v1"
    KIND: ClassVar[str]

    def __init_subclass__(cls) -> None:
        if "KIND" not in vars(cls):
            cls.KIND = cls.__name__

    @staticmethod
    def load(manifest: Manifest) -> "NylResource":
        """
        Load a Nyl resource from a manifest.
        """

        kind = manifest["kind"]
        if kind not in REGISTRY:
            raise ValueError(f"Unsupported Nyl resource kind: {kind}")

        module_name = __name__ + "." + kind.lower()
        module = __import__(module_name, fromlist=[kind])
        cls: type[NylResource] = getattr(module, kind)
        assert isinstance(cls, type) and issubclass(cls, NylResource), f"{cls} is not a NylResource"

        manifest = Manifest(manifest)
        manifest.pop("apiVersion")
        manifest.pop("kind")

        return deser(manifest, cls)

    @staticmethod
    def maybe_load(manifest: Manifest) -> "NylResource | None":
        """
        Maybe load the manifest into a NylResource if the `apiVersion` matches. If the resource kind is not supported,
        an error will be raised.
        """

        if manifest["apiVersion"] != NylResource.API_VERSION:
            return None

        return NylResource.load(manifest)

    @staticmethod
    def is_nyl_resource(manifest: Manifest) -> bool:
        """
        Check if a manifest is a Nyl resource.
        """

        return manifest.get("apiVersion") == NylResource.API_VERSION


@dataclass
class ObjectMetadata:
    """
    Kubernetes object metadata.
    """

    name: str
    namespace: str | None = None
    labels: dict[str, str] | None = None
    annotations: dict[str, str] | None = None
