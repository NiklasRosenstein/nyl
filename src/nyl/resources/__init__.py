"""
This package contains Nyl's own Kubernetes-esque resources.
"""

from abc import ABC
from dataclasses import dataclass
from typing import Any, ClassVar
from databind.json import load as deser


REGISTRY = {
    "HelmChart",
    "StatefulSecret",
    "Package",
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
    def load(manifest: dict[str, Any]) -> "NylResource":
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

        manifest = dict(manifest)
        manifest.pop("apiVersion")
        manifest.pop("kind")

        return deser(manifest, cls)


@dataclass
class ObjectMetadata:
    """
    Kubernetes object metadata.
    """

    name: str
    namespace: str | None = None
    labels: dict[str, str] | None = None
    annotations: dict[str, str] | None = None
