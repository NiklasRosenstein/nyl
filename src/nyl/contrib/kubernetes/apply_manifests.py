
from dataclasses import dataclass
import logging
import os
from pathlib import Path
import subprocess
from .manifest_source import ManifestSource

logger = logging.getLogger(__name__)

@dataclass
class ApplyManifests:
    """
    This class implements the application of Kubernetes manifests to a Kubernetes cluster. The manifests must be
    provided by a `ManifestSource` instance.
    """

    manifests: ManifestSource
    """
    Source for the Kubernetes manifests to apply.
    """

    kube_config: Path | None = None
    """
    Path to the Kubernetes configuration file. If not provided, the default config will be used.
    """

    kube_context: str | None = None
    """
    The Kubernetes context to use. If not provided, the current context will be used.
    """

    _generated_manifests: str | None = None
    """
    Internal. A cache for the generated manifests.
    """

    def _prepare(self) -> None:
        """
        Prepare the manifests for applying.
        """

        if self._generated_manifests is None:
            self._generated_manifests = self.manifests.generate()

    def _kubectl(self, command: list[str], input: str | None = None) -> None:
        """
        Run a `kubectl` command with the given arguments.
        """

        command = [
            "kubectl",
            *command,
        ]

        env = os.environ.copy()
        if self.kube_config is not None:
            env["KUBECONFIG"] = str(self.kube_config)
        if self.kube_context is not None:
            command.extend(["--context", self.kube_context])

        result = subprocess.run(command, input=input, text=True, env=env, capture_output=True)
        if result.returncode == 0:
            return

        if "Error from server (NotFound): namespaces" in result.stderr:
            raise NotImplementedError("everything is new")

    def diff(self) -> None:
        """
        Show the diff of the manifests that will be applied.
        """

        self._prepare()
        assert self._generated_manifests is not None

        # TODO: How does diff handle removed resources?

        self._kubectl(["diff", "-f", "-", "--prune"], input=self._generated_manifests)

    def apply(self) -> None:
        raise NotImplementedError("Not implemented yet")
