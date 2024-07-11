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
    table.add_column("Tunnel ID", justify="right", style="cyan")
    table.add_column("Profile")
    table.add_column("Status")
    table.add_column("Proxy")
    table.add_column("Forwardings")

    with TunnelManager() as manager:
        for spec, status in manager.get_tunnels():
            is_current = spec.locator.split(":")[0] == str(config.file)
            if not all and not is_current:
                continue

            forwardings = ", ".join(
                f"{status.local_ports.get(k, '?')}:{v.host}:{v.port}" for k, v in spec.forwardings.items()
            )
            table.add_row(
                f"{status.id}*" if is_current and all else status.id,
                spec.locator.split(":")[1],
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
        locator=f"{config.file}:{profile_name}",
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
    locator = f"{config.file}:{profile_name}"

    with TunnelManager() as manager:
        manager.close_tunnel(locator)
