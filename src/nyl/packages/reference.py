from collections.abc import Iterable
from dataclasses import dataclass
import re
from typing import Callable, TypeVar

from nyl.tools.kvstore import Value


Value_T = TypeVar("Value_T", bound=Value)


@dataclass
class Reference:
    """
    Represents a reference to a field provided by another resource. Usually this references Kubernetes `Secret`
    resources. References are rendered into the template as placeholders to be replaced by the actual values later,
    allowing Nyl to detect when resource depend on each other and to create them in the correct order.

    References do not support string manipulation or other operations. They are only used to reference values from
    other resources as-is.

    The structure of a reference is as follows:

        * `kind`
        * `[namespace/]name`
        * `key`
    """

    kind: str
    namespace: str | None
    name: str
    key: str

    def __str__(self) -> str:
        """
        Returns the placeholder string for this reference.
        """

        return f"((NYL_REFERENCE {self.kind} {self.namespace or ''}/{self.name} {self.key}))"

    @staticmethod
    def from_triplet(kind: str, namespace_and_name: str, key: str) -> "Reference":
        if "/" in namespace_and_name:
            namespace, name = namespace_and_name.split("/", 1)
        else:
            namespace, name = None, namespace_and_name
        return Reference(kind, namespace or None, name, key)

    @staticmethod
    def sub(value: Value_T, callback: Callable[["Reference"], str]) -> Value_T:
        """
        Recursively iterates over a value and its children, yielding references and replacing them with the result of the
        callback.

        :param value: The value to iterate over.
        :param callback: The callback to call when a reference is found. The callback should return the value to replace the
                        reference with.
        :return: A generator that yields references and the modified value.
        """

        match value:
            case dict():
                return {key: Reference.sub(item, callback) for key, item in value.items()}  # type: ignore[return-value]
            case list():
                return [Reference.sub(item, callback) for item in value]  # type: ignore[return-value]
            case str():

                def repl(match: re.Match[str]) -> str:
                    ref = Reference.from_triplet(match.group(1), match.group(2), match.group(3))
                    return callback(ref)

                return re.sub(r"\(\(NYL_REFERENCE (\S+) (\S+) (\S+)\)\)", repl, value)  # type: ignore[return-value]

        return value

    @staticmethod
    def collect(value: Value_T) -> Iterable["Reference"]:
        """
        Collect all references from a value.
        """

        match value:
            case dict():
                for item in value.values():
                    yield from Reference.collect(item)
            case list():
                for item in value:
                    yield from Reference.collect(item)
            case str():
                for match in re.finditer(r"\(\(NYL_REFERENCE (\S+) (\S+) (\S+)\)\)", value):
                    yield Reference.from_triplet(match.group(1), match.group(2), match.group(3))
