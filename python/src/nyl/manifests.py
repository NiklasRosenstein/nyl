"""Small declaration helpers over rendered dictionaries, without cluster access."""

from __future__ import annotations

from collections.abc import Iterable, Iterator
from enum import Enum
from typing import Any

from ruamel.yaml import YAML
from ruamel.yaml.constructor import SafeConstructor
from ruamel.yaml.error import YAMLError

from .errors import (
    AmbiguousResourceError,
    DeclarationError,
    InvalidOutputError,
    ResourceNotFoundError,
)


class _Namespace(Enum):
    ANY = "all namespaces"


class _ManifestConstructor(SafeConstructor):
    pass


# Kubernetes manifest dictionaries contain JSON scalars, including date-like strings.
_ManifestConstructor.add_constructor(
    "tag:yaml.org,2002:timestamp", _ManifestConstructor.construct_scalar
)


class ManifestSet:
    """An ordered, iterable collection of resources."""

    def __init__(self, resources: Iterable[Resource] = ()) -> None:
        self._resources = tuple(resources)

    @classmethod
    def from_yaml(cls, stream: str | bytes) -> ManifestSet:
        """Safely parse a YAML 1.2 stream, ignoring empty documents.

        Nyl quotes string scalars when emitting YAML. No YAML 1.1 boolean or
        timestamp coercion is applied here. Parser errors omit input excerpts.
        """
        yaml = YAML(typ="safe", pure=True)
        yaml.version = (1, 2)
        yaml.Constructor = _ManifestConstructor
        resources = []
        try:
            for document in yaml.load_all(stream):
                if document is not None:
                    resources.append(Resource(document))
        except (YAMLError, UnicodeError, RecursionError):
            raise InvalidOutputError("Invalid rendered YAML (input omitted)") from None
        if any(
            info.doc_version is not None
            and (info.doc_version.major, info.doc_version.minor) != (1, 2)
            for info in yaml.doc_infos
        ):
            raise InvalidOutputError("Rendered YAML must use YAML 1.2 scalar semantics")
        return cls(resources)

    def __iter__(self) -> Iterator[Resource]:
        return iter(self._resources)

    def __len__(self) -> int:
        return len(self._resources)

    def __repr__(self) -> str:
        return f"ManifestSet({len(self)} resources)"

    def select(
        self,
        kind: str | None = None,
        name: str | None = None,
        *,
        namespace: str | None | _Namespace = _Namespace.ANY,
        api_version: str | None = None,
    ) -> ManifestSet:
        """Filter in document order; omitted namespace searches all namespaces."""
        return ManifestSet(
            resource
            for resource in self
            if (kind is None or resource.kind == kind)
            and (name is None or resource.name == name)
            and (namespace is _Namespace.ANY or resource.namespace == namespace)
            and (api_version is None or resource.api_version == api_version)
        )

    def one(
        self,
        kind: str,
        name: str | None = None,
        *,
        namespace: str | None | _Namespace = _Namespace.ANY,
        api_version: str | None = None,
    ) -> Resource:
        """Require exactly one match, reporting identities on selection failure."""
        matches = self.select(kind, name, namespace=namespace, api_version=api_version)
        if len(matches) == 1:
            return next(iter(matches))
        selector = (
            f"kind={kind!r}, name={name!r}, namespace={namespace!r}, api_version={api_version!r}"
        )
        if not matches:
            identities = ", ".join(repr(resource) for resource in self) or "(none)"
            raise ResourceNotFoundError(f"No match for {selector}. Available: {identities}")
        identities = ", ".join(repr(resource) for resource in matches)
        raise AmbiguousResourceError(f"{len(matches)} matches for {selector}: {identities}")


