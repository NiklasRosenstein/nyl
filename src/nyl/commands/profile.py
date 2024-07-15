"""
Interact with your Nyl profile configuration.
"""

import shlex
from nyl.profiles import ProfileManager
from nyl.utils import new_typer


app = new_typer(name="profile", help=__doc__)


@app.command()
def activate(profile_name: str) -> None:
    """
    Activate the given profile.

    Evaluate the stdout of this command to export the KUBECONFIG into your environment.
    """

    with ProfileManager.load() as manager:
        profile = manager.activate_profile(profile_name)

    print(f"export KUBECONFIG={shlex.quote(str(profile.kubeconfig.absolute()))}")
