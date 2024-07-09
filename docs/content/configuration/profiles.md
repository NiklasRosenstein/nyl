# Profiles

Profiles are a way to define targets for deploying your application directly with Nyl. They are not needed when
deploying with ArgoCD, as the target cluster is defined in the ArgoCD application.

Profiles are defined in a `nyl-profiles.yaml` file that is located in the current working directory or any of its parent directories. As a last resort, the file will be searched in `~/.config/nyl/nyl-profiles.yaml`.

The configuration describes

1. How to obtain the kubeconfig for the target cluster.
2. How to connect to the target cluster (e.g. via a tunnel).

This is particularly useful for clusters that are not directly accessible from the machine running Nyl, such as on-premises clusters or clusters behind a firewall.

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
