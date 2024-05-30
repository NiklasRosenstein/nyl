
import logging
logging.basicConfig(level=logging.DEBUG)

from nyl.contrib.kubernetes import HelmManifestSource, ApplyManifests

manifests = HelmManifestSource(
    release_name="postgresql",
    namespace="postgresql",
    chart_name="postgresql",
    repository="https://charts.bitnami.com/bitnami",
    # version="10.1.0",
    values={},
)

apply = ApplyManifests(manifests)

apply.diff()
