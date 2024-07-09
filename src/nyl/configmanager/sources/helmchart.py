from dataclasses import dataclass, field
import hashlib
import json
import shlex
import subprocess
from tempfile import TemporaryDirectory
from typing import TYPE_CHECKING, Any, Literal

from pathlib import Path, PosixPath
from urllib.parse import parse_qs, urlparse
import yaml
from loguru import logger

if TYPE_CHECKING:
    from ..project import Project


@dataclass
class HelmChart:
    repository: str
    chart: str
    releaseName: str
    releaseNamespace: str
    version: str | None = None
    values: dict[str, Any] = field(default_factory=dict)
    set: dict[str, Any] = field(default_factory=dict)

    apiVersion: Literal["nyl/v1alpha1"] = "nyl/v1alpha1"
    kind: Literal["HelmChart"] = "HelmChart"

    _applyset_key: str | None = field(init=False, default=None)

    def get_applyset_key(self) -> str:
        assert self._applyset_key is not None, "HelmChart._applyset_key must be set before calling get_applyset_key()"
        return self._applyset_key

    def get_manifests(self, project: "Project") -> list[dict[str, Any]]:
        repository: str | None = self.repository
        chart: str | Path = self.chart
        assert repository is not None

        if repository.startswith("oci://"):
            chart = f"{repository}/{chart}"
            repository = None

        if repository and repository.startswith("nyl://"):
            # Find in the Nyl packages index.
            assert isinstance(chart, str)
            chart = project.find_package((repository[len("nyl://") :] + "/@charts/" + chart).lstrip("/"))

        if repository and repository.startswith("git+"):
            # Clone the repository and find the chart in the repository.
            parsed = urlparse(repository[len("git+") :])
            without_query_params = parsed._replace(query="").geturl()
            hashed = hashlib.md5(without_query_params.encode()).hexdigest()
            clone_dir = project.cache_directory / f"{hashed}-{PosixPath(parsed.path).name}"
            if clone_dir.exists():
                logger.info("Using cached clone of {} at {}", without_query_params, clone_dir)
                command = ["git", "fetch", "--tags"]
                cwd = clone_dir
            else:
                logger.info("Cloning {} to {}", without_query_params, clone_dir)
                command = ["git", "clone", without_query_params, str(clone_dir)]
                cwd = None
            subprocess.check_call(command, cwd=cwd)

            # todo: What if multiple HelmCharts refer to the same directory? Probably better to have a
            #       worktree per instance that refers to a ref of the repository.
            query = parse_qs(parsed.query)
            if "ref" in query:
                logger.info("Checking out ref {}", query["ref"][0])
                command = ["git", "checkout", query["ref"][0]]
                subprocess.check_call(command, cwd=clone_dir)

            repository = None
            chart = str(clone_dir / chart)

        with TemporaryDirectory() as tmp:
            values_file = Path(tmp) / "values.yaml"
            values_file.write_text(yaml.dump(self.values))

            command = [
                "helm",
                "template",
                *(
                    (
                        "--repo",
                        repository,
                    )
                    if repository
                    else ()
                ),
                *(
                    (
                        "--version",
                        self.version,
                        "--devel",
                    )
                    if self.version
                    else ()
                ),
                "--values",
                str(values_file),
                "--namespace",
                self.releaseNamespace,
                self.releaseName,
                str(chart),
            ]

            for key, value in self.set.items():
                command.append("--set")
                command.append(f"{key}={json.dumps(value)}")

            logger.info("Generating manifests with Helm: $ {}", " ".join(map(shlex.quote, command)))
            try:
                result = subprocess.run(command, capture_output=True, check=True)
            except subprocess.CalledProcessError as e:
                raise ValueError(f"Failed to get manifests: {e}\n\nstdout: {e.stdout}\n\nstderr: {e.stderr}")

            # TODO: Create namespace?

            manifests: list[dict[str, Any]] = list(filter(None, yaml.safe_load_all(result.stdout.decode())))

            for manifest in manifests:
                if "namespace" not in manifest["metadata"]:
                    logger.warning(
                        "Manifest {}/{} does not have a namespace, injecting {}",
                        manifest["kind"],
                        manifest["metadata"]["name"],
                        self.releaseNamespace,
                    )
                    manifest["metadata"]["namespace"] = self.releaseNamespace

            return manifests

        assert False
