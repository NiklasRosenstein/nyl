from __future__ import annotations
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, overload


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
    """
    Path to the Kubernetes configuration file. Relative to the profile configuration file. If not specified, it falls
    back to the default location (per `KUBECONFIG` or otherwise `~/.kube/config`).
    """

    context: str | None = None
    """
    The context to use from the Kubeconfig file. If not specified, the current context is used.
    """


@dataclass(kw_only=True, frozen=True)
class KubeconfigFromSsh:
    """
    Represents how to obtain the Kubeconfig from an SSH connection.
    """

    type: Literal["ssh"] = "ssh"
    user: str
    """
    The username to connect to the remote host with.
    """

    host: str
    """
    The remote host to connect to.
    """

    identity_file: str | None = None
    """
    An SSH private key file to use for authentication.
    """

    path: str
    """
    The path where the Kubeconfig can be retrieved from.
    """

    context: str | None = None
    """
    The context to use from the Kubeconfig file. If not specified, the current context is used.
    """


@dataclass(kw_only=True)
class SshTunnel:
    """
    Configuration for an SSH tunnel.
    """

    type: Literal["ssh"] = "ssh"

    user: str
    """
    The username to connect to the remote host with.
    """

    host: str
    """
    The host to tunnel through.
    """

    identity_file: str | None = None
    """
    An SSH private key file to use for authentication.
    """


@dataclass
class ProfileConfig:
    FILENAME = "nyl-profiles.yaml"
    FALLBACK_PATH = Path.home() / ".config" / "nyl" / FILENAME
    STATE_DIRNAME = ".nyl"

    file: Path | None
    profiles: dict[str, Profile]

    @overload
    @staticmethod
    def find_config_file(cwd: Path | None = None, not_found_ok: bool = Literal[False]) -> Path: ...

    @overload
    @staticmethod
    def find_config_file(cwd: Path | None = None, not_found_ok: bool = Literal[True]) -> Path | None: ...

    @staticmethod
    def find_config_file(cwd: Path | None = None, not_found_ok: bool = False) -> Path | None:
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

        if not_found_ok:
            return None

        raise FileNotFoundError(
            f"Configuration file '{ProfileConfig.FILENAME}' not found in '{Path.cwd()}', any of its parent directories "
            f"or '{ProfileConfig.FALLBACK_PATH.parent}'"
        )

    @staticmethod
    def load(file: Path | None) -> "ProfileConfig":
        """
        Load the profiles configuration from the given file.
        """

        from databind.json import load as deser
        from yaml import safe_load

        if file is None:
            return ProfileConfig(None, {})

        profiles = deser(safe_load(file.read_text()), dict[str, Profile], filename=str(file))
        return ProfileConfig(file, profiles)
