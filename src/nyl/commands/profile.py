"""
Interact with your Nyl profile configuration.
"""

import shlex

from typer import Argument
from nyl.profiles import ProfileManager
from nyl.tools.typer import new_typer
from nyl.tools.cli import print_info


app = new_typer(name="profile", help=__doc__)


@app.command()
def activate(profile_name: str = Argument("default", envvar="NYL_PROFILE")) -> None:
    """
    Activate a Nyl profile.

    Evaluate the stdout of this command to export the KUBECONFIG into your environment.
    """

    with ProfileManager.load() as manager:
        # TODO: Add more information, e.g. if a tunnel is being used and was started/restart/reused.
        print_info(f"Activating profile '{profile_name}' from '{manager.config.file}'.")
        profile = manager.activate_profile(profile_name)
        print_info(f"Profile '{profile_name}' activated.")

    for key, value in profile.env.items():
        print(f"export {key}={shlex.quote(value)}")
