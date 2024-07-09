import os
import signal
import subprocess
from typing import Any
from loguru import logger


class Tunnel:
    """
    Helper to create SSH tunnels.
    """

    def __init__(self, forwarding: str, host: str) -> None:
        """
        Args:
            forwarding: A port forwarding, such as `6443:127.0.0.1:6443`.
            host: The host to connect to.
        """

        self.forwarding = forwarding
        self.host = host
        self._process: subprocess.Popen | None = None

    def __del__(self) -> None:
        if self._process is not None:
            logger.warning("Tunnel object was not cleaned up properly")
            self.stop()

    def __enter__(self) -> "Tunnel":
        self.start()
        return self

    def __exit__(self, exc_type: Exception, exc_value: Exception, traceback: Any) -> None:
        self.stop()

    def start(self) -> None:
        """
        Start the tunnel.
        """

        if self._process is not None and self._process.poll() is None:
            raise RuntimeError("Tunnel already started")

        # note: we use os.setsid to assign the tunnel process to a new process group, so that we can kill it and
        #       its children safely with os.killpg.

        command = ["ssh", "-NL", self.forwarding, self.host]
        logger.info("Creating tunnel with command $ {command}", command=" ".join(command))
        self._process = subprocess.Popen(command, stdin=subprocess.DEVNULL, preexec_fn=os.setsid)

    def stop(self) -> None:
        """
        Stop the tunnel.
        """

        if self._process is None:
            raise RuntimeError("Tunnel not started")

        os.killpg(os.getpgid(self._process.pid), signal.SIGTERM)
        self._process.wait()
        self._process = None

    def is_running(self) -> bool:
        """
        Check if the tunnel is running.
        """

        return self._process is not None and self._process.poll() is None
