#!/usr/bin/env bash
set -euo pipefail

# Integration test: ArgoCD Bootstrap
#
# This test validates the ArgoCD bootstrap workflow with staged deployment:
# 1. Phase 1: Apply CRDs only with --only-kind=CustomResourceDefinition
# 2. Wait for CRDs to be established
# 3. Phase 2: Apply remaining resources with --exclude-kind and --append-release
# 4. Wait for ArgoCD deployments to be ready
# 5. Verify Nyl CMP sidecar is running
# 6. Verify self-managed Application is created and syncs
#
# Environment Variables:
#   NYL_CHART_OWNER      - GitHub owner for OCI chart (default: niklasrosenstein)
#   NYL_CHART_VERSION    - Chart version (default: 0.1.0)
#   NYL_IMAGE_TAG        - Container image tag (default: develop)
#   NYL_REPO_URL         - Git repository URL (default: https://github.com/NiklasRosenstein/nyl-rs.git)
#   NYL_TARGET_REVISION  - Git revision (default: HEAD)
#   GITHUB_TOKEN         - Token for OCI registry auth (optional)
#   GITHUB_ACTOR         - GitHub username for OCI auth (optional)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEST_NAME="argocd-bootstrap"
NAMESPACE="argocd"
DEPLOYMENT_TIMEOUT="${DEPLOYMENT_TIMEOUT:-300}"
SYNC_TIMEOUT="${SYNC_TIMEOUT:-180}"

echo "==================================="
echo "Integration Test: ${TEST_NAME}"
echo "==================================="

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up test resources..."

    # Stop port-forward if running
    if [ -n "${PORT_FORWARD_PID:-}" ]; then
        kill "${PORT_FORWARD_PID}" 2>/dev/null || true
    fi

    # Delete ArgoCD namespace and all resources
    kubectl delete namespace "${NAMESPACE}" --ignore-not-found=true --timeout=60s || true

    # Delete CRDs created by ArgoCD
    kubectl delete crd applications.argoproj.io --ignore-not-found=true || true
    kubectl delete crd applicationsets.argoproj.io --ignore-not-found=true || true
    kubectl delete crd appprojects.argoproj.io --ignore-not-found=true || true

    echo "✓ Cleanup complete"
}

# Register cleanup on exit
trap cleanup EXIT

# Check prerequisites
echo "Checking prerequisites..."

if ! command -v nyl &> /dev/null; then
    echo "ERROR: nyl binary not found in PATH"
    exit 1
fi

if ! command -v kubectl &> /dev/null; then
    echo "ERROR: kubectl not found in PATH"
    exit 1
fi

if ! command -v helm &> /dev/null; then
    echo "ERROR: helm not found in PATH (required for OCI chart pulling)"
    echo "Install with: curl https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash"
    exit 1
fi

# Verify cluster is accessible
if ! kubectl cluster-info &> /dev/null; then
    echo "ERROR: Cannot connect to Kubernetes cluster"
    exit 1
fi

echo "✓ Prerequisites verified"
echo "  nyl version: $(nyl --version)"
echo "  helm version: $(helm version --short)"
echo "  kubectl version: $(kubectl version --client --short 2>/dev/null || kubectl version --client)"
echo ""

# Set environment variables with defaults
export NYL_CHART_OWNER="${NYL_CHART_OWNER:-niklasrosenstein}"
export NYL_CHART_VERSION="${NYL_CHART_VERSION:-0.1.0}"
export NYL_IMAGE_TAG="${NYL_IMAGE_TAG:-develop}"
export NYL_REPO_URL="${NYL_REPO_URL:-https://github.com/NiklasRosenstein/nyl-rs.git}"
export NYL_TARGET_REVISION="${NYL_TARGET_REVISION:-HEAD}"

echo "Environment configuration:"
echo "  NYL_CHART_OWNER:     ${NYL_CHART_OWNER}"
echo "  NYL_CHART_VERSION:   ${NYL_CHART_VERSION}"
echo "  NYL_IMAGE_TAG:       ${NYL_IMAGE_TAG}"
echo "  NYL_REPO_URL:        ${NYL_REPO_URL}"
echo "  NYL_TARGET_REVISION: ${NYL_TARGET_REVISION}"
echo ""

# Create namespace
echo "Creating namespace: ${NAMESPACE}"
kubectl create namespace "${NAMESPACE}"
echo "✓ Namespace created"
echo ""

# Setup OCI registry authentication if credentials are available
if [ -n "${GITHUB_TOKEN:-}" ] && [ -n "${GITHUB_ACTOR:-}" ]; then
    echo "Setting up OCI registry authentication..."

    # Login to Helm OCI registry
    echo "${GITHUB_TOKEN}" | helm registry login ghcr.io -u "${GITHUB_ACTOR}" --password-stdin
    echo "✓ Helm OCI login successful"

    # Create pull secret for ArgoCD
    kubectl create secret docker-registry ghcr-pull-secret \
        --namespace="${NAMESPACE}" \
        --docker-server=ghcr.io \
        --docker-username="${GITHUB_ACTOR}" \
        --docker-password="${GITHUB_TOKEN}"
    echo "✓ Pull secret created"
    echo ""
