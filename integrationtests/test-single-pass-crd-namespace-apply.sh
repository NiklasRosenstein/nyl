#!/usr/bin/env bash
set -euo pipefail

# Integration test: Single-pass apply with Namespace + CRD + CR
#
# This test validates that one `nyl apply` invocation can apply:
# - Namespace
# - CustomResourceDefinition
# - Custom resource in that namespace
# from a single manifest file.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_NAME="single-pass-crd-namespace-apply"
NAMESPACE="test-${TEST_NAME}"
CRD_NAME="widgets.example.com"
CR_NAME="test-widget"
MANIFEST_PATH="${SCRIPT_DIR}/${TEST_NAME}.yaml"

echo "==================================="
echo "Integration Test: ${TEST_NAME}"
echo "==================================="

cleanup() {
    echo ""
    echo "Cleaning up test resources..."
    kubectl delete namespace "${NAMESPACE}" --ignore-not-found=true || true
    kubectl delete crd "${CRD_NAME}" --ignore-not-found=true || true
    rm -f "${MANIFEST_PATH}"
}
trap cleanup EXIT

echo "Checking prerequisites..."
if ! command -v nyl &> /dev/null; then
    echo "ERROR: nyl binary not found in PATH"
    exit 1
fi

if ! command -v kubectl &> /dev/null; then
    echo "ERROR: kubectl not found in PATH"
    exit 1
fi

if ! kubectl cluster-info &> /dev/null; then
    echo "ERROR: Cannot connect to Kubernetes cluster"
    exit 1
fi

echo "✓ Prerequisites verified"
echo ""

echo "Creating test manifest: ${MANIFEST_PATH}"
cat > "${MANIFEST_PATH}" <<EOF
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: ${TEST_NAME}
  namespace: ${NAMESPACE}
---
apiVersion: v1
kind: Namespace
metadata:
  name: ${NAMESPACE}
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: ${CRD_NAME}
spec:
  group: example.com
  names:
    kind: Widget
    plural: widgets
    singular: widget
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
              properties:
                message:
                  type: string
---
apiVersion: example.com/v1
kind: Widget
metadata:
  name: ${CR_NAME}
  namespace: ${NAMESPACE}
spec:
  message: "hello from single-pass apply"
EOF
echo "✓ Test manifest created"
echo ""

echo "Running single-pass nyl apply..."
nyl apply "${MANIFEST_PATH}"
echo "✓ nyl apply completed"
echo ""

echo "Validating created resources..."
kubectl get namespace "${NAMESPACE}" > /dev/null
echo "✓ Namespace created"

kubectl get crd "${CRD_NAME}" > /dev/null
echo "✓ CRD created"

kubectl wait --for condition=established "crd/${CRD_NAME}" --timeout=30s > /dev/null
echo "✓ CRD established"

kubectl get widget "${CR_NAME}" -n "${NAMESPACE}" > /dev/null
echo "✓ Custom resource created"

WIDGET_MESSAGE="$(kubectl get widget "${CR_NAME}" -n "${NAMESPACE}" -o jsonpath='{.spec.message}')"
if [ "${WIDGET_MESSAGE}" != "hello from single-pass apply" ]; then
    echo "ERROR: Unexpected widget spec.message: ${WIDGET_MESSAGE}"
    exit 1
fi
echo "✓ Custom resource payload verified"

echo ""
echo "==================================="
echo "✅ TEST PASSED: ${TEST_NAME}"
echo "==================================="
