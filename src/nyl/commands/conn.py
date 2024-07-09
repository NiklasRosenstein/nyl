"""
Manage cluster connections configured in `nyl-profiles.yaml`.
"""

from typing import Optional
from nyl.utils import new_typer


app = new_typer(name="conn", help=__doc__)


@app.command()
def list() -> None:
    """
    List all active connections.
    """


@app.command()
def open(profile: str) -> None:
    """
    Open a connection to the cluster targeted by the profile.
    """


@app.command()
def close(profile: Optional[str]) -> None:
    """
    Close all connections or the connection for a specific profile.
    """
