"""
Manage cluster tunnels configured in `nyl-profiles.yaml`.
"""

from typing import Optional

from loguru import logger
from typer import Argument
from rich.console import Console
from rich.table import Table

from nyl.profiles.config import ProfileConfig
from nyl.profiles.tunnelmanager import TunnelManager, TunnelSpec
from nyl.utils import new_typer


app = new_typer(name="tun", help=__doc__)


@app.command()
def status(all: bool = False) -> None:
    """
    Show the status of all tunnels.
    """

    config = ProfileConfig.load(ProfileConfig.find_config_file())

    table = Table()
    table.add_column("Profile", justify="right", style="cyan")
    table.add_column("Tunnel ID")
    table.add_column("Status")
    table.add_column("Proxy")
    table.add_column("Forwardings")

    with TunnelManager() as manager:
        for spec, status in manager.get_tunnels():
            if not all and spec.locator.config_file != str(config.file):
                continue

            profile = spec.locator.profile
            if all:
                profile = f"{profile} ({spec.locator.config_file})"

            forwardings = ", ".join(
                f"{status.local_ports.get(k, '?')}:{v.host}:{v.port}" for k, v in spec.forwardings.items()
            )
            table.add_row(
                profile,
                status.id,
                status.status,
                f"{spec.user}@{spec.host}",
                forwardings,
            )

    Console().print(table)


@app.command()
def open(profile_name: str) -> None:
    """
    Open a tunnel to the cluster targeted by the profile.
    """

    config = ProfileConfig.load(ProfileConfig.find_config_file())

    try:
        profile = config.profiles[profile_name]
    except KeyError:
        logger.error("Profile '{}' not found in '{}'.", profile_name, config.file)
        exit(1)

    if not profile.tunnel:
        raise ValueError(f"Profile '{profile_name}' does not have a tunnel configuration.")

    # TODO: Know the Kubernetes host/port to forward.
    spec = TunnelSpec(
        locator=TunnelSpec.Locator(str(config.file), profile_name),
        forwardings={"kubernetes": TunnelSpec.Forwarding(host="localhost", port=6443)},
        user=profile.tunnel.user,
        host=profile.tunnel.host,
        identity_file=profile.tunnel.identity_file,
    )

    with TunnelManager() as manager:
        manager.open_tunnel(spec)


@app.command()
def close(profile_name: Optional[str] = Argument(None)) -> None:
    """
    Close all tunnels or the tunnel for a specific profile.
    """

    if profile_name is None:
        with TunnelManager() as manager:
            for spec, _status in manager.get_tunnels():
                manager.close_tunnel(spec.locator)
        return

    config = ProfileConfig.load(ProfileConfig.find_config_file())
    with TunnelManager() as manager:
        manager.close_tunnel(TunnelSpec.Locator(str(config.file), profile_name))
