"""
Interact with the secrets providers configured in `nyl-secrets.yaml`.
"""

from nyl.utils import new_typer


app = new_typer(name="secrets", help=__doc__)


@app.command()
def list(alias: str) -> None:
    """
    List the keys for all secrets in the provider.
    """


@app.command()
def get(alias: str, key: str) -> None:
    """
    Get the value of a secret as JSON.
    """
