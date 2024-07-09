from pathlib import Path
from typing import Any, TypeVar
import yaml
import databind.json
from typer import Typer

T = TypeVar("T")


def deser_yaml(cls: type[T], file: Path) -> T:
    data = yaml.safe_load(file.read_text())
    return databind.json.load(data, cls, filename=str(file))


def new_typer(**kwargs: Any) -> Typer:
    return Typer(
        no_args_is_help=True,
        pretty_exceptions_enable=False,
        **kwargs,
    )
