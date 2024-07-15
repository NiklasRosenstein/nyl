import argparse
from contextlib import ExitStack
import os
from pathlib import Path
import shlex
import subprocess
import sys
from textwrap import indent
from typing import Any, Optional
from concurrent.futures import Future, ThreadPoolExecutor, as_completed

from nyl.appmanager.appmanager import ApplicationManager
from nyl.appmanager.crd import generate_application_resource
from nyl.kubectl import Kubectl
from nyl.profiles.kubeconfig import KubeconfigManager
from nyl.tunnel import Tunnel
import yaml
from loguru import logger
from nyl.configmanager.project import ManifestSource, Project
from .utils import deser_yaml


from typer import Option, Typer

app = Typer(pretty_exceptions_enable=False, no_args_is_help=True)


class Nyl:
    def __init__(self) -> None:
        self._estack = ExitStack()

        self._app = Typer(pretty_exceptions_enable=False, no_args_is_help=True)
        self._app.callback()(self._init)
        self._app.command()(self.env)
        self._app.command()(self.run)
        self._app.command()(self.close_tunnels)
        self._app.command()(self.ls)
        self._app.command()(self.apply)

        # Set in _init()
        self._project: Project
        self._skip_tunnel: bool

        # Caches
        self._profiles_config_file: Path | None = None
        self._profile_manager: KubeconfigManager | None = None
        self._tunnel: Tunnel | None = None
        self._kubectl_cache: Kubectl | None = None
        self._apps_cache: ApplicationManager | None = None

    def __enter__(self) -> "Nyl":
        return self

    def __exit__(self, exc_type, exc_value, traceback) -> None:
        self._estack.close()

    def _init(
        self,
        profiles: Optional[Path] = Option(
            None,
            help="Path to the `nyl-profiles.yaml` to use. If not set, it will be searched in the current directory "
            "and its parents.",
        ),
        config: Path = Option(Path("nyl.yaml"), help="Path to the Nyl configuration file."),
        skip_tunnel: bool = Option(False, help="Skip creating a tunnel."),
    ) -> None:
        self._profiles_config_file = profiles
        self._project = deser_yaml(Project, config)
        self._skip_tunnel = skip_tunnel

    def _connect(self) -> Kubectl:
        """
        Ensure that we can connect to the cluster, e.g. by creating a tunnel.
        """

        if self._kubectl_cache is not None:
            return self._kubectl_cache

        kubectl = Kubectl()

        if self._project.getKubeconfig and not self._skip_tunnel:
            logger.info("Getting kubeconfig with command $ {command}", command=self._project.getKubeconfig)
            kubeconfig_content = subprocess.check_output(self._project.getKubeconfig, shell=True, text=True)
            kubectl.set_kubeconfig(kubeconfig_content)

        if self._project.tunnel and not self._skip_tunnel:
            tunnel = Tunnel(*self._project.tunnel.split())
            self._estack.enter_context(tunnel)
            connect_retries = 5

        if not self._skip_tunnel:
            logger.info("Connect to the cluster ...")
            cluster_info = indent(
                kubectl.cluster_info(retries=connect_retries, retry_interval_seconds=5),
                "\t// ",
                lambda _: True,
            )
            logger.info("Successfully connected to the cluster\n{cluster_info}", cluster_info=cluster_info)

        self._kubectl_cache = kubectl
        return kubectl

    def _get_profiles_config_file(self) -> Path:
        profiles = self._profiles_config_file
        if profiles is None:
            # TODO: Move into utility function.
            profiles = Path("nyl-profiles.yaml").absolute()
            previous_profiles: Path | None = None
            while profiles != previous_profiles:
                if profiles.exists():
                    break
                previous_profiles = profiles
                profiles = profiles.parent.parent / profiles.name
            else:
                raise ValueError("Could not find `nyl-profiles.yaml` from the current directory.")
            self._profiles_config_file = profiles

        return profiles

    def _get_profile_manager(self) -> KubeconfigManager:
        if self._profile_manager is None:
            profiles = self._get_profiles_config_file()
            self._profile_manager = KubeconfigManager.from_config_file(profiles)
        return self._profile_manager

    @property
    def _apps(self) -> ApplicationManager:
        if self._apps_cache is not None:
            return self._apps_cache

        from nyl.appmanager import ApplicationManager
        from kubernetes.config import load_kube_config
        from kubernetes.client import ApiClient

        kubectl = self._connect()
        load_kube_config(config_file=kubectl.env["KUBECONFIG"])
        manager = ApplicationManager(ApiClient(), kubectl)

        self._apps_cache = manager
        return manager

    def env(self, profile: str = Option("default")) -> None:
        """
        Print environment variables for connecting with a Kubernetes cluster profile.

        If the profile uses a tunnel to connect to the cluster, the tunnel is also created.
        """

        manager = self._get_profile_manager()
        env = manager.init_profile(profile=profile, force_refresh=False)
        for key, value in sorted(env.items()):
            print(f"export {key}={shlex.quote(value)}")

    def run(self, command: list[str], profile: str = Option("default")) -> None:
        """
        Run a command in the context of a Nyl profile
        """

        manager = self._get_profile_manager()
        env = manager.init_profile(profile=profile, force_refresh=False)
        sys.exit(subprocess.run(command, env={**os.environ, **env}).returncode)

    def close_tunnels(self) -> None:
        manager = self._get_profile_manager()
        manager.close_tunnels()

    def ls(self) -> None:
        """
        List applications in the cluster.
        """

        print(self._apps.list_applications())

    def apply(self, app: str) -> None:
        """
        Apply an application to the cluster.
        """

        pass  # TODO

    def main(self) -> None:
        self._app()


