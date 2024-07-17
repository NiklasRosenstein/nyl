from pathlib import Path
from typing import Any
import jinja2
import yaml

from .reference import Reference


class PackageTemplater:
    """
    Helper class to evaluate templates in a package.
    """

    def __init__(self, parameters: dict[str, Any]) -> None:
        self._env = jinja2.Environment(undefined=jinja2.StrictUndefined)
        self._env.globals["Params"] = parameters
        self._globals = Globals()

        for key in dir(self._globals):
            if not key.startswith("_"):
                self._env.globals[key] = getattr(self._globals, key)

    def render(self, template: str) -> str:
        """
        Renders the given template.
        """

        return self._env.from_string(template).render()

    def render_directory(self, directory: Path) -> list[dict[str, Any]]:
        """
        Renders all templates in the given directory.
        """

        manifests: list[dict[str, Any]] = []
        for item in directory.iterdir():
            if item.name.startswith("nyl-") or item.suffix != ".yaml" or not item.is_file():
                continue
            manifests.extend(yaml.safe_load_all(self.render(item.read_text())))
        return manifests


class Globals:
    @staticmethod
    def ref(kind: str, namespace_and_name: str, key: str) -> Reference:
        return Reference.from_triplet(kind, namespace_and_name, key)

    @staticmethod
    def randhex(length: int) -> str:
        from secrets import token_hex

        return token_hex(length)
