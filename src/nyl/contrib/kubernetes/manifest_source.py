
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path
import subprocess
import textwrap
from typing import Any
import yaml
import logging

logger = logging.getLogger(__name__)


@dataclass
class ManifestSourceError(Exception):
    """
    Represents an error that occurred while generating Kubernetes manifests from a source.
    """

    source: "ManifestSource"
    message: str

    def __str__(self) -> str:
        if "\n" in self.message:
            message = "\n\n" + textwrap.indent(self.message, "  ")
        else:
            message = f"{self.message}"
        return f"Error generating manifests from source {self.source}: {message}"


class ManifestSource(ABC):
    """
    Represents a source of Kubernetes manifests that can be applied to a cluster.
    """

    @abstractmethod
    def generate(self) -> str:
        """
        Generate the Kubernetes manifests from the source.

        Returns:
            The generated Kubernetes manifests as a YAML-string.
        """
        raise NotImplementedError


@dataclass
class RawManifestSource(ManifestSource):
    """
    Represents raw Kubernetes manifests from a directory.
    """

    manifest_files: list[Path | str] | None = None
    """
    A list of paths to the manifest files. If not provided, all files in the directory will be used.
    """

    manifest_dir: Path | None = None
    """
    A directory containing the manifest files. If not provided, the current working directory will be used.
    If no manifest files are specified, this must be provided and all `*.yaml` files in the directory will be used.
    """

    recursive: bool = False
    """
    Whether to search for manifest files recursively in the directory (only if `manifest_files` is not provided).
    """

    def generate(self) -> str:
        if self.manifest_files is not None:
            logger.info("Reading raw Kubernetes manifests (%d files)", len(self.manifest_files))
            manifest_dir = self.manifest_dir or Path.cwd()
            files = [manifest_dir / file for file in self.manifest_files]
        elif self.manifest_dir is not None:
            logger.info("Reading raw Kubernetes manifests from directory '%s'", self.manifests_dir)
            files = list(self.manifests_dir.glob("**/*.yaml" if self.recursive else "*.yaml"))
        else:
            raise ManifestSourceError(self, "Either 'manifest_files' or 'manifest_dir' must be provided")

        results = []
        for file in files:
            logger.debug("Checking that manifest file '%s' is valid YAML", file)
            with open(file, "r") as f:
                content = f.read()
            try:
                yaml.safe_load(content)
            except yaml.YAMLError as e:
                raise ManifestSourceError(self, f"Manifest file '{file}' is not valid YAML: {e}")
            results.append(content)

        return '\n---\n'.join(results)


@dataclass
class HelmManifestSource(ManifestSource):
    """
    Represents Helm chart manifests.
    """

    release_name: str
    namespace: str
    chart_dir: Path | None = None
    chart_name: str | None = None
    repository: str | None = None
    version: str | None = None
    devel: bool = False
    values: dict[str, Any] | None = None

    def generate(self) -> str:
        if self.chart_dir is not None:
            chart = str(self.chart_dir.absolute())
        elif self.chart_name is not None:
            chart = self.chart_name
        else:
            raise ManifestSourceError(self, "Either 'chart_dir' or 'chart_name' must be provided")

        logger.info("Generating manifests from Helm chart (%s, repository=%r, version=%r)", chart, self.repository, self.version)

        command = [
            "helm",
            "template",
            chart,
            self.release_name,
            "--namespace",
            self.namespace,
            *(("--repo", self.repository) if self.repository else ()),
            *(("--version", self.version) if self.version else ()),
            *(("--devel",) if self.devel else ()),
            "--debug",
        ]
        logger.debug("Running command: %s", command)

        # print(subprocess.run(command, capture_output=, text=True).stdout)
        if (result := subprocess.run(command, capture_output=True, text=True)).returncode != 0:
            raise ManifestSourceError(self, f"Failed to generate manifests: {result.stderr}")

        return result.stdout