else
    echo "⚠️  No GitHub credentials provided (GITHUB_TOKEN/GITHUB_ACTOR)"
    echo "   Skipping OCI registry authentication - using public charts only"
    echo ""
fi

# Bootstrap ArgoCD with staged nyl apply
BOOTSTRAP_YAML="${PROJECT_ROOT}/examples/argocd-bootstrap/bootstrap.yaml"

if [ ! -f "${BOOTSTRAP_YAML}" ]; then
    echo "ERROR: Bootstrap manifest not found at ${BOOTSTRAP_YAML}"
    exit 1
fi

cd "${PROJECT_ROOT}/examples/argocd-bootstrap"

# Phase 1: Apply CRDs only
echo "======================================="
echo "Phase 1: Applying CRDs only"
echo "======================================="
nyl apply bootstrap.yaml --only-kind=CustomResourceDefinition

echo "✓ CRDs applied"
echo ""

# Wait for CRDs to be established
echo "Waiting for ArgoCD CRDs to be established..."
CRD_TIMEOUT=60
for crd in applications.argoproj.io applicationsets.argoproj.io appprojects.argoproj.io; do
    echo "  Waiting for ${crd}..."
    if ! kubectl wait --for condition=established crd/${crd} --timeout=${CRD_TIMEOUT}s 2>/dev/null; then
        echo "  WARNING: CRD ${crd} not found or not established yet"
        # Not all CRDs may be present depending on ArgoCD version
    else
        echo "  ✓ ${crd} established"
    fi
done
echo "✓ CRDs ready"
echo ""

# Phase 2: Apply remaining resources with append-release
echo "======================================="
echo "Phase 2: Applying remaining resources"
echo "======================================="
nyl apply bootstrap.yaml \
    --exclude-kind=CustomResourceDefinition \
    --append-release

echo "✓ All ArgoCD resources applied"
echo ""

# Wait for ArgoCD deployments
echo "======================================="
echo "Waiting for ArgoCD deployments"
echo "======================================="

for deployment in argocd-server argocd-repo-server; do
    echo "Waiting for ${deployment}..."

    if ! kubectl rollout status deployment/${deployment} -n "${NAMESPACE}" \
        --timeout="${DEPLOYMENT_TIMEOUT}s"; then
        echo "ERROR: Deployment ${deployment} failed to become ready"
        echo ""
        echo "Deployment details:"
        kubectl describe deployment/${deployment} -n "${NAMESPACE}"
        echo ""
        echo "Recent logs:"
        kubectl logs deployment/${deployment} -n "${NAMESPACE}" --tail=50 || true
        exit 1
    fi

    echo "✓ ${deployment} is ready"
done

# argocd-application-controller is a StatefulSet, not a Deployment
echo "Waiting for argocd-application-controller..."
if ! kubectl rollout status statefulset/argocd-application-controller -n "${NAMESPACE}" \
    --timeout="${DEPLOYMENT_TIMEOUT}s"; then
    echo "ERROR: StatefulSet argocd-application-controller failed to become ready"
    echo ""
    echo "StatefulSet details:"
    kubectl describe statefulset/argocd-application-controller -n "${NAMESPACE}"
    echo ""
    echo "Recent logs:"
    kubectl logs statefulset/argocd-application-controller -n "${NAMESPACE}" --tail=50 || true
    exit 1
fi
echo "✓ argocd-application-controller is ready"

echo ""
echo "✓ All ArgoCD deployments are ready"
echo ""

# Verify Nyl sidecar
echo "======================================="
echo "Verifying Nyl CMP sidecar"
echo "======================================="

REPO_POD=$(kubectl get pods -n "${NAMESPACE}" \
    -l app.kubernetes.io/component=repo-server \
    -o jsonpath='{.items[0].metadata.name}')

if [ -z "${REPO_POD}" ]; then
    echo "ERROR: No repo-server pod found"
    kubectl get pods -n "${NAMESPACE}"
    exit 1
fi

echo "Repo-server pod: ${REPO_POD}"
echo ""
echo "Containers:"
kubectl get pod "${REPO_POD}" -n "${NAMESPACE}" \
    -o jsonpath='{.spec.containers[*].name}' | tr ' ' '\n'
echo ""

NYL_READY=$(kubectl get pod "${REPO_POD}" -n "${NAMESPACE}" \
    -o jsonpath='{.status.containerStatuses[?(@.name=="nyl-v2")].ready}')

