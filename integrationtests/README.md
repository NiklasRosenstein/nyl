# Integration Tests

This directory contains integration tests for Nyl that require a live Kubernetes cluster.

## Overview

Each test is a standalone bash script that:
- Validates specific Nyl functionality
- Can be run manually against any cluster
- Cleans up resources on exit (success or failure)
- Reports clear pass/fail status

## Prerequisites

- `nyl` binary in PATH
- `kubectl` configured to connect to a cluster
- Sufficient cluster permissions to create/delete resources

## Running Tests

### Run all tests

```bash
./run-all.sh
```

### Run a specific test

```bash
./test-kind-filtering-append-release.sh
./test-argocd-bootstrap.sh
```

### Run with fresh cluster (recommended for CI)

```bash
# Start fresh Minikube cluster
minikube delete || true
minikube start --kubernetes-version=v1.29.0

# Run tests
./run-all.sh

# Cleanup
minikube delete
```

## Available Tests

### test-kind-filtering-append-release.sh

Tests phased deployment with kind filtering and append-release mode:
- Applies CRDs first with `--only-kind=CustomResourceDefinition`
- Waits for CRD establishment
- Previews Phase 2 with `diff --append-release`
- Applies remaining resources with `--append-release`
- Verifies CRD was not pruned
- Validates all resources exist

**Duration:** ~30 seconds
**Resources:** 1 namespace, 1 CRD, 1 ConfigMap, 1 Deployment

### test-argocd-bootstrap.sh

Tests ArgoCD deployment using the Nyl Helm chart OCI image with staged deployment:
- **Phase 1:** Applies CRDs only with `--only-kind=CustomResourceDefinition`
- Waits for CRDs to be established
- **Phase 2:** Applies remaining resources with `--exclude-kind=CustomResourceDefinition --append-release`
- Waits for ArgoCD deployments (server, repo-server, application-controller)
- Verifies Nyl CMP sidecar is running in the repo-server pod
- Installs ArgoCD CLI
- Logs into ArgoCD and verifies self-managed Application
- Syncs the Application and validates health
- Verifies CRDs were not pruned during Phase 2

**Duration:** ~5-10 minutes (includes ArgoCD installation)
**Resources:** ArgoCD namespace with full ArgoCD installation (~10 deployments/statefulsets)
**Prerequisites:** Helm CLI installed
**Optional:** `GITHUB_TOKEN` and `GITHUB_ACTOR` environment variables for OCI registry authentication

## Writing New Tests

### Test Script Template

```bash
#!/usr/bin/env bash
set -euo pipefail

# Test metadata
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_NAME="my-test-name"
NAMESPACE="test-${TEST_NAME}"

echo "==================================="
echo "Integration Test: ${TEST_NAME}"
echo "==================================="

# Cleanup function
cleanup() {
    echo "Cleaning up test resources..."
    kubectl delete namespace "${NAMESPACE}" --ignore-not-found=true || true
    # Add other cleanup as needed
}
trap cleanup EXIT

# Check prerequisites
if ! command -v nyl &> /dev/null; then
    echo "ERROR: nyl binary not found"
    exit 1
fi

if ! kubectl cluster-info &> /dev/null; then
    echo "ERROR: Cannot connect to cluster"
    exit 1
fi

# Test logic here...

echo "✅ TEST PASSED: ${TEST_NAME}"
```

### Best Practices

1. **Use unique names** - Namespace, resource names should be unique per test
2. **Always cleanup** - Use trap to ensure cleanup runs on exit
3. **Check prerequisites** - Verify nyl, kubectl, cluster access
4. **Be verbose** - Print clear status messages
5. **Test isolation** - Each test should be independent
6. **Fast failure** - Use `set -e` to fail fast on errors
7. **Validate thoroughly** - Check all expected resources exist

## CI Integration

Tests run automatically in the Integration CI workflow (`.github/workflows/ci-integration.yaml`).

The CI workflow:
1. Sets up Minikube with Kubernetes v1.29.0
2. Installs nyl binary
3. Runs all integration tests in sequence
4. Optionally recreates cluster between tests for isolation

## Troubleshooting

### Test fails with "Cannot connect to cluster"

Ensure kubectl is configured:
```bash
kubectl cluster-info
kubectl get nodes
```

### Test fails with "nyl binary not found"

Install nyl or add it to PATH:
```bash
# Build from source
cd nyl && cargo build --release
export PATH="$PWD/target/release:$PATH"

# Or use released binary
nyl --version
```

### Resources not cleaned up

Manually run cleanup:
```bash
kubectl delete namespace test-kind-filtering-append-release
kubectl delete crd testresources.example.com
```

### Need to debug a test

Run with bash debug mode:
```bash
bash -x ./test-kind-filtering-append-release.sh
```

## Adding Tests to CI

Edit `.github/workflows/ci-integration.yaml` and add your test to the test matrix or sequence:

```yaml
- name: Run integration tests
  run: |
    cd integrationtests
    ./test-kind-filtering-append-release.sh
    ./test-your-new-feature.sh  # Add here
```
