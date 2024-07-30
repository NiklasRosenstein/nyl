from unittest.mock import MagicMock
from pathlib import Path
from nyl.generator import Generator
from nyl.generator.dispatch import DispatchingGenerator
from nyl.resources import REGISTRY


def test__DispatchingGenerator__default__creates_generator_for_every_nyl_resource_kind() -> None:
    generator = DispatchingGenerator.default(
        git_repo_cache_dir=Path("/tmp"), search_path=[], working_dir=Path("/tmp"), client=MagicMock()
    )
    for kind in REGISTRY:
        assert kind in generator.generators.keys()
        assert isinstance(generator.generators[kind], Generator)
