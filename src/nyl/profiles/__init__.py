from dataclasses import dataclass
from pathlib import Path
from typing import Any
import warnings

from loguru import logger
import requests
import urllib3

from .tunnel import TunnelManager, TunnelSpec
from .kubeconfig import KubeconfigManager
from .config import ProfileConfig, SshTunnel

from nr.stream import Optional


@dataclass
class ActivatedProfile:
    kubeconfig: Path


@dataclass
class ProfileManager:
    """
    This class combines the [TunnelManager] and [KubeconfigManager] to provide a holistic interface for managing
    connections and configuration to Kubernetes clusters.
    """

    config: ProfileConfig
    tunnels: TunnelManager
    kubeconfig: KubeconfigManager

    def __enter__(self) -> "ProfileManager":
        self.tunnels.__enter__()
        return self

    def __exit__(self, *args: Any) -> None:
        self.tunnels.__exit__(*args)

    def activate_profile(self, profile_name: str) -> ActivatedProfile:
        """
        Ensure that the Kubernetes config and tunnel (if any) for the profile are available.
        """

        profile = self.config.profiles[profile_name]
        raw_kubeconfig = self.kubeconfig.get_raw_kubeconfig(profile_name, profile.kubeconfig)

        if profile.tunnel:
            forwardings = {"kubernetes": f"{raw_kubeconfig.api_host}:{raw_kubeconfig.api_port}"}
            tun_spec = get_tunnel_spec(self.config.file, profile_name, profile.tunnel)
            tun_status = Optional(self.tunnels.get_tunnel(tun_spec.locator)).map(lambda x: x[1]).or_else(None)
            is_restarted = tun_status is None or tun_status.status != "open"
            tun_status = self.tunnels.open_tunnel(tun_spec)
            tun_description = f" → {profile.tunnel.user}@{profile.tunnel.host} → {forwardings['kubernetes']}"

            raw_kubeconfig.api_host = "localhost"
            raw_kubeconfig.api_port = tun_status.local_ports["kubernetes"]

            # If the tunnel was only just started, it may need some time to connect.
            timeout = 30 if is_restarted else 2
        else:
            tun_description = ""
            timeout = 2

        api_server = f"https://{raw_kubeconfig.api_host}:{raw_kubeconfig.api_port}"
        logger.info("Checking for API server connectivity ({}{})", api_server, tun_description)
        _wait_for_api_server(api_server, timeout)

        return ActivatedProfile(
            kubeconfig=self.kubeconfig.get_updated_kubeconfig(
                profile_name=profile_name,
                path=raw_kubeconfig.path,
                context=raw_kubeconfig.context,
                api_host=raw_kubeconfig.api_host,
                api_port=raw_kubeconfig.api_port,
            )
        )

    @staticmethod
    def load() -> "ProfileManager":
        """
        Load the profile manager from the default configuration file.
        """

        config = ProfileConfig.load(ProfileConfig.find_config_file())
        tunnels = TunnelManager()
        kubeconfig = KubeconfigManager(cwd=config.file.parent, state_dir=config.file.with_name(".nyl") / "profiles")
        return ProfileManager(config, tunnels, kubeconfig)


def _wait_for_api_server(url: str, timeout: float) -> None:
    with warnings.catch_warnings():
        warnings.filterwarnings("ignore", category=urllib3.exceptions.InsecureRequestWarning)
        response = requests.get(url, verify=False, timeout=timeout)
    if response.json().get("kind") == "Status":
        # Looks well enough like a Kubernetes status object.
        return


def get_tunnel_spec(config_file: Path, profile: str, conf: SshTunnel) -> TunnelSpec:
    return TunnelSpec(
        locator=TunnelSpec.Locator(str(config_file), profile),
        forwardings={"kubernetes": TunnelSpec.Forwarding(host="localhost", port=6443)},
        user=conf.user,
        host=conf.host,
        identity_file=conf.identity_file,
    )
