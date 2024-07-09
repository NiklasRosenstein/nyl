"""
Manage cluster connections configured in `nyl-profiles.yaml`.
"""

from typing import Optional

from nyl.profiles.config import ProfileConfig
from nyl.profiles.connections import ConnectionManager
from nyl.utils import new_typer


app = new_typer(name="conn", help=__doc__)


@app.command()
def list() -> None:
    """
    List all active connections.
    """

    manager = ConnectionManager()
    with manager.locked():
        for conn in manager.get_connection_statuses():
            print(conn)


@app.command()
def open(profile: str) -> None:
    """
    Open a connection to the cluster targeted by the profile.
    """

    config = ProfileConfig.load(ProfileConfig.find_config_file())
    manager = ConnectionManager()
    with manager.locked():
        manager.open_connection(config_file=config.file, alias=profile, config=config.profiles[profile].tunnel)


@app.command()
def close(profile: Optional[str] = None) -> None:
    """
    Close all connections or the connection for a specific profile.
    """

    manager = ConnectionManager()

    if profile is None:
        with manager.locked():
            for conn in manager.get_connection_statuses():
                manager.close_connection(conn.id)
        return

    # TODO
    raise NotImplementedError("Closing a specific connection is not yet implemented.")
