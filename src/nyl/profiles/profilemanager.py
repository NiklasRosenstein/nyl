import json
import os
from pathlib import Path
import random
import subprocess
from urllib.parse import urlparse
import yaml
from databind.json import load as json_deserialize
from .config import KubeconfigFromSsh, LocalKubeconfig, Profile, SshTunnel
from loguru import logger


class ProfileManager:
    """
    Helper class to manage Kubernetes connection profiles.
    """

    @staticmethod
    def from_config_file(path: Path, state_dir: Path | None = None) -> "ProfileManager":
        """
        Load profiles from a YAML configuration file.

        Args:
            path: Path to the YAML configuration file.
            state_dir: Directory to store state information, such as active tunnels. If not specified, the `.nyl`
                       directory next to the configuration file is used.
        """

        with path.open() as f:
            profiles = json_deserialize(yaml.safe_load(f), dict[str, Profile])
        if state_dir is None:
            state_dir = path.with_name(".nyl")
        return ProfileManager(profiles, state_dir)

    def __init__(self, profiles: dict[str, Profile], state_dir: Path) -> None:
        self._profiles = profiles
        self._state_dir = state_dir

    def init_profile(self, *, profile: str, force_refresh: bool) -> dict[str, str]:
        """
        Initialize a profile's local state, such as fetching the Kubeconfig file and creating an SSH tunnel (if needed).

        Args:
            profile: The name of the profile to use.
            force_refresh: If `True`, refresh the profile's state even if it is already initialized.
        Returns:
            A dictionary with environment variables to use when running commands with the profile.
        """

        logger.info("Initializing profile '{}'.", profile)

        if profile not in self._profiles:
            raise ValueError(f"Profile '{profile}' does not exist.")

        state_dir = self._state_dir / "profiles" / profile
        config = self._profiles[profile]

        match config.kubeconfig:
            case LocalKubeconfig():
                kubeconfig = os.getenv("KUBECONFIG", os.path.expanduser("~/.kube/config"))
                if Path(kubeconfig).exists():
                    raise FileNotFoundError(f"Kubeconfig file '{kubeconfig}' does not exist.")
                logger.info("Using local Kubeconfig file '{}'.", kubeconfig)
            case KubeconfigFromSsh():
                kubeconfig = state_dir / "kubeconfig.orig"
                if not Path(kubeconfig).exists() or force_refresh:
                    logger.info(
                        "Fetching Kubeconfig via SSH ({}@{}:{}).",
                        config.kubeconfig.user,
                        config.kubeconfig.host,
                        config.kubeconfig.path,
                    )
                    # Fetch the Kubeconfig file.
                    kubeconfig.parent.mkdir(parents=True, exist_ok=True)
                    command = [
                        "ssh",
                        f"{config.kubeconfig.user}@{config.kubeconfig.host}",
                        "cat",
                        config.kubeconfig.path,
                    ]
                    kubeconfig_content = subprocess.check_output(command, text=True)
                    Path(kubeconfig).write_text(kubeconfig_content)
                else:
                    logger.info("Reusing cached Kubeconfig.", kubeconfig)
                    logger.debug("Cached Kubeconfig at '{}'.", kubeconfig)
            case _:
                raise ValueError(f"Unsupported Kubeconfig type: {config.kubeconfig.__class__.__name__}")

        # Lookup the Kubernetes cluster API address from the Kubeconfig file.
        kubeconfig_data = yaml.safe_load(Path(kubeconfig).read_text())
        context_name = config.kubeconfig.context
        if context_name is None:
            context_name = kubeconfig_data["current-context"]
        cluster_name = [c["name"] for c in kubeconfig_data["contexts"] if c["name"] == context_name][0]
        cluster = [c["cluster"] for c in kubeconfig_data["clusters"] if c["name"] == cluster_name][0]
        kubernetes_api = cluster["server"]
        kubernetes_api_host = urlparse(kubernetes_api).netloc
        logger.debug("Kubernetes API from Kubeconfig: '{}'", kubernetes_api)

        tunnel_state_file = state_dir / "tunnel.state"

        # Modified Kubeconfig file to use the local tunnel port/with the selected context.
        kubeconfig = state_dir / "kubeconfig.local"
        kubeconfig_data["current-context"] = context_name

        match config.tunnel:
            case SshTunnel():
                tunnel_port = ensure_tunnel(
                    tunnel_state_file,
                    kubernetes_api_host,
                    config.tunnel.user,
                    config.tunnel.host,
                    config.tunnel.identity_file,
                )

                # Update the Kubeconfig file to use the local tunnel port.
                cluster["server"] = f"https://localhost:{tunnel_port}"
            case _:
                terminate_tunnel(tunnel_state_file)

        kubeconfig.write_text(yaml.dump(kubeconfig_data))

        env = {"KUBECONFIG": str(kubeconfig.absolute())}
        return env

    def close_tunnel(self, profile: str) -> None:
        """
        Close the SSH tunnel for a profile.

        Args:
            profile: The name of the profile to use.
        """

        if profile not in self._profiles:
            raise ValueError(f"Profile '{profile}' does not exist.")

        state_dir = self._state_dir / "profiles" / profile
        tunnel_state_file = state_dir / "tunnel.state"
        terminate_tunnel(tunnel_state_file)

    def close_tunnels(self) -> None:
        """
        Close all SSH tunnels.
        """

        profiles_dir = self._state_dir / "profiles"
        if not profiles_dir.exists():
            return
        for profile_dir in profiles_dir.iterdir():
            tunnel_state_file = profile_dir / "tunnel.state"
            terminate_tunnel(tunnel_state_file)


