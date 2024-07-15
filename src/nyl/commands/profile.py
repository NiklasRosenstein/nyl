"""
Interact with your Nyl profile configuration.
"""

from nyl.profiles import ProfileManager
from nyl.utils import new_typer


app = new_typer(name="profile", help=__doc__)


@app.command()
def activate(profile_name: str) -> None:
    with ProfileManager.load() as manager:
        manager.activate_profile(profile_name)
