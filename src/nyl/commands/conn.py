"""
Manage cluster connections configured in `nyl-profiles.yaml`.
"""

from typing import Optional

from loguru import logger
from typer import Argument

from nyl.profiles.config import ProfileConfig
from nyl.profiles.tunnelmanager import TunnelManager, TunnelSpec
from nyl.utils import new_typer


app = new_typer(name="conn", help=__doc__)


@app.command()
def list() -> None:
    """
    List all active connections.
    """

    with TunnelManager() as manager:
        for conn in manager.get_tunnels():
            print(conn)


@app.command()
def open(profile_name: str) -> None:
    """
    Open a connection to the cluster targeted by the profile.
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
    Close all connections or the connection for a specific profile.
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
