from dataclasses import dataclass
import os
from pathlib import Path
import subprocess
from typing import Any
from urllib.parse import urlparse
import yaml
from .config import KubeconfigFromSsh, LocalKubeconfig
from loguru import logger


@dataclass(kw_only=True)
class GetRawKubeconfigResult:
    path: Path
    context: str
    api_host: str
    api_port: int


class KubeconfigManager:
    """
    Helper class to manage Kubernetes configuration files for profiles.

    Args:
        cwd: The working directory for the Kubeconfig manager. This is used to resolve relative paths.
        state_dir: The directory where the Kubeconfig files are stored. Usually `.nyl/profiles`.
    """

    def __init__(self, cwd: Path, state_dir: Path) -> None:
        self._cwd = cwd
        self._state_dir = state_dir

    def get_raw_kubeconfig(
        self,
        profile_name: str,
        source: LocalKubeconfig | KubeconfigFromSsh,
        force_refresh: bool = False,
    ) -> GetRawKubeconfigResult:
        """
        Return the Kubeconfig file for a profile.

        For a `LocalKubeconfig`, the Kubeconfig file is expected to be in the default location or a custom path
        specified in the environment and the path to it will be returned. For a `KubeconfigFromSsh`, the Kubeconfig
        file is fetched via SSH.

        Args:
            profile_name: The name of the Kubernetes profile.
            source: The source of the Kubeconfig file.
            force_refresh: If `True`, refresh the Kubeconfig file even if it is already cached.
        Returns:
            The path to the Kubeconfig file and the API host and port.
        """

        # Load the original Kubeconfig.
        match source:
            case LocalKubeconfig():
                raw_kubeconfig = Path(os.getenv("KUBECONFIG", os.path.expanduser("~/.kube/config")))
                if Path(raw_kubeconfig).exists():
                    raise FileNotFoundError(f"Kubeconfig file '{raw_kubeconfig}' does not exist.")
                logger.info("Using local Kubeconfig file '{}'.", raw_kubeconfig)

            case KubeconfigFromSsh():
                raw_kubeconfig = self._state_dir / profile_name / "kubeconfig.orig"
                if not Path(raw_kubeconfig).exists() or force_refresh:
                    logger.info(
                        "Fetching Kubeconfig via SSH ({}@{}:{}).",
                        source.user,
                        source.host,
                        source.path,
                    )
                    # Fetch the Kubeconfig file.
                    raw_kubeconfig.parent.mkdir(parents=True, exist_ok=True)
                    command = [
                        "ssh",
                        f"{source.user}@{source.host}",
                        "cat",
                        source.path,
                    ]
                    if source.identity_file:
                        command[1:1] = ["-i", str(self._cwd / source.identity_file)]
                    kubeconfig_content = subprocess.check_output(command, text=True)
                    Path(raw_kubeconfig).write_text(kubeconfig_content)
                else:
                    logger.info("Reusing cached Kubeconfig.", raw_kubeconfig)
                    logger.debug("Cached Kubeconfig at '{}'.", raw_kubeconfig)
            case _:
                raise ValueError(f"Unsupported Kubeconfig type: {source.__class__.__name__}")

        # Find the Kubernetes API host and port.
        kubeconfig_data = yaml.safe_load(raw_kubeconfig.read_text())
        kubeconfig_data = _trim_to_context(kubeconfig_data, source.context)
        server = kubeconfig_data["clusters"][0]["cluster"]["server"]
        api_host = urlparse(server).hostname
        api_port = urlparse(server).port or 443

        return GetRawKubeconfigResult(
            path=raw_kubeconfig, context=kubeconfig_data["current-context"], api_host=api_host, api_port=api_port
        )

    def get_updated_kubeconfig(
        self, *, profile_name: str, path: Path, context: str, api_host: str, api_port: int
    ) -> Path:
        kubeconfig_data = yaml.safe_load(path.read_text())
        kubeconfig_data = _trim_to_context(kubeconfig_data, context)

        # TODO: Do we need to support the Kubernetes API hosted on a subpath?
        kubeconfig_data["clusters"][0]["cluster"]["server"] = f"https://{api_host}:{api_port}"

        final_kubeconfig = self._state_dir / profile_name / "kubeconfig.local"
        final_kubeconfig.parent.mkdir(parents=True, exist_ok=True)
        final_kubeconfig.write_text(yaml.dump(kubeconfig_data))

        return final_kubeconfig


def _trim_to_context(kubeconfig: dict[str, Any], context: str | None) -> dict[str, Any]:
    """
    Trim a Kubeconfig down to a single context. If *context* is `None`, it will be trinmed to the current context.
    """

    if context is None:
        context = kubeconfig["current-context"]
    else:
        kubeconfig["current-context"] = context

    kubeconfig["contexts"] = [c for c in kubeconfig["contexts"] if c["name"] == context]
    if not kubeconfig["contexts"]:
        raise ValueError(f"Context '{context}' not found in Kubeconfig file.")
    context_data = kubeconfig["contexts"][0]

    kubeconfig["clusters"] = [c for c in kubeconfig["clusters"] if c["name"] == context_data["context"]["cluster"]]
    if not kubeconfig["clusters"]:
        raise ValueError(f"Cluster '{context_data['context']['cluster']}' not found in Kubeconfig file.")

    kubeconfig["users"] = [u for u in kubeconfig["users"] if u["name"] == context_data["context"]["user"]]
    if not kubeconfig["users"]:
        raise ValueError(f"User '{context_data['context']['user']}' not found in Kubeconfig file.")

    return kubeconfig
