"""
Interact with the secrets providers configured in `nyl-secrets.yaml`.
"""

import json
from nyl.secrets.config import SecretsConfig
from nyl.tools.typer import new_typer


app = new_typer(name="secrets", help=__doc__)


@app.command()
def list() -> None:
    """
    List the keys for all secrets in the provider.
    """

    secrets = SecretsConfig.load()
    for key in secrets.provider.keys():
        print(key)


@app.command()
def get(key: str, pretty: bool = False) -> None:
    """
    Get the value of a secret as JSON.
    """

    secrets = SecretsConfig.load()
    print(json.dumps(secrets.provider.get(key), indent=4 if pretty else None))
