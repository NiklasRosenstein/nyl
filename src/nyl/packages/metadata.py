from dataclasses import dataclass


@dataclass
class PackageMetadata:
    allow_cross_namespace: bool = False
    """ Whether the definition of resources in more than one namespace is permitted. """
