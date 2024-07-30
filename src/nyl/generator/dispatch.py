from dataclasses import dataclass, field
from pathlib import Path
from nyl.generator import Generator
from nyl.resources import NylResource
from nyl.tools.types import Manifest, Manifests
from kubernetes.client.api_client import ApiClient


@dataclass
class DispatchingGenerator(Generator[Manifest], resource_type=Manifest):
    """
    Dispatches to the appropriate generator based on the resource type.

    Any resources can be passed to this generator, but only resources that have a corresponding generator will be
    processed. Any other resources will be returned as-is.
    """

    generators: dict[str, Generator] = field(default_factory=dict)
    """ Collection of generators to dispatch to based on the resource kind. """

    @staticmethod
    def default(
        *, git_repo_cache_dir: Path, search_path: list[Path], working_dir: Path, client: ApiClient
    ) -> "DispatchingGenerator":
        """
        Create a new DispatchingGenerator with the default set of generators.
        """

        from nyl.generator.helmchart import HelmChartGenerator
        from nyl.generator.statefulsecret import StatefulSecretGenerator

        return DispatchingGenerator(
            generators={
                "HelmChart": HelmChartGenerator(
                    git_repo_cache_dir=git_repo_cache_dir, search_path=search_path, working_dir=working_dir
                ),
                "StatefulSecret": StatefulSecretGenerator(client),
            }
        )

    # Generator implementation

    def generate(self, /, res: Manifest) -> Manifests:
        if (nyl_resource := NylResource.maybe_load(res)) is None:
            return Manifests([res])

        if nyl_resource.KIND not in self.generators:
            raise ValueError(f"No generator found for resource kind: {nyl_resource.KIND}")

        generator = self.generators[nyl_resource.KIND]
        return generator.generate(nyl_resource)
