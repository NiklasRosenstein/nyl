import contextlib
from dataclasses import dataclass
import os
import random
import signal
import subprocess
from typing import Any, Iterable, Iterator, Literal
from pathlib import Path
from filelock import FileLock
from loguru import logger

from nyl.profiles.config import SshTunnel


@dataclass
class ConnectionStatus:
    id: str
    config_file: Path
    profile_alias: str
    status: Literal["open", "broken"]
    ssh_pid: int | None
    port: int | None


class ConnectionManager:
    """
    Helper class to manage connections to Kubernetes clusters as defined in the profiles configuration.

    The connection manager tracks profile connection data such as the Kubeconfig file and SSH tunnels globally,
    i.e. possibly across multiple `nyl-profiles.yaml` configuration files.
    """

    DEFAULT_STATE_DIR = Path.home() / ".nyl" / "connections"

    def __init__(self, state_dir: Path | None = None) -> None:
        """
        Args:
            state_dir: Path to the directory where the connection manager stores its global state.
        """

        self.state_dir = state_dir or self.DEFAULT_STATE_DIR
        self._state_file = self.state_dir / "state.json"
        self._is_locked = False
        self._state: _State | None = None

    @contextlib.contextmanager
    def locked(self) -> Iterator[None]:
        """
        Acquire a lock on the state file. Only one connection manager may perform modifications to the global
        connection state at a time. The internally cached state returned by `_load_state` is only valid within the
        context of this lock, and will be unloaded when the lock is released.
        """

        with FileLock(self.state_dir / ".lock", timeout=5):
            self._is_locked = True
            try:
                yield
            finally:
                self._save_state()
                self._is_locked = False
                self._state = None

    def _load_state(self) -> "_State":
        """
        Load the global connection state from disk.
        """

        assert self._is_locked
        if self._state is None:
            if self._state_file.is_file():
                self._state = _State.load(self.state_dir / "state.json")
            else:
                self._state = _State([])
        return self._state

    def _save_state(self) -> None:
        """
        Save the global connection state to disk.
        """

        assert self._is_locked
        if self._state is not None:
            self._state.save(self._state_file)

    def get_connection_statuses(self) -> Iterable[ConnectionStatus]:
        """
        Get the status of all connections
        """

        if not self._is_locked:
            raise RuntimeError("ConnectionManager.locked() context must be active")

        connections = self._load_state().connections
        for connection in connections:
            # TODO: Check if tunnel PID is still running
            yield ConnectionStatus(
                id=connection.id,
                config_file=connection.config_file,
                profile_alias=connection.alias,
                status="open" if connection.tunnel_pid is not None else "broken",
                ssh_pid=connection.tunnel_pid,
                port=connection.tunnel_port,
            )

    def open_connection(self, config_file: Path, alias: str, config: SshTunnel) -> None:
        """
        Open an SSH tunnel from the given config file and profile alias. If a connection is already open, it is
        closed first.
        """

        if not self._is_locked:
            raise RuntimeError("ConnectionManager.locked() context must be active")

        state = self._load_state()
        if (conn := state.get_connection(config_file, alias)) is None:
            conn = _Connection(new_connection_id(), config_file, alias, None)
            state.connections.append(conn)

        conn.kill_tunnel()
        conn.start_tunnel(config)

    def close_connection(self, id: str) -> bool:
        """
        Close the connection for the given profile alias.

        Returns:
            `True` if a connection was closed, `False` if no connection was found.
        """

        if not self._is_locked:
            raise RuntimeError("ConnectionManager.locked() context must be active")

        state = self._load_state()
        if conn := state.get_connection_by_id(id):
            conn.kill_tunnel()
            state.connections.remove(conn)
            return True

        return False


def new_connection_id() -> str:
    """
    Generate a new unique connection ID.
    """

    return f"conn-{random.randint(1000, 9999)}"


@dataclass
class _Connection:
    """
    Represents connection data for a single profile.
    """

    # TODO: Assign unique IDs to connections/random names to be able to identify them later in the CLI?

    id: str
    """
    A unique ID for the connection.
    """

    config_file: Path
    """
    Path to the `nyl-profiles.yaml` file where the profile is defined.
    """

    alias: str
    """
    The alias of the profile.
    """

    raw_kubeconfig: str | None = None
    """
    The raw Kubeconfig file content as it was retrieved per the profile configuration. Before this file is written
    to disk for use with `kubectl`, it may be modified by the connection manager to connect through an SSH tunnel.
    """

    tunnel_state: dict[str, Any] | None = None
    """
    Parameters with which the SSH tunnel was created. This is used to understand when the tunnel needs to be
    recreated.
    """

    tunnel_pid: int | None = None
    """
    The PID of the SSH tunnel process, if it is currently running.
    """

    tunnel_port: int | None = None
    """
    The local port on which the SSH tunnel is listening.
    """

    def kill_tunnel(self) -> None:
        if self.tunnel_pid is None:
            return
        logger.debug("Closing SSH tunnel for profile '{}' with PID {}.", self.alias, self.tunnel_pid)
        os.kill(self.tunnel_pid, signal.SIGTERM)
        self.tunnel_pid = None
        self.tunnel_port = None
        self.tunnel_state = None

    def start_tunnel(self, tunnel: SshTunnel) -> None:
        # TODO: Better method to find an available port.
        self.tunnel_port = random.randint(10000, 20000)

        # TODO: Get as a parameter the target host:port to forward to.
        command = ["ssh", "-N", "-L", f"{self.tunnel_port}:localhost:6443", f"{tunnel.user}@{tunnel.host}"]
        if tunnel.identity_file is not None:
            command.extend(["-i", tunnel.identity_file])

        logger.debug("Opening SSH tunnel for profile '{}' with command: {}.", self.alias, command)
        process = subprocess.Popen(command)
        self.tunnel_pid = process.pid


@dataclass
class _State:
    """
    Represents the global connection state.
    """

    connections: list[_Connection]

    def get_connection(self, config_file: Path, alias: str) -> _Connection | None:
        """
        Get the connection for the given profile alias. If no connection exists, a new one is created.
        """

        for conn in self.connections:
            if conn.config_file == config_file and conn.alias == alias:
                return conn

        return None

    def get_connection_by_id(self, id: str) -> _Connection | None:
        """
        Get the connection for the given profile alias. If no connection exists, a new one is created.
        """

        for conn in self.connections:
            if conn.id == id:
                return conn

        return None

    @staticmethod
    def load(file: Path) -> "_State":
        """
        Load the global connection state from a file.
        """

        from databind.json import load as deser
        from json import loads

        return deser(loads(file.read_text()), _State, filename=str(file))

    def save(self, file: Path) -> None:
        """
        Save the global connection state to a file.
        """

        from databind.json import dump as ser
        from json import dumps

        file.write_text(dumps(ser(self, _State, filename=str(file)), indent=2))