def main() -> None:
    with Nyl() as nyl:
        nyl.main()
    exit()


if __name__ == "__main__":
    main()


parser = argparse.ArgumentParser()
parser.add_argument("-c", "--config", type=Path, help="Path to the configuration file.", default=Path("nyl.yaml"))
parser.add_argument("-s", "--skip-tunnel", action="store_true", help="Skip creating a tunnel.")
parser.add_argument("-a", "--apply", action="store_true", help="Apply the manifests to the cluster.")
# parser.add_argument("-d", "--diff", action="store_true", help="Diff the manifests against the cluster.")
parser.add_argument("-o", "--output", help="Output the manifests to a file. Can be - for stdout.")
parser.add_argument("--run", help="Run a command in the context of the tunneled cluster.")
parser.add_argument(
    "--force-conflicts", action="store_true", help="Force apply even if there are conflicts between field managers."
)
parser.add_argument("sources", nargs="*", help="Select the sources to load. If not provided, all sources are loaded.")


def _main() -> None:
    stack = ExitStack()
    with stack:
        main_internal(stack)


def main_internal(stack: ExitStack) -> None:
    args = parser.parse_args()
    project = deser_yaml(Project, args.config)
    project.init(args.config.absolute().parent)
    connect_retries = 0

    kubectl = Kubectl()
    stack.enter_context(kubectl)

    if args.run and (args.skip_tunnel or not project.tunnel):
        raise ValueError("Cannot run a command without creating a tunnel")

    # Load the sources.
    if args.sources:
        unlisted_sources = set(args.sources) - set(project.sources)
        if unlisted_sources:
            raise ValueError(f"Unrecognized sources: {unlisted_sources}")
        project.sources = args.sources
    sources = project.load_sources()
    if not sources:
        raise ValueError("No sources found")

    if project.getKubeconfig and not args.skip_tunnel:
        logger.info("Getting kubeconfig with command $ {command}", command=project.getKubeconfig)
        kubeconfig_content = subprocess.check_output(project.getKubeconfig, shell=True, text=True)
        kubectl.set_kubeconfig(kubeconfig_content)

    if project.tunnel and not args.skip_tunnel:
        tunnel = Tunnel(*project.tunnel.split())
        stack.enter_context(tunnel)
        connect_retries = 5

    if not args.skip_tunnel:
        logger.info("Connect to the cluster ...")
        cluster_info = indent(
            kubectl.cluster_info(retries=connect_retries, retry_interval_seconds=5), "\t// ", lambda _: True
        )
        logger.info("Successfully connected to the cluster\n{cluster_info}", cluster_info=cluster_info)

    if args.run:
        logger.info("Running command in the context of the cluster")
        sys.exit(subprocess.run(args.run, shell=True, env={**os.environ, **kubectl.env}).returncode)

    # if project.mode == "ApplySet":
    #     # todo: avoid doing this over and over again.
    #     logger.info("Ensuring ApplySet CRD is present.")
    #     kubectl.apply([APPLICATION_CRD])

    from nyl.appmanager import ApplicationManager
    from kubernetes.config import load_kube_config
    from kubernetes.client import ApiClient

    load_kube_config(config_file=kubectl.env["KUBECONFIG"])
    manager = ApplicationManager(ApiClient(), kubectl)
    manager.list_applications()
    exit()

    # Evaluate the sources and combine them into groups by source keys.
    all_manifests: list[dict[str, Any]] = []
    manifest_groups: dict[str, list[dict[str, Any]]] = {}

    futures: dict[Future[list[dict[str, Any]]], ManifestSource] = {}
    with ThreadPoolExecutor() as executor:
        for source in sources:
            futures[executor.submit(source.get_manifests, project)] = source
    for future in as_completed(futures):
        source = futures[future]
        all_manifests.extend(future.result())
        manifest_groups.setdefault(source.get_applyset_key(), []).extend(future.result())

    if args.apply:  # or args.diff:
        for applyset_key, manifests in manifest_groups.items():
            if project.mode == "ApplySet":
                applyset = generate_application_resource(
                    applyset_key,
                    contains_resources={
                        (
                            f"{manifest['kind']}."
                            + (manifest["apiVersion"].split("/")[0] if "/" in manifest["apiVersion"] else "")
                        ).rstrip(".")
                        for manifest in manifests
                        if manifest["kind"] != "ApplySet"
                    },
                    kubectl_version=kubectl.version()["gitVersion"],
                )
                kubectl.apply([applyset])

            kubectl.apply(
                manifests,
                force_conflicts=args.force_conflicts,
                applyset=f"applyset/{applyset_key}" if project.mode == "ApplySet" else None,
                prune=project.mode == "ApplySet",
            )

        sys.exit(0)

        # elif args.diff:
        #     logger.info("Diffing manifests")
        #     sys.exit(subprocess.run(f"kubectl diff -f {manifest_file}", shell=True, env=env).returncode)
    elif args.output:
        if args.output == "-":
            logger.info("Outputting manifests to stdout")
            print(yaml.safe_dump_all(all_manifests))
        else:
            logger.info("Outputting manifests to '%s'", args.output)
            with open(args.output, "w") as f:
                f.write(yaml.safe_dump_all(all_manifests))


if __name__ == "__main__":
    main()
