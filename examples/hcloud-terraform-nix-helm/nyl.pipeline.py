from nyl.pipeline import Pipeline
from nyl.contrib.nixos import NixosAnywhereStep
from nyl.contrib.terraform import ApplyTerraformStep
from nyl.contrib.k8s import DeployHelmChartStep


def pipeline(location: str = "nbg1", node_count: int = 3) -> Pipeline:
    p = Pipeline()

    provision = ApplyTerraformStep(
        name="provision",
        description=f"Provision {node_count} VM(s) for the k3s cluster.",
        source=f"{p.dir}/provision",
        vars={
            "location": location,
            "node_count": node_count,
        },
    )
    p.add_step(provision)

    # TODO: Inject SSH key for Nixos-Anywhere to be able to connect.
    # TODO: Support a jump-host for Nixos-Anywhere to connect to the VMs.

    install_k3s_bootstrap = NixosAnywhereStep(
        name="install-k3s-bootstrap",
        description="Bootstrap the first node with k3s.",
        source=f"{p.dir}/install_k3s",
        hosts=provision.outputs["ip_addresses"][0],
    )
    p.add_step(install_k3s_bootstrap)

    install_k3s = NixosAnywhereStep(
        name="install-k3s",
        description="Install k3s on the remaining nodes to form a cluster.",
        source=f"{p.dir}/install_k3s",
        hosts=provision.outputs["ip_addresses"][1:],
    )
    p.add_step(install_k3s)

    # TODO: Get Kubeconfig from the first node and pass it to deploy step.

    deploy = DeployHelmChartStep(
        name="deploy",
        description="Deploy a Helm chart to the k3s cluster.",
    )
    p.add_step(deploy)

    return p
