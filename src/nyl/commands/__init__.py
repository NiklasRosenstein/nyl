"""
Nyl is a flexible configuration management tool for Kubernetes resources that can be used to generate and deploy
applications directly or integrate as an ArgoCD ConfigManagementPlugin.
"""

from enum import Enum
import sys
from loguru import logger
from typer import Option
from nyl.tools.typer import new_typer


app = new_typer(help=__doc__)


from . import crds  # noqa: F401,E402
from . import new  # noqa: E402
from . import profile  # noqa: E402
from . import secrets  # noqa: E402
from . import template  # noqa: F401,E402
from . import tun  # noqa: E402

app.add_typer(new.app)
app.add_typer(profile.app)
app.add_typer(secrets.app)
app.add_typer(tun.app)


class LogLevel(str, Enum):
    TRACE = "trace"
    DEBUG = "debug"
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"


@app.callback()
def _callback(
    log_level: LogLevel = Option(LogLevel.INFO, "--log-level", "-l", help="The log level to use."),
) -> None:
    logger.remove()
    logger.add(sys.stderr, level=log_level.name)
