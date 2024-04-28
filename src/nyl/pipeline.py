from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from pathlib import Path
import sys
from typing import Any, ClassVar, Generic, Literal, TypeVar

from nyl.var import Var

T = TypeVar("T")


class PipelineStepImpl(ABC, Generic[T]):
    """
    Base class for pipeline step implementations.
    """

    context_cls: ClassVar[type[T]]

    def __init_subclass__(cls, context_cls: type[T]) -> None:
        cls.context_cls = context_cls

    @abstractmethod
    def init(self, ctx: T) -> None:
        """
        Initialize the step.
        """

        raise NotImplementedError

    @abstractmethod
    def plan(self, ctx: T) -> None:
        """
        Plan the step.
        """

        raise NotImplementedError

    @abstractmethod
    def apply(self, ctx: T) -> None:
        """
        Apply the step.
        """

        raise NotImplementedError


@dataclass
class PipelineStep:
    name: str
    impl: PipelineStepImpl[Any]
    outputs: dict[str, Any] | None = field(init=False)

    def output(self, name: str) -> Var[Any]:
        return Var(lambda: self.outputs[name])


class Pipeline:
    def __init__(self, dir: Path | Literal["parent-frame", "cwd"] = "parent-frame") -> None:
        match dir:
            case "parent-frame":
                dir = Path(sys._getframe(1).f_code.co_filename)
            case "cwd":
                dir = Path.cwd()
            case str():
                raise ValueError(f"Invalid dir argument: {dir!r} (did you intended to pass a pathlib.Path?)")

        self.dir = dir
        self.steps: dict[str, PipelineStep] = {}

    def add(self, name: str, impl: PipelineStepImpl[Any]) -> PipelineStep:
        if name in self.steps:
            raise ValueError(f"Step {name!r} already exists")
        step = PipelineStep(name, impl)
        self.steps[name] = step
        return step
