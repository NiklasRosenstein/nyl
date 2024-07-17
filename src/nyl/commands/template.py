import base64
import hashlib
from pathlib import Path, PosixPath
import shlex
import subprocess
from tempfile import TemporaryDirectory
from typing import Any
from urllib.parse import parse_qs, urlparse
from loguru import logger
from typer import Option
import yaml
from nyl.packages.reference import Reference
from nyl.packages.templating import PackageTemplater
from nyl.profiles import ProfileManager
from kubernetes.config.incluster_config import load_incluster_config
from kubernetes.config.kube_config import load_kube_config
from kubernetes.client.api_client import ApiClient
from kubernetes.dynamic import DynamicClient
from kubernetes.dynamic.exceptions import NotFoundError

from nyl.resources import NylResource
from nyl.resources.helmchart import HelmChart
from nyl.resources.package import Package
from nyl.resources.statefulsecret import StatefulSecret
from . import app


@app.command()
def template(
    package: Path = Option(Path("."), help="The package to render."),
    profile: str = Option("default", envvar="NYL_PROFILE", help="The Nyl profile to use."),
    in_cluster: bool = Option(
        False, help="Use the in-cluster Kubernetes configuration. The --profile option is ignored."
    ),
) -> None:
    """
    Render a package template into full Kubernetes resources.
    """

    if in_cluster:
        logger.info("Using in-cluster configuration.")
        load_incluster_config()
    else:
        with ProfileManager.load() as profiles:
            active = profiles.activate_profile(profile)
            logger.info(f"Using profile '{profile}' with kubeconfig '{active.kubeconfig}'.")
            load_kube_config(str(active.kubeconfig))

    manifests = template_package_dir(package, {})
    print(yaml.safe_dump_all(manifests))


def template_helm_chart(
    self: HelmChart,
    fallback_namespace: str,
    git_repo_cache_dir: Path,
) -> list[dict[str, Any]]:
    if self.chart.repository:
        if self.chart.path:
            raise ValueError("Cannot specify both `chart.repository` and `chart.path`.")
        if self.chart.git:
            raise ValueError("Cannot specify both `chart.repository` and `chart.git`.")
        if not self.chart.name:
            raise ValueError("`chart.name` must be set when `chart.repository` is set.")

        if self.chart.repository.startswith("oci://"):
            chart = f"{self.chart.repository}/{self.chart.name}"
            repository = None
        else:
            chart = self.chart.name
            repository = self.chart.repository

    elif self.chart.git:
        # Clone the repository and find the chart in the repository.
        parsed = urlparse(self.chart.git[len("git+") :])
        without_query_params = parsed._replace(query="").geturl()
        hashed = hashlib.md5(without_query_params.encode()).hexdigest()
        clone_dir = git_repo_cache_dir / f"{hashed}-{PosixPath(parsed.path).name}"
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

        chart = str(clone_dir / (self.chart.path or ""))

    elif self.chart.path:
        chart = self.chart.path

    else:
        print(self)
        raise ValueError("Either `chart.repository`, `chart.git` or `chart.path` must be set.")

    # if repository and repository.startswith("nyl://"):
    #     # Find in the Nyl packages index.
    #     assert isinstance(chart, str)
    #     chart = project.find_package((repository[len("nyl://") :] + "/@charts/" + chart).lstrip("/"))

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
                    self.chart.version,
                    "--devel",
                )
                if self.chart.version
                else ()
            ),
            "--values",
            str(values_file),
            "--namespace",
            self.release.namespace or fallback_namespace,
            self.release.name or self.name,
            str(chart),
        ]

        # for key, value in self.set.items():
        #     command.append("--set")
        #     command.append(f"{key}={json.dumps(value)}")

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
                    self.release.namespace or fallback_namespace,
                )
                manifest["metadata"]["namespace"] = self.release.namespace or fallback_namespace

        return manifests

    assert False


def template_package_resource(self: Package, namespace: str | None = None) -> None:
    # Find the package in the search path.
    search_path = [Path("../../packages")]  # TODO
    for path in search_path:
        package_dir = path / self.package
        if package_dir.exists():
            break
    else:
        raise ValueError(f"Could not find package '{self.package}' in search path {search_path}.")

    logger.info("Resolved package '{}' to directory '{}'", self.metadata.name, package_dir)

    return template_package_dir(package_dir, self.parameters, self.metadata.namespace or namespace)


def template_package_dir(
    directory: Path, parameters: dict[str, Any], namespace: str | None = None
) -> list[dict[str, Any]]:
    package_metadata_file = directory / "nyl-package.yaml"
    if package_metadata_file.exists():
        package_metadata = yaml.safe_load(package_metadata_file.read_text())
    else:
        package_metadata = {}

    # Fill in parameter defaults.
    parameters = dict(parameters)
    for key, value in package_metadata.get("parameters", {}).items():
        if "default" in value and key not in parameters:
            parameters[key] = value["default"]

    k8s = DynamicClient(ApiClient())
    templater = PackageTemplater(parameters)
    manifests: list[dict[str, Any]] = []
    for manifest in templater.render_directory(directory):
        # Check if the resource has any references and try to resolve them. If a reference cannot be resolved, the
        # manifest must be skipped. We emit a warning and continue with the next manifest.
        refs = list(Reference.collect(manifest))
        if refs:
            skip_resource = False
            resolves = {}
            for ref in refs:
                # TODO: Determine the namespace to fall back to.
                try:
                    result = k8s.get(
                        resource=k8s.resources.get(api_version="v1", kind=ref.kind, group=""),
                        name=ref.name,
                        namespace=ref.namespace,  # TODO: Determine the namespace to backfill to.
                    )

                    value = result["data"][ref.key]
                    if value is None:
                        raise KeyError
                    assert isinstance(value, str)
                    resolves[str(ref)] = base64.b64decode(value.encode("ascii")).decode("utf-8")
                except NotFoundError:
                    logger.warning(
                        "Skipping resource {}/{} because its reference to {} could not be resolved.",
                        manifest["apiVersion"],
                        manifest["kind"],
                        ref,
                    )
                    skip_resource = True
                    break
                except KeyError:
                    logger.warning(
                        "Skipping resource {}/{} because its reference to {} could not be resolved (does not contain key {}).",
                        manifest["apiVersion"],
                        manifest["kind"],
                        ref,
                        ref.key,
                    )
                    skip_resource = True
                    break
            if skip_resource:
                continue
            manifest = Reference.sub(manifest, lambda ref: resolves[str(ref)])

        if manifest["apiVersion"] == NylResource.API_VERSION:
            resource = NylResource.load(manifest)

            match resource:
                case HelmChart():
                    manifests.extend(
                        template_helm_chart(resource, resource.release.namespace or namespace, Path(".nyl/repo-cache"))
                    )
                case Package():
                    manifests.extend(template_package_resource(resource, namespace))
                case StatefulSecret():
                    logger.warning("StatefulSecret not currently supported")
        else:
            manifests.append(manifest)

    for manifest in manifests:
        if "namespace" not in manifest["metadata"]:
            logger.warning(
                "Manifest {}/{} does not have a namespace, injecting {}",
                manifest["kind"],
                manifest["metadata"]["name"],
                namespace,
            )
            manifest["metadata"]["namespace"] = namespace

    return manifests
