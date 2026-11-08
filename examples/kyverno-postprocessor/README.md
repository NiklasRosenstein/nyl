# Kyverno Post-Processor Example

This example demonstrates how Nyl applies [Kyverno policies](https://niklasrosenstein.github.io/nyl/kubernetes-rendering/validation/kyverno-policies/)
at render time. Policies annotated with `k8s.nyl/apply-policy-scope` are applied
to the other resources instead of being emitted.

## Setup

1. Install Kyverno CLI:
   ```bash
   # For Linux/macOS
   curl -LO https://github.com/kyverno/kyverno/releases/download/v1.11.0/kyverno-cli_v1.11.0_linux_x86_64.tar.gz
   tar -xvzf kyverno-cli_v1.11.0_linux_x86_64.tar.gz
   sudo mv kyverno /usr/local/bin/
   ```

2. Render the example:
   ```bash
   nyl render examples/kyverno-postprocessor/manifests.yaml --offline --kube-version v1.28.0 --kube-api-versions apps/v1
   ```

## What This Example Shows

This example shows mutation policies applied to Service resources, and labels added to Services and Deployments.

See `manifests.yaml` for the full example configuration.

