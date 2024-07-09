from dataclasses import dataclass
import logging
from pathlib import Path
from typing import Any, Literal, Protocol

from nyl.configmanager.sources.helmchart import HelmChart
from nyl.configmanager.sources.rawmanifests import RawManifests
from databind.json import load as deser
from nyl.configmanager.sopsfile import SopsFile
import yaml
import jinja2

logger = logging.getLogger(__name__)
ManifestProviders = HelmChart
SecretProviders = SopsFile


class ManifestSource(Protocol):
    def get_applyset_key(self) -> str: ...
    def get_manifests(self, project: "Project") -> list[dict[str, Any]]: ...


@dataclass
class Project:
    # todo: provide a way to override the (globally unique) applyset name?
    sources: list[str]
    " List of files that contain sources for Kubernetes manifests. "

    packages: list[str]
    " List of supported package sources. "

    secrets: list[SecretProviders]
    " List of sources for secrets. "

    mode: Literal["ApplySet", "Raw"] = "ApplySet"
    """
    The mode used to apply manifests. If 'ApplySet', we use kubectl's `--applyset` and `--prune` flags. Otherwise,
    we simply do a `kubectl apply` with the manifests. In order to enable namespace-spanning applysets, Nyl creates
    its own CRD to act as a parent resource for the applyset.
    """

    getKubeconfig: str | None = None
    tunnel: str | None = None

    _directory: Path | None = None

    @property
    def directory(self) -> Path:
        assert self._directory is not None, "Project.init() must be called first"
        return self._directory

    @property
    def cache_directory(self) -> Path:
        return self.directory.joinpath(".nyl")

    def init(self, relative_to: Path) -> None:
        self._directory = relative_to

    def load_sources(self) -> list[ManifestSource]:
        assert self._directory is not None, "Project.init() must be called first"

        env = jinja2.Environment(undefined=jinja2.StrictUndefined)
        secrets = env.globals["Secrets"] = {}
        for secret in self.secrets:
            secrets[secret.name] = secret.get_lookup_resolver()

        sources: list[ManifestSource] = []
        for source in self.sources:
            logger.info("Loading source %s", source)
            template = env.from_string(self._directory.joinpath(source).read_text())
            template.filename = str(self._directory.joinpath(source))

            raw_manifests = RawManifests(Path(source).stem, [])

            for manifest in yaml.safe_load_all(template.render()):
                if manifest["apiVersion"] == "nyl/v1alpha1":
                    # We want to keep the generated manifests and raw manifests in the same order.
                    if raw_manifests.manifests:
                        sources.append(raw_manifests)
                        raw_manifests = RawManifests(raw_manifests.applyset_key, [])

                    provider = deser(manifest, ManifestProviders)
                    provider._applyset_key = raw_manifests.applyset_key
                    sources.append(provider)
                elif manifest["apiVersion"].startswith("nyl/"):
                    raise ValueError(f"Unknown apiVersion: {manifest['apiVersion']}")
                else:
                    raw_manifests.manifests.append(manifest)

            if raw_manifests.manifests:
                sources.append(raw_manifests)

        return sources

    def find_package(self, name: str) -> Path:
        assert self._directory is not None, "Project.init() must be called first"

        paths = [self._directory.joinpath(name).resolve() for name in self.packages]

        for package in paths:
            package_path = package.joinpath(package).joinpath(name)
            if package_path.is_dir():
                return package_path

        raise ValueError(f"Package not found: '{name}' in\n-" + "\n-".join(str(p) for p in paths))
