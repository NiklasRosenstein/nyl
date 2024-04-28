from functools import partial
from typing import Callable, Generic, TypeVar

T = TypeVar("T")


class Var(Generic[T]):
    """
    A Var is a lazy value that can be resolved to a concrete value.
    """

    def __init__(self, value: T | Callable[[], T]) -> None:
        if not callable(value):

            def _static(value: T) -> T:
                return value

            value = partial(_static, value)
        self._value = value

    def resolve(self) -> T:
        return self._value()
