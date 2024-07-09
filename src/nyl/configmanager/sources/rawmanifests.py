from dataclasses import dataclass
from typing import TYPE_CHECKING, Any


if TYPE_CHECKING:
    from ..project import Project, ManifestSource
else:
    ManifestSource = object


@dataclass
class RawManifests(ManifestSource):
    applyset_key: str
    manifests: list[dict[str, Any]]

    def get_applyset_key(self) -> str:
        return self.applyset_key

    def get_manifests(self, project: "Project") -> list[dict[str, Any]]:
        return self.manifests
