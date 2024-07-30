from pathlib import Path
from typing import Iterable
from nyl.secrets.config import SecretProvider


class NullSecretsProvider(SecretProvider):
    def init(self, config_file: Path) -> None:
        pass

    def keys(self) -> Iterable[str]:
        return []

    def get(self, secret_name: str) -> str:
        raise KeyError(f"No secrets provider configured; cannot retrieve secret '{secret_name}'.")
