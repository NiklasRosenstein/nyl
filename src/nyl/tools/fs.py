from typing import Literal, overload
from pathlib import Path


@overload
def find_config_file(filename: str, cwd: Path | None = None, required: Literal[False] = False) -> Path | None: ...


@overload
def find_config_file(filename: str, cwd: Path | None = None, required: Literal[True] = True) -> Path: ...


def find_config_file(filename: str, cwd: Path | None = None, required: bool = True) -> Path | None:
    """
    Find a file with the given *filename* in the given *cwd* or any of its parent directories.
    """

    if cwd is None:
        cwd = Path.cwd()

    for directory in [cwd] + list(cwd.parents):
        file = directory / filename
        if file.exists():
            return file

    if required:
        raise FileNotFoundError(f"Could not find '{filename}' in '{Path.cwd()}' or any of its parent directories.")

    return None
