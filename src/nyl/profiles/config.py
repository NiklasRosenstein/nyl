from __future__ import annotations
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal


@dataclass
class Profile:
    """
    Represents a Kubernetes connection profile.
    """

    kubeconfig: LocalKubeconfig | KubeconfigFromSsh = field(default_factory=lambda: LocalKubeconfig())
    """
    Describe how the Kubeconfig is to be obtained.
    """

    tunnel: SshTunnel | None = None
    """
    Describe how to create an SSH tunnel to reach the Kubernetes cluster API.
    """


@dataclass(kw_only=True, frozen=True)
class LocalKubeconfig:
    """
    Use the local Kubeconfig file, either from the default location or a custom path specified in the environment.
    """

    type: Literal["local"] = "local"
    path: str | None = None
    context: str | None = None


@dataclass(kw_only=True, frozen=True)
class KubeconfigFromSsh:
    """
    Represents how to obtain the Kubeconfig from an SSH connection.
    """

    type: Literal["ssh"] = "ssh"
    user: str
    host: str
    path: str
    identity_file: str | None = None
    context: str | None = None


@dataclass(kw_only=True)
class SshTunnel:
    """
    Configuration for an SSH tunnel.
    """

    type: Literal["ssh"] = "ssh"
    user: str
    host: str
    identity_file: str | None = None


@dataclass
class ProfileConfig:
    FILENAME = "nyl-profiles.yaml"
    FALLBACK_PATH = Path.home() / ".config" / "nyl" / FILENAME
    STATE_DIRNAME = ".nyl"

    file: Path
    profiles: dict[str, Profile]

    @staticmethod
    def find_config_file(cwd: Path | None = None) -> Path:
        """
        Find the `nyl-profiles.yaml` in the given *cwd* or any of its parent directories, and ultimately fall back to
        `~/.nyl/nyl-profiles.yaml` in the user's home directory.

        Raises:
            FileNotFoundError: If the configuration file could not be found.
        """

        if cwd is None:
            cwd = Path.cwd()

        for directory in [cwd] + list(cwd.parents) + [Path.home()]:
            config_file = directory / ProfileConfig.FILENAME
            if config_file.exists():
                return config_file.absolute()

        if ProfileConfig.FALLBACK_PATH.exists():
            return ProfileConfig.FALLBACK_PATH.absolute()

        raise FileNotFoundError(
            f"Configuration file '{ProfileConfig.FILENAME}' not found in '{Path.cwd()}', any of its parent directories "
            f"or '{ProfileConfig.FALLBACK_PATH.parent}'"
        )

    @staticmethod
    def load(file: Path) -> "ProfileConfig":
        """
        Load the profiles configuration from the given file.
        """

        from databind.json import load as deser
        from yaml import safe_load

        profiles = deser(safe_load(file.read_text()), dict[str, Profile], filename=str(file))
        return ProfileConfig(file, profiles)
