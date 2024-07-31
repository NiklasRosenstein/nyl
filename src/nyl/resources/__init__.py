"""
This package contains Nyl's own Kubernetes-esque resources.
"""

from abc import ABC
from dataclasses import dataclass
from typing import ClassVar, cast
from databind.json import load as deser, dump as ser

from nyl.tools.types import Manifest

API_VERSION_K8S = "nyl.io/v1"
API_VERSION_INLINE = "inline.nyl.io/v1"


class NylResource(ABC):
    """
    Base class for Nyl custom resources.
    """

    API_VERSION: ClassVar[str]
    """
    The API version of the resource. This is usually `inline.nyl.io/v1` for resources that are inlined by Nyl at
    templating time and are not present in the final manifest, or `nyl.io/v1` for resources that are actual Kubernetes
    resources.
    """

    KIND: ClassVar[str]
    """
    The kind identifier of the resource. If not set, this will default to the class name.
    """

    def __init_subclass__(cls, api_version: str, kind: str | None = None) -> None:
        cls.API_VERSION = api_version
        if kind is not None or "KIND" not in vars(cls):
            cls.KIND = kind or cls.__name__

    @staticmethod
    def load(manifest: Manifest) -> "NylResource":
        """
        Load a Nyl resource from a manifest.
        """

        if manifest.get("apiVersion") not in (API_VERSION_K8S, API_VERSION_INLINE):
            raise ValueError(f"Unsupported apiVersion: {manifest.get('apiVersion')!r}")

        kind = manifest["kind"]
        module_name = __name__ + "." + kind.lower()
        try:
            module = __import__(module_name, fromlist=[kind])
            cls: type[NylResource] = getattr(module, kind)
            assert isinstance(cls, type) and issubclass(cls, NylResource), f"{cls} is not a NylResource"
        except (ImportError, AttributeError, AssertionError):
            raise ValueError(f"Unsupported resource kind: {kind}")

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

        if manifest.get("apiVersion") in (API_VERSION_K8S, API_VERSION_INLINE):
            return NylResource.load(manifest)
        return None

    def dump(self) -> Manifest:
        """
        Dump the resource to a manifest.
        """

        manifest = cast(Manifest, ser(self, type(self)))
        manifest["apiVersion"] = self.API_VERSION
        manifest["kind"] = self.KIND
        return Manifest(manifest)


@dataclass
class ObjectMetadata:
    """
    Kubernetes object metadata.
    """

    name: str
    namespace: str | None = None
    labels: dict[str, str] | None = None
    annotations: dict[str, str] | None = None
