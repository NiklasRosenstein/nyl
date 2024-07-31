from pathlib import Path
from typing import cast
from loguru import logger
from structured_templates import TemplateEngine
from typer import Argument, Option
import yaml
from nyl.generator import reconcile_generator
from nyl.generator.dispatch import DispatchingGenerator
from nyl.profiles import ProfileManager
from kubernetes.config.incluster_config import load_incluster_config
from kubernetes.config.kube_config import load_kube_config
from kubernetes.client.api_client import ApiClient
from nyl.project.config import ProjectConfig
from nyl.secrets.config import SecretsConfig
from nyl.tools.types import Manifest, Manifests

# from nyl.resources import NylResource
# from nyl.resources.helmchart import HelmChart
# from nyl.resources.statefulsecret import StatefulSecret
from . import app


@app.command()
def template(
    paths: list[Path] = Argument(..., help="The YAML file(s) to render. Can be a directory."),
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

    project = ProjectConfig.load()
    if project.file:
        state_dir = project.file.parent / ".nyl"
    else:
        state_dir = Path(".nyl")

    secrets = SecretsConfig.load()

    template_engine = TemplateEngine(
        globals_={
            "secrets": secrets.provider,
            "random_password": _random_password,
            "bcrypt": _bcrypt,
        }
    )

    generator = DispatchingGenerator.default(
        git_repo_cache_dir=state_dir / "repo-cache",
        search_path=project.config.search_path,
        working_dir=Path.cwd(),
        client=ApiClient(),
    )

    manifests = load_manifests(paths)
    manifests = cast(Manifests, template_engine.evaluate(manifests))
    manifests = reconcile_generator(generator, manifests)

    print(yaml.safe_dump_all(manifests))

    # Warn out if any of the manifests do not have a namespace.
    for manifest in manifests:
        if (manifest.get("apiVersion"), manifest.get("kind")) != ("v1", "Namespace") and "namespace" not in manifest[
            "metadata"
        ]:
            logger.warning("Manifest {}/{} does not have a namespace", manifest["kind"], manifest["metadata"]["name"])


def load_manifests(paths: list[Path]) -> Manifests:
    """
    Load all manifests from a directory.
    """

    logger.trace("Loading manifests from paths: {}", paths)

    files = []
    for path in paths:
        if path.is_dir():
            for item in path.iterdir():
                if item.name.startswith("nyl-") or item.suffix != ".yaml" or not item.is_file():
                    continue
                files.append(item)
        else:
            files.append(path)

    logger.trace("Files to load: {}", files)

    result = Manifests([])
    for file in files:
        result.extend(map(Manifest, yaml.safe_load_all(file.read_text())))

    return result

    # package_metadata_file = directory / "nyl-package.yaml"
    # if package_metadata_file.exists():
    #     package_metadata = yaml.safe_load(package_metadata_file.read_text())
    # else:
    #     package_metadata = {}

    # # Fill in parameter defaults.
    # parameters = dict(parameters)
    # for key, value in package_metadata.get("parameters", {}).items():
    #     if "default" in value and key not in parameters:
    #         parameters[key] = value["default"]

    # k8s = DynamicClient(ApiClient())
    # templater = PackageTemplater(parameters)

    #     # Check if the resource has any references and try to resolve them. If a reference cannot be resolved, the
    #     # manifest must be skipped. We emit a warning and continue with the next manifest.
    #     refs = list(Reference.collect(manifest))
    #     if refs:
    #         skip_resource = False
    #         resolves = {}
    #         for ref in refs:
    #             # TODO: Determine the namespace to fall back to.
    #             try:
    #                 result = k8s.get(
    #                     resource=k8s.resources.get(api_version="v1", kind=ref.kind, group=""),
    #                     name=ref.name,
    #                     namespace=ref.namespace,  # TODO: Determine the namespace to backfill to.
    #                 )

    #                 value = result["data"][ref.key]
    #                 if value is None:
    #                     raise KeyError
    #                 assert isinstance(value, str)
    #                 resolves[str(ref)] = base64.b64decode(value.encode("ascii")).decode("utf-8")
    #             except NotFoundError:
    #                 logger.warning(
    #                     "Skipping resource {}/{} because its reference to {} could not be resolved.",
    #                     manifest["apiVersion"],
    #                     manifest["kind"],
    #                     ref,
    #                 )
    #                 skip_resource = True
    #                 break
    #             except KeyError:
    #                 logger.warning(
    #                     "Skipping resource {}/{} because its reference to {} could not be resolved (does not contain key {}).",
    #                     manifest["apiVersion"],
    #                     manifest["kind"],
    #                     ref,
    #                     ref.key,
    #                 )
    #                 skip_resource = True
    #                 break
    #         if skip_resource:
    #             continue
    #         manifest = Reference.sub(manifest, lambda ref: resolves[str(ref)])

    #     if manifest["apiVersion"] == NylResource.API_VERSION:
    #         resource = NylResource.load(manifest)

    #         match resource:
    #             case HelmChart():
    #                 manifests.extend(
    #                     template_helm_chart(resource, resource.release.namespace or namespace, Path(".nyl/repo-cache"))
    #                 )
    #             case Package():
    #                 manifests.extend(template_package_resource(resource, namespace))
    #             case StatefulSecret():
    #                 logger.warning("StatefulSecret not currently supported")
    #     else:
    #         manifests.append(manifest)

    # for manifest in manifests:
    #     if "namespace" not in manifest["metadata"]:
    #         logger.warning(
    #             "Manifest {}/{} does not have a namespace, injecting {}",
    #             manifest["kind"],
    #             manifest["metadata"]["name"],
    #             namespace,
    #         )
    #         manifest["metadata"]["namespace"] = namespace

    # return manifests


def _random_password(length: int = 32) -> str:
    """
    Generate a random password.
    """

    import secrets

    return secrets.token_urlsafe(length)


def _bcrypt(password: str) -> str:
    """
    Hash a password using bcrypt.
    """

    import bcrypt

    return bcrypt.hashpw(password.encode("utf-8"), bcrypt.gensalt()).decode("utf-8")
