from pathlib import Path
from typing import Any
from loguru import logger
from typer import Option
import yaml
from nyl.packages.templating import PackageTemplater
from nyl.profiles import ProfileManager
from kubernetes.config.incluster_config import load_incluster_config
from kubernetes.config.kube_config import load_kube_config

from nyl.resources import NylResource
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

    templater = PackageTemplater()
    manifests: list[dict[str, Any]] = []
    for manifest in templater.render_directory(package):
        # Check if the resource has any references and try to resolve them. If a reference cannot be resolved, the
        # manifest must be skipped. We emit a warning and continue with the next manifest.
        # TODO

        if manifest["apiVersion"] == NylResource.API_VERSION:
            resource = NylResource.load(manifest)
            print(resource)
        else:
            manifests.append(manifest)

    print(yaml.safe_dump_all(manifests))
