from nyl.resources import ObjectMetadata
from nyl.resources.applyset import ApplySet, calculate_applyset_id


def test__ApplySet__dump() -> None:
    resource = ApplySet(
        metadata=ObjectMetadata(
            name="test-applyset",
            namespace=None,
        )
    )
    resource.tooling = "kubectl/1.30"
    resource.contains_group_kinds = ["Service", "Deployment.apps"]
    resource.validate()

    assert resource.dump() == {
        "apiVersion": "nyl.io/v1",
        "kind": "ApplySet",
        "metadata": {
            "name": "test-applyset",
            "annotations": {
                "applyset.kubernetes.io/tooling": "kubectl/1.30",
                "applyset.kubernetes.io/contains-group-kinds": "Deployment.apps,Service",  # sorted
            },
            "labels": {
                "applyset.kubernetes.io/id": calculate_applyset_id(
                    name="test-applyset",
                    namespace="",
                    group="nyl.io",
                ),
            },
        },
    }
