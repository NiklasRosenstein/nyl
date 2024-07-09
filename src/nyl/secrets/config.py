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


class SecretsConfig(list[SecretProvider]):
    pass
