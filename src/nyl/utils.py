from pathlib import Path
from typing import TypeVar
import yaml
import databind.json

T = TypeVar("T")


def deser_yaml(cls: type[T], file: Path) -> T:
    data = yaml.safe_load(file.read_text())
    return databind.json.load(data, cls, filename=str(file))
