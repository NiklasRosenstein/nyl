from dataclasses import dataclass, field
from typing import Any

from nyl.resources import NylResource


@dataclass
class ChartRef:
    """
    Represents a reference to a Helm chart.
    """

    path: str | None = None
    """ Path to the chart in the Git repository; or relative to the file that defines the resource. """

    git: str | None = None
    """ URL to a Git repository containing the chart. May include a query string to specify a `ref` or `rev`. """

    repository: str | None = None
    """ A Helm repository, if the chart is not local. Must either use the `https://` or `oci://` scheme. """

    name: str | None = None
    """ The name of the chart. This is only needed when `repository` is set. """

    version: str | None = None
    """ The version of the chart. This is only needed when `repository` is set. """


@dataclass
class ReleaseMetadata:
    """
    Metadata for a Helm release.
    """

    name: str | None = None
    """ The name of the release. If not set, the name of the Helm chart resource is used. """

    namespace: str | None = None
    """ The namespace where the release should be installed. """


@dataclass(kw_only=True)
class HelmChart(NylResource):
    """
    Represents a Helm chart.
    """

    name: str
    """
    The name of the Helm chart resource. This is used to identify the chart in resources defined in a package. It
    is also used as the release name when installing the chart, unless the `release.name` field is set.
    """

    chart: ChartRef
    """ Reference to the Helm chart. """

    release: ReleaseMetadata = field(default_factory=ReleaseMetadata)
    """ Metadata for the release. """

    values: dict[str, Any] = field(default_factory=dict)
    """ Values for the Helm chart. """
