"""
Nyl is a flexible configuration management tool for Kubernetes resources that can be used to generate and deploy
applications directly or integrate as an ArgoCD ConfigManagementPlugin.
"""

from nyl.utils import new_typer


app = new_typer(help=__doc__)


from . import profile  # noqa: E402
from . import secrets  # noqa: E402
from . import tun  # noqa: E402

app.add_typer(profile.app)
app.add_typer(secrets.app)
app.add_typer(tun.app)
