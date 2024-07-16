from dataclasses import dataclass
from typing import Any
from kubernetes.client import CustomObjectsApi
from kubernetes.client.api_client import ApiClient
from . import crd
from nyl.tools.kubectl import Kubectl


@dataclass
class Application:
    """
    Represents an application deployed in the system.
    """

    name: str
    contains_group_kinds: list[str]


class ApplicationManager:
    """
    Manage applications deployed in the system.
    """

    def __init__(self, client: ApiClient, kubectl: Kubectl) -> None:
        """
        Args:
            client: Kubernetes API client.
            kubectl: A corresponding `kubectl` client to interact with the cluster. We re-use kubectl's applset
                implementation to create and manage applications in the system.
        """

        self._client = client
        self._kubectl = kubectl
        self._kubectl_version_cached: str | None = None

    @property
    def _kubectl_version(self) -> str:
        """
        Get the version of the kubectl client.
        """

        if self._kubectl_version_cached is None:
            self._kubectl_version_cached = self._kubectl.version()["gitVersion"]
        return self._kubectl_version_cached

    def _get_applyset_ref(self, name: str) -> str:
        """
        Get the reference to the ApplySet resource for the application.
        """

        return f"application.{crd.GROUP}/{name}"

    def list_applications(self) -> Application:
        """
        List applications deployed in the system.
        """

        api = CustomObjectsApi(self._client)
        objects = api.list_cluster_custom_object(  # noqa
            group=crd.GROUP,
            version=crd.VERSION,
            plural=crd.PLURAL,
        )

        # TODO
        return  # type: ignore[return-value]

    def get_application(self, name: str) -> Application:
        """
        Get an application deployed in the system.
        """

        api = CustomObjectsApi(self._client)
        obj = api.get_cluster_custom_object(group=crd.GROUP, version=crd.VERSION, plural=crd.PLURAL, name=name)
        print(obj)
        # TODO
        return  # type: ignore[return-value]

    def delete_application(self, name: str, cascade: bool = True) -> None:
        """
        Delete an application from the system.

        Args:
            name: The name of the application to delete.
            cascade: If True, delete all resources associated with the application.
        """

        if cascade:
            self._kubectl.apply(
                manifests=[],
                force_conflicts=True,
                server_side=True,
                applyset=self._get_applyset_ref(name),
            )

        api = CustomObjectsApi(self._client)
        api.delete_cluster_custom_object(group=crd.GROUP, version=crd.VERSION, plural=crd.PLURAL, name=name)

    # def upsert_application(self, app: Application) -> None:
    #     """
    #     Create or update an application in the system.
    #     """

    #     api = CustomObjectsApi(self._client)
    #     api.create_cluster_custom_object(
    #         group=crd.GROUP,
    #         version=crd.VERSION,
    #         plural=crd.PLURAL,
    #         body=crd.generate_application_resource(
    #             app.name,
    #             contains_resources=app.contains_group_kinds,
    #             kubectl_version=self._kubectl_version,
    #         ),
    #     )

    def update_application(self, name: str, manifests: list[dict[str, Any]]) -> None:
        """
        Update an application in the system or create it.

        Args:
            name: The name of the application to update.
            manifests: The updated manifests of the application.
        """

        resource_kinds = {crd.get_canonical_resource_kind_name(m["apiVersion"], m["kind"]) for m in manifests}

        # Ensure that the application object exists.
        # TODO Check if it exists instead of always calling the create endpoint?
        # TODO Do we maybe need to update the resource kinds only after the apply?
        #       Somehow kubectl must understand the previous resource kinds that were involved in the
        #       application prior to the update.

        api = CustomObjectsApi(self._client)
        api.create_cluster_custom_object(
            group=crd.GROUP,
            version=crd.VERSION,
            plural=crd.PLURAL,
            body=crd.generate_application_resource(
                name,
                contains_resources=resource_kinds,
                kubectl_version=self._kubectl_version,
            ),
        )

        self._kubectl.apply(
            manifests=manifests,
            force_conflicts=True,
            server_side=True,
            applyset=self._get_applyset_ref(name),
        )
