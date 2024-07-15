from dataclasses import dataclass


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

    @staticmethod
    def from_triplet(kind: str, namespace_and_name: str, key: str) -> "Reference":
        if "/" in namespace_and_name:
            namespace, name = namespace_and_name.split("/", 1)
        else:
            namespace, name = None, namespace_and_name
        return Reference(kind, namespace, name, key)

    def __str__(self) -> str:
        """
        Returns the placeholder string for this reference.
        """

        return f"((NYL_REFERENCE {self.kind} {self.namespace or ''}/{self.name} {self.key}))"
