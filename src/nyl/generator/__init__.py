"""
This package contains everything related to the generation of Kubernetes manifests via Nyl.
"""

from abc import ABC, abstractmethod
from typing import Any, ClassVar, Generic, TypeVar

from nyl.resources import NylResource
from nyl.tools.types import Manifests, Manifest

T = TypeVar("T")


class Generator(ABC, Generic[T]):
    """
    Base class for generating Kubernetes manifests from Nyl resources.
    """

    resource_type: ClassVar[type[Any]]

    def __init_subclass__(cls, resource_type: type[T], **kwargs):
        cls.resource_type = resource_type
        super().__init_subclass__(**kwargs)

    @abstractmethod
    def generate(self, /, resource: T) -> Manifests:
        """
        Evaluate a Nyl resource and return a list of the generated Kubernetes manifests.
        """

        raise NotImplementedError


def reconcile_generator(generator: Generator, manifests: Manifests) -> Manifests:
    """
    Recursively reconcile all Nyl resources in the manifests using the given generator.
    """

    queue = Manifests(manifests)
    result = Manifests([])
    loops = 0

    while queue:
        if loops > 1000:
            raise RuntimeError("Reconciliation loop limit exceeded (1000).")

        resource = queue.pop(0)
        if NylResource.is_nyl_resource(resource):
            queue.extend(generator.generate(resource))
        else:
            result.append(resource)
        loops += 1

    return result
