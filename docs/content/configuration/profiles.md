# Profiles

Profiles are a way to define targets for deploying your application directly with Nyl. They are not needed when
deploying with ArgoCD, as the target cluster is defined in the ArgoCD application.

Profiles are defined in a `nyl-profiles.yaml` file that is located in the current working directory or any of its parent directories. As a last resort, the file will be searched in `~/.config/nyl/nyl-profiles.yaml`.

The configuration describes

1. How to obtain the kubeconfig for the target cluster.
2. How to connect to the target cluster (e.g. via a tunnel).

This is particularly useful for clusters that are not directly accessible from the machine running Nyl, such as
on-premises clusters or clusters behind a firewall.

## Example profile

```yaml title="nyl-profile.yaml"
default:
  kubeconfig:
    type: ssh
    user: root
    host: mycluster.example.com
    path: /etc/rancher/k3s/k3s.yaml
  tunnel:
    type: ssh
    user: root
    host: mycluster.example.com
```

!!! todo
    Include specification of configuration data model.

## Connection management

The Nyl CLI will automatically manage the connection to the target cluster by obtaining the kubeconfig and setting up
the tunnel. The tunnel will typically remain open unless it is explicitly closed by the user to reduce the overhead of
setting up the connection for each invocation of Nyl.

Connections can be managed manually using the `nyl conn` command. Connection state is stored globally in
`~/.config/nyl/.state/nyl-connections.json`. Note that while you may have multiple `nyl-profiles.yaml` files on your
system, the connection state is stored globally, and such is the interaction with `nyl conn`.

```
nyl conn list                 List all active connections.
nyl conn open <profile>       Open a connection to the cluster targeted by the profile.
nyl conn close [<profile>]    Close all connections or the connection for a specific profile.
```
