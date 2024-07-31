import importlib
import pkgutil
from unittest.mock import MagicMock
from pathlib import Path

from loguru import logger
from nyl.generator import Generator
from nyl.generator.dispatch import DispatchingGenerator
from nyl.resources import API_VERSION_INLINE, NylResource


def test__DispatchingGenerator__default__creates_generator_for_every_nyl_inline_resource_kind() -> None:
    """
    This test ensures that the default DispatchingGenerator is created with a Generator for every Nyl inline resource
    kind to ensure that all resources can be generated.
    """

    # List the members of the nyl.resources module to look for Nyl inline resource kinds.
    resource_kinds: set[str] = set()
    for info in pkgutil.iter_modules(importlib.import_module("nyl.resources").__path__, "nyl.resources."):
        module = importlib.import_module(info.name)
        for value in vars(module).values():
            if (
                isinstance(value, type)
                and issubclass(value, NylResource)
                and value != NylResource
                and value.__module__ == info.name
                and value.API_VERSION == API_VERSION_INLINE
            ):
                resource_kinds.add(value.KIND)

    logger.info("Detected Nyl inline resource kinds: {}", resource_kinds)

    generator = DispatchingGenerator.default(
        git_repo_cache_dir=Path("/tmp"), search_path=[], working_dir=Path("/tmp"), client=MagicMock()
    )

    for kind in resource_kinds:
        assert kind in generator.generators.keys()
        assert isinstance(generator.generators[kind], Generator)