class Resource:
    """A Kubernetes resource dictionary with identity and Pod template helpers."""

    def __init__(self, raw: dict[str, Any]) -> None:
        if not isinstance(raw, dict):
            raise InvalidOutputError("Each rendered document must be a resource dictionary")
        metadata = raw.get("metadata")
        if (
            not all(isinstance(raw.get(key), str) and raw[key] for key in ("apiVersion", "kind"))
            or not isinstance(metadata, dict)
            or not isinstance(metadata.get("name"), str)
            or not metadata["name"]
            or (
                metadata.get("namespace") is not None and not isinstance(metadata["namespace"], str)
            )
        ):
            raise InvalidOutputError(
                "Resource requires string apiVersion, kind, metadata.name and optional namespace"
            )
        self.raw = raw

    @property
    def api_version(self) -> str:
        """The declared API version."""
        return str(self.raw["apiVersion"])

    @property
    def kind(self) -> str:
        """The declared kind."""
        return str(self.raw["kind"])

    @property
    def name(self) -> str:
        """The declared metadata name."""
        return str(self.raw["metadata"]["name"])

    @property
    def namespace(self) -> str | None:
        """The declared namespace, or None; no default namespace is inferred."""
        value = self.raw["metadata"].get("namespace")
        return str(value) if value is not None else None

    def __repr__(self) -> str:
        return (
            f"Resource({self.api_version}, {self.kind}, {self.namespace or '<none>'}/{self.name})"
        )

    def containers(self, include_init: bool = False) -> Iterator[Container]:
        """Yield regular containers, followed by init containers when requested."""
        paths = {
            "Pod": ("spec",),
            "Deployment": ("spec", "template", "spec"),
            "StatefulSet": ("spec", "template", "spec"),
            "DaemonSet": ("spec", "template", "spec"),
            "ReplicaSet": ("spec", "template", "spec"),
            "ReplicationController": ("spec", "template", "spec"),
            "Job": ("spec", "template", "spec"),
            "CronJob": ("spec", "jobTemplate", "spec", "template", "spec"),
        }
        if self.kind not in paths:
            raise DeclarationError(f"{self!r}: containers() does not support this kind")
        spec = self.raw
        for key in paths[self.kind]:
            value = spec.get(key)
            if not isinstance(value, dict):
                raise DeclarationError(f"{self!r}: missing or invalid Pod template at {key}")
            spec = value
        for key in ("containers", "initContainers") if include_init else ("containers",):
            declarations = spec.get(key, [])
            if not isinstance(declarations, list):
                raise DeclarationError(f"{self!r}: {key} must be a list")
            for raw in declarations:
                yield Container(raw)


class Container:
    """A container declaration; repr never includes environment values."""

    def __init__(self, raw: dict[str, Any]) -> None:
        if not isinstance(raw, dict) or not isinstance(raw.get("name"), str) or not raw["name"]:
            raise DeclarationError("Container requires a string name")
        self.raw = raw

    @property
    def name(self) -> str:
        """The container name."""
        return str(self.raw["name"])

    @property
    def security_context(self) -> dict[str, Any]:
        """The declared securityContext, or an empty dictionary when omitted."""
        value = self.raw.get("securityContext", {})
        if not isinstance(value, dict):
            raise DeclarationError(f"{self!r}: securityContext must be a dictionary")
        return value

    @property
    def env(self) -> Environment:
        """Inspect explicit env entries, without resolving envFrom or references."""
        return Environment(self)

    def __repr__(self) -> str:
        return f"Container({self.name!r})"


class Environment:
    """Read one unambiguous explicit environment declaration at a time."""

    def __init__(self, container: Container) -> None:
        self._container = container

    def _entry(self, name: str) -> dict[str, Any]:
        declarations = self._container.raw.get("env", [])
        if not isinstance(declarations, list) or any(
            not isinstance(entry, dict) for entry in declarations
        ):
            raise DeclarationError(f"{self._container!r}: env must be a list of dictionaries")
        entries = [entry for entry in declarations if entry.get("name") == name]
        if len(entries) != 1:
            raise DeclarationError(
                f"{self._container!r}: expected one env declaration for {name!r}, "
                f"found {len(entries)}"
            )
        entry: dict[str, Any] = entries[0]
        return entry

    def value(self, name: str) -> str:
        """Return a directly declared literal string; reject valueFrom entries."""
        entry = self._entry(name)
        if "valueFrom" in entry or not isinstance(entry.get("value"), str):
            raise DeclarationError(
                f"{self._container!r}: env {name!r} requires a literal string value"
            )
        return str(entry["value"])

    def secret_ref(self, name: str) -> tuple[str, str]:
        """Return (Secret name, key), without reading the Secret."""
        entry = self._entry(name)
        source = entry.get("valueFrom")
        reference = source.get("secretKeyRef") if isinstance(source, dict) else None
        if (
            "value" in entry
            or not isinstance(source, dict)
            or set(source) != {"secretKeyRef"}
            or not isinstance(reference, dict)
            or not all(
                isinstance(reference.get(key), str) and reference[key] for key in ("name", "key")
            )
        ):
            raise DeclarationError(
                f"{self._container!r}: env {name!r} requires a secretKeyRef name/key"
            )
        return reference["name"], reference["key"]
