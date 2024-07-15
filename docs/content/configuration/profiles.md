# Profiles

Profiles are a way to define targets for deploying your application directly with Nyl. They are not needed when
deploying with ArgoCD, as the target cluster is defined in the ArgoCD application.

Profiles are defined in a `nyl-profiles.yaml` file that is located in the current working directory or any of its parent directories. As a last resort, the file will be searched in `~/.nyl/nyl-profiles.yaml`.

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

## Activating a profile

Profiles can be activated using the `nyl profile activate` command. It fetches the Kubeconfig and opens the SSH tunnel
(if any) and prints the `KUBECONFIG` environment variable that can be used to interact with the target cluster.

```sh
$ nyl profile activate
export KUBECONFIG=/project/path/.nyl/profiles/default/kubeconfig.local
export KUBE_CONFIG_PATH=/project/path/.nyl/profiles/default/kubeconfig.local
```

The profile name can be omitted, in which case it defaults to the value of the `NYL_PROFILE` environment variable
or the string `"default"`.

## Tunnel management

The Nyl CLI will automatically manage tunnels to the target cluster by proxying through an SSH jump host. 
The tunnel will typically remain open unless it is explicitly closed by the user to reduce the overhead of
setting up the tunnel for each invocation of Nyl.

Tunnels can be managed manually using the `nyl tun` command. Tunnel state is stored globally in
`~/.nyl/tunnels/state.json`. Note that while you may have multiple `nyl-profiles.yaml` files on your
system, the tunnel state is stored globally, and such is the interaction with `nyl tun`.

```
nyl tun status               List all known tunnels.
nyl tun start <profile>      Open a tunnel to the cluster targeted by the profile.
nyl tun stop [<profile>]     Close all tunnels or the tunnel for a specific profile.
```
