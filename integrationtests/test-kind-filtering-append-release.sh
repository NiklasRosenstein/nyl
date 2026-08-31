#!/usr/bin/env bash
set -euo pipefail

# Integration test: Kind Filtering + Append-Release
#
# This test validates the phased deployment workflow:
# 1. Apply CRDs only with --only-kind
# 2. Wait for CRDs to be established
# 3. Preview with diff --append-release
# 4. Apply remaining resources with --append-release
# 5. Verify CRD was not pruned
# 6. Verify all resources exist

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_NAME="kind-filtering-append-release"
NAMESPACE="test-${TEST_NAME}"

echo "==================================="
echo "Integration Test: ${TEST_NAME}"
echo "==================================="

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up test resources..."
    kubectl delete namespace "${NAMESPACE}" --ignore-not-found=true || true
    kubectl delete crd testresources.example.com --ignore-not-found=true || true
    rm -f "${SCRIPT_DIR}/${TEST_NAME}.yaml"
}

# Register cleanup on exit
trap cleanup EXIT

# Check prerequisites
if ! command -v nyl &> /dev/null; then
    echo "ERROR: nyl binary not found in PATH"
    exit 1
fi

if ! command -v kubectl &> /dev/null; then
    echo "ERROR: kubectl not found in PATH"
    exit 1
fi

# Verify cluster is accessible
if ! kubectl cluster-info &> /dev/null; then
    echo "ERROR: Cannot connect to Kubernetes cluster"
    exit 1
fi

echo "✓ Prerequisites verified"
echo ""

# Create test namespace
echo "Creating test namespace: ${NAMESPACE}"
kubectl create namespace "${NAMESPACE}"
echo "✓ Namespace created"
echo ""

# Create test manifest
echo "Creating test manifest..."
cat > "${SCRIPT_DIR}/${TEST_NAME}.yaml" <<'EOF'
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: test-filter
  namespace: test-kind-filtering-append-release
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: testresources.example.com
spec:
  group: example.com
  names:
    kind: TestResource
    plural: testresources
  scope: Namespaced
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
  namespace: test-kind-filtering-append-release
data:
  key: value
  test: "kind-filtering"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deployment
  namespace: test-kind-filtering-append-release
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test-filter
  template:
    metadata:
      labels:
        app: test-filter
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
        resources:
          requests:
            memory: "64Mi"
            cpu: "50m"
          limits:
            memory: "128Mi"
            cpu: "100m"
EOF
echo "✓ Test manifest created"
echo ""

# Phase 1: Apply only CRDs
echo "======================================="
echo "Phase 1: Applying CRDs only"
echo "======================================="
nyl apply "${SCRIPT_DIR}/${TEST_NAME}.yaml" --only-kind=CustomResourceDefinition

# Verify CRD exists
if ! kubectl get crd testresources.example.com &> /dev/null; then
    echo "ERROR: CRD not created"
    exit 1
fi
echo "✓ CRD created"

# Wait for CRD to be established
echo "Waiting for CRD to be established..."
if ! kubectl wait --for condition=established crd/testresources.example.com --timeout=30s; then
    echo "ERROR: CRD did not become established"
    kubectl describe crd testresources.example.com
    exit 1
fi
echo "✓ CRD established"
echo ""

# Preview Phase 2 with diff
echo "======================================="
echo "Previewing Phase 2 with diff"
echo "======================================="
nyl diff "${SCRIPT_DIR}/${TEST_NAME}.yaml" \
    --exclude-kind=CustomResourceDefinition \
    --append-release
echo ""

# Phase 2: Apply everything else with append-release
echo "======================================="
echo "Phase 2: Applying remaining resources"
echo "======================================="
nyl apply "${SCRIPT_DIR}/${TEST_NAME}.yaml" \
    --exclude-kind=CustomResourceDefinition \
    --append-release

# Verify CRD still exists (not pruned)
if ! kubectl get crd testresources.example.com &> /dev/null; then
    echo "ERROR: CRD was pruned (should have been preserved by --append-release)"
    exit 1
fi
echo "✓ CRD still exists (not pruned)"

# Verify ConfigMap exists
if ! kubectl get configmap test-config -n "${NAMESPACE}" &> /dev/null; then
    echo "ERROR: ConfigMap not created"
    exit 1
fi
echo "✓ ConfigMap created"

# Verify Deployment exists
if ! kubectl get deployment test-deployment -n "${NAMESPACE}" &> /dev/null; then
    echo "ERROR: Deployment not created"
    exit 1
fi
echo "✓ Deployment created"

# Verify deployment has correct labels
LABELS=$(kubectl get deployment test-deployment -n "${NAMESPACE}" -o jsonpath='{.spec.template.metadata.labels.app}')
if [ "${LABELS}" != "test-filter" ]; then
    echo "ERROR: Deployment has incorrect labels: ${LABELS}"
    exit 1
fi
echo "✓ Deployment has correct labels"

# Additional validation: Check nyl release state
echo ""
echo "Validating nyl release state..."
RELEASE_NAME="test-filter"
RELEASE_CONFIGMAP=$(kubectl get configmap -n "${NAMESPACE}" -l "app.kubernetes.io/name=nyl,app.kubernetes.io/instance=${RELEASE_NAME}" -o name | head -1)

if [ -z "${RELEASE_CONFIGMAP}" ]; then
    echo "WARNING: No nyl release ConfigMap found (might be stored differently)"
else
    echo "✓ Nyl release state found: ${RELEASE_CONFIGMAP}"
fi

echo ""
echo "==================================="
echo "✅ TEST PASSED: ${TEST_NAME}"
echo "==================================="
echo "Summary:"
echo "  - CRDs applied in Phase 1"
echo "  - CRDs preserved with --append-release in Phase 2"
echo "  - All resources created successfully"
echo "  - No pruning occurred"
echo ""
