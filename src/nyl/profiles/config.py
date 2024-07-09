from __future__ import annotations
from dataclasses import dataclass, field
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
    Represents configuration for an SSH tunnel to a host from which the Kubernetes cluster can be accessed.
    """

    type: Literal["ssh"] = "ssh"
    user: str
    host: str
    identity_file: str | None = None