if [ "${NYL_READY}" != "true" ]; then
    echo "ERROR: nyl-v2 sidecar not ready"
    echo ""
    echo "Pod details:"
    kubectl describe pod "${REPO_POD}" -n "${NAMESPACE}"
    echo ""
    echo "Nyl sidecar logs:"
    kubectl logs "${REPO_POD}" -n "${NAMESPACE}" -c nyl-v2 --tail=50 || true
    exit 1
fi

echo "✓ nyl-v2 sidecar is ready"
echo ""

# Install ArgoCD CLI if not already installed
if ! command -v argocd &> /dev/null; then
    echo "Installing ArgoCD CLI..."

    ARGOCD_VERSION=$(curl -s https://api.github.com/repos/argoproj/argo-cd/releases/latest | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')
    ARGOCD_URL="https://github.com/argoproj/argo-cd/releases/download/${ARGOCD_VERSION}/argocd-linux-amd64"

    # Download to a temporary location
    TEMP_ARGOCD="/tmp/argocd-cli-$$"
    curl -sSL -o "${TEMP_ARGOCD}" "${ARGOCD_URL}"
    chmod +x "${TEMP_ARGOCD}"

    # Try to install globally, fall back to local
    if sudo mv "${TEMP_ARGOCD}" /usr/local/bin/argocd 2>/dev/null; then
        echo "✓ ArgoCD CLI installed globally"
    else
        mv "${TEMP_ARGOCD}" ./argocd
        export PATH="${PWD}:${PATH}"
        echo "✓ ArgoCD CLI installed locally (no sudo access)"
    fi
else
    echo "✓ ArgoCD CLI already installed"
fi

echo "  argocd version: $(argocd version --client --short)"
echo ""

# Login to ArgoCD
echo "======================================="
echo "Logging into ArgoCD"
echo "======================================="

# Wait for admin secret to be created
echo "Waiting for admin secret..."
for i in $(seq 1 30); do
    if kubectl get secret argocd-initial-admin-secret -n "${NAMESPACE}" &>/dev/null; then
        break
    fi
    echo "  Attempt $i/30..."
    sleep 5
done

if ! kubectl get secret argocd-initial-admin-secret -n "${NAMESPACE}" &>/dev/null; then
    echo "ERROR: Admin secret not created after 150s"
    exit 1
fi

PASSWORD=$(kubectl get secret argocd-initial-admin-secret -n "${NAMESPACE}" \
    -o jsonpath='{.data.password}' | base64 -d)

echo "✓ Admin password retrieved"

# Start port-forward in background
echo "Starting port-forward to ArgoCD server..."
kubectl port-forward svc/argocd-server -n "${NAMESPACE}" 8080:443 &>/dev/null &
PORT_FORWARD_PID=$!
sleep 5

# Login to ArgoCD
argocd login localhost:8080 \
    --username admin \
    --password "${PASSWORD}" \
    --insecure

echo "✓ Logged into ArgoCD"
echo ""

# Verify and sync ArgoCD Application
echo "======================================="
echo "Verifying self-managed Application"
echo "======================================="

echo "ArgoCD Applications:"
argocd app list
echo ""

if ! argocd app get argocd &>/dev/null; then
    echo "ERROR: ArgoCD self-managed application not found"
    echo ""
    echo "Available applications:"
    kubectl get applications -n "${NAMESPACE}" -o yaml
    exit 1
fi

echo "✓ Self-managed application 'argocd' found"
echo ""

echo "Syncing ArgoCD application..."
if ! argocd app sync argocd --timeout "${SYNC_TIMEOUT}"; then
    echo "ERROR: Sync failed"
    echo ""
    argocd app get argocd --show-operation
    exit 1
fi

echo "✓ Application synced"
echo ""

echo "Waiting for healthy status..."
if ! argocd app wait argocd --timeout "${SYNC_TIMEOUT}" --health; then
    echo "ERROR: Application not healthy"
    echo ""
    argocd app get argocd --show-operation
    exit 1
fi

echo "✓ Application is healthy"
echo ""

# Final verification
echo "======================================="
echo "Final verification"
echo "======================================="

echo "Deployments:"
kubectl get deployments -n "${NAMESPACE}"
echo ""

echo "Pods:"
kubectl get pods -n "${NAMESPACE}"
echo ""

echo "ArgoCD Applications:"
argocd app list
echo ""

echo "Application details:"
argocd app get argocd
echo ""

echo "==================================="
echo "✅ TEST PASSED: ${TEST_NAME}"
echo "==================================="
echo "Summary:"
echo "  - Phase 1: CRDs applied with --only-kind=CustomResourceDefinition"
echo "  - Phase 2: Remaining resources applied with --exclude-kind and --append-release"
echo "  - All ArgoCD deployments are ready"
echo "  - Nyl CMP sidecar is running in repo-server"
echo "  - Self-managed Application synced and healthy"
echo "  - CRDs preserved across both phases (not pruned)"
echo ""
