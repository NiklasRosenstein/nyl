"""The subprocess boundary delegates rendering policy and caching to Rust."""

import math
import os
import subprocess
from pathlib import Path

from .errors import ExecutableNotFoundError, RenderError, RenderTimeoutError
from .manifests import ManifestSet


class NylClient:
    """Invoke an installed Nyl executable from a fixed project directory."""

    def __init__(
        self,
        project: str | os.PathLike[str],
        executable: str | os.PathLike[str] = "nyl",
        timeout: float = 120,
    ) -> None:
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be positive and finite")
        self.project = Path(project).resolve()
        self.executable = os.fspath(executable)
        self.timeout = timeout

    def render(
        self,
        source: str | os.PathLike[str],
        *,
        target: str | None = None,
        offline: bool = False,
        allow_secret_inputs: bool = False,
    ) -> ManifestSet:
        """Render one source file, preserving the project's validation defaults.

        Relative source paths are resolved by the CLI from ``project``. Each call
        executes the CLI afresh; its own cache decides which artifacts to reuse.
        """
        command = [self.executable, "render"]
        if target is not None:
            command.extend(["--target", target])
        if offline:
            command.append("--offline")
        if allow_secret_inputs:
            command.append("--allow-secret-inputs")
        command.extend(["--", os.fspath(source)])
        try:
            result = subprocess.run(
                command,
                shell=False,
                cwd=self.project,
                capture_output=True,
                timeout=self.timeout,
                check=False,
            )
        except FileNotFoundError:
            if not self.project.is_dir():
                raise RenderError(
                    f"Project directory does not exist: {self.project}", command=command
                ) from None
            raise ExecutableNotFoundError(
                f"Nyl executable not found: {self.executable!r}; install the Nyl CLI separately",
                command=command,
            ) from None
        except subprocess.TimeoutExpired as error:
            raise RenderTimeoutError(
                f"Nyl rendering exceeded {self.timeout} seconds",
                command=command,
                diagnostics=(error.stderr or b"").decode("utf-8", errors="replace"),
            ) from None
        except OSError as error:
            raise RenderError(f"Could not execute Nyl: {error.strerror}", command=command) from None
        diagnostics = result.stderr.decode("utf-8", errors="replace")
        if result.returncode:
            raise RenderError(
                f"Nyl rendering failed with exit code {result.returncode}",
                command=command,
                diagnostics=diagnostics,
                exit_code=result.returncode,
            )
        return ManifestSet.from_yaml(result.stdout)