def ensure_tunnel(state_file: Path, kubernetes_api_host: str, user: str, host: str, identity_file: str | None) -> int:
    """
    Ensure an SSH tunnel to the Kubernetes API is established.

    Args:
        state_file: Path to the state file to store the tunnel information.
        kubernetes_api_host: The hostname of the Kubernetes API.
        user: The SSH user to use.
        host: The SSH host to connect to.
        identity_file: The SSH identity file to use.

    Returns:
        The local port number the tunnel is established on.
    """

    config = {
        "kubernetes_api_host": "localhost",
        "user": "user",
        "host": "host",
        "identity_file": "identity_file",
    }

    if state_file.exists() and (state_text := state_file.read_text()):
        state = json.loads(state_text)
        if state["config"] != config:
            logger.info("SSH tunnel configuration changed. Terminating existing tunnel.")
            terminate_tunnel(state_file)
        elif state["pid"] is not None:
            try:
                os.kill(state["pid"], 0)
            except ProcessLookupError:
                logger.warning("Stale SSH tunnel process found. Terminating.")
                terminate_tunnel(state_file)
            else:
                logger.info("Reusing existing SSH tunnel to Kubernetes API on local port {}.", state["port"])
                return state["port"]

    # TODO: Better method to find an available port.
    port = random.randint(10000, 20000)

    command = ["ssh", "-N", "-L", f"{port}:{kubernetes_api_host}", f"{user}@{host}"]
    if identity_file is not None:
        command.extend(["-i", identity_file])

    logger.info("Establishing SSH tunnel to Kubernetes API on local port {}.", port)
    process = subprocess.Popen(command)
    state = {"pid": process.pid, "port": port, "config": config}
    state_file.write_text(json.dumps(state))

    # TODO: Wait for a successful HTTPS connection to the Kubernetes API.
    return port


def terminate_tunnel(state_file: Path) -> None:
    """
    Terminate the SSH tunnel to the Kubernetes API.

    Args:
        state_file: Path to the state file storing the tunnel information.
    """

    if state_file.exists():
        with state_file.open() as f:
            state = json.load(f)
        if state["pid"] is not None:
            try:
                os.kill(state["pid"], 15)
            except ProcessLookupError:
                pass
            else:
                logger.info("Terminated SSH tunnel to Kubernetes API on local port {}.", state["port"])
        state_file.unlink()
