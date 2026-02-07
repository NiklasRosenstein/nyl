# Kyverno Post-Processor Example

This example demonstrates how to use the Kyverno post-processor in Nyl to apply policies to Kubernetes manifests at render time.

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

## Example Features

The example demonstrates:

1. **Inline Policies**: Full Kyverno policy resources defined inline
2. **Shorthand Rules**: Using convenience fields like `mutatingPolicyRules` to reduce boilerplate
3. **Global Scope**: Policies that apply to all resources in the render

## Files

- `manifests.yaml`: Contains both resources to be mutated and the Kyverno post-processor configuration
