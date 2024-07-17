from dataclasses import dataclass, field
from typing import Any

from nyl.resources import NylResource, ObjectMetadata


@dataclass(kw_only=True)
class Package(NylResource):
    """
    Instantiate another Nyl package.
    """

    metadata: ObjectMetadata

    package: str

    parameters: dict[str, Any] = field(default_factory=dict)
