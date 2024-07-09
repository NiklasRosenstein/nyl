from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, Iterable
from pathlib import Path
from databind.core import Union

SecretValue = dict[str, Any] | list[Any] | str | int | float | bool | None
"""
A secret is a JSON-serializable value that can be stored in a secret provider.
"""


@Union(style=Union.FLAT, discriminator_key="provider")
@dataclass
class SecretProvider(ABC):
    """
    A SecretProvider is a source of secrets that can be accessed by keys.
    """

    @abstractmethod
    def init(self, config_file: Path) -> None:
        """
        Called after loading the provider configuration from a configuration file. The file's path is provided to
        allow the provider to resolve relative paths.
        """

    @abstractmethod
    def keys(self) -> Iterable[str]:
        """
        Return an iterator over all keys in the provider.
        """

    @abstractmethod
    def get(self, key: str) -> SecretValue:
        """
        Retrieve a secret by key.

        Args:
            key: The key of the secret to retrieve.
        Returns:
            The secret value.
        Raises:
            KeyError: If the key does not exist.
        """


@dataclass
class SecretsConfig:
    FILENAME = "nyl-secrets.yaml"

    provider: SecretProvider

    @staticmethod
    def find_config_file(cwd: Path | None = None) -> Path:
        """
        Find the `nyl-secrets.yaml` in the given *cwd* or any of its parent directories.
        """

        if cwd is None:
            cwd = Path.cwd()

        for directory in [cwd] + list(cwd.parents):
            file = directory / SecretsConfig.FILENAME
            if file.exists():
                return file

        raise FileNotFoundError(
            f"Could not find '{SecretsConfig.FILENAME}' in '{Path.cwd()}' or any of its parent directories."
        )

    @staticmethod
    def load(file: Path) -> "SecretsConfig":
        """
        Load the secrets configuration from a file.
        """

        from databind.json import load as deser
        from yaml import safe_load

        return SecretsConfig(deser(safe_load(file.read_text()), SecretProvider, filename=str(file)))
