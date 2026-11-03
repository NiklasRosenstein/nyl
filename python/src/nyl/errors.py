"""Errors contain diagnostics and identities, never rendered manifest bodies."""


class NylError(Exception):
    """Base class for the client's errors."""


class ExecutionError(NylError):
    """A CLI invocation failed; diagnostics may contain sensitive CLI messages."""

    def __init__(
        self,
        message: str,
        *,
        command: list[str],
        diagnostics: str = "",
        exit_code: int | None = None,
    ) -> None:
        self.command = tuple(command)
        self.diagnostics = diagnostics
        self.exit_code = exit_code
        super().__init__(f"{message}\n{diagnostics}".rstrip())


class ExecutableNotFoundError(ExecutionError):
    """The configured Nyl executable is unavailable."""


class RenderTimeoutError(ExecutionError):
    """Rendering exceeded the client's timeout."""


class RenderError(ExecutionError):
    """The CLI returned a nonzero exit code or could not start."""


class InvalidOutputError(NylError):
    """Rendered stdout is not a stream of Kubernetes resource dictionaries."""


class SelectionError(NylError):
    """A resource selector does not identify exactly one resource."""


class ResourceNotFoundError(SelectionError):
    """No resource matches a selector."""


class AmbiguousResourceError(SelectionError):
    """Several resources match a selector."""


class DeclarationError(NylError):
    """A container helper cannot interpret the authored declaration."""
