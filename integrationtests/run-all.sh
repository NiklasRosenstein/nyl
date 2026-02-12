#!/usr/bin/env bash
set -euo pipefail

# Run all integration tests
#
# Usage:
#   ./run-all.sh              # Run all tests against existing cluster
#   ./run-all.sh --recreate   # Recreate Minikube between each test (slow but isolated)
#
# Optional environment variables:
#   RUN_ARGOCD_CREDENTIAL_LOOKUP_TEST=true
#     Include test-argocd-credential-lookup-local.sh in the test run.
#     This test requires TEST_REPO_URL, TEST_REPO_USERNAME, TEST_REPO_PASSWORD.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECREATE_CLUSTER=false
KUBERNETES_VERSION="v1.29.0"

# Parse arguments
if [ "${1:-}" = "--recreate" ]; then
    RECREATE_CLUSTER=true
    echo "⚠️  Cluster recreation enabled - tests will run in full isolation"
    echo ""
fi

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

echo "✓ Prerequisites verified"
echo ""

# Find all test scripts
TEST_SCRIPTS=()
while IFS= read -r script; do
    test_name=$(basename "${script}")
    if [ "${test_name}" = "test-argocd-credential-lookup-local.sh" ] && \
        [ "${RUN_ARGOCD_CREDENTIAL_LOOKUP_TEST:-false}" != "true" ]; then
        continue
    fi
    TEST_SCRIPTS+=("${script}")
done < <(find "${SCRIPT_DIR}" -name "test-*.sh" -type f | sort)

if [ ${#TEST_SCRIPTS[@]} -eq 0 ]; then
    echo "ERROR: No test scripts found in ${SCRIPT_DIR}"
    exit 1
fi

echo "Found ${#TEST_SCRIPTS[@]} integration test(s)"
if [ "${RUN_ARGOCD_CREDENTIAL_LOOKUP_TEST:-false}" != "true" ]; then
    echo "Note: skipping test-argocd-credential-lookup-local.sh (set RUN_ARGOCD_CREDENTIAL_LOOKUP_TEST=true to include)"
fi
echo ""

# Track results
PASSED=0
FAILED=0
FAILED_TESTS=()

# Run each test
for test_script in "${TEST_SCRIPTS[@]}"; do
    test_name=$(basename "${test_script}")

    echo "######################################"
    echo "# Running: ${test_name}"
    echo "######################################"
    echo ""

    # Recreate cluster if requested
    if [ "${RECREATE_CLUSTER}" = true ]; then
        echo "Recreating Minikube cluster..."
        if command -v minikube &> /dev/null; then
            minikube delete || true
            minikube start --kubernetes-version="${KUBERNETES_VERSION}" --wait=all
            echo "✓ Fresh cluster ready"
            echo ""
        else
            echo "WARNING: minikube not found, skipping cluster recreation"
            echo ""
        fi
    fi

    # Run test
    if bash "${test_script}"; then
        echo ""
        echo "✅ PASSED: ${test_name}"
        ((PASSED++))
    else
        echo ""
        echo "❌ FAILED: ${test_name}"
        ((FAILED++))
        FAILED_TESTS+=("${test_name}")
    fi

    echo ""
    echo ""
done

# Print summary
echo "######################################"
echo "# Integration Test Summary"
echo "######################################"
echo ""
echo "Total:  ${#TEST_SCRIPTS[@]}"
echo "Passed: ${PASSED}"
echo "Failed: ${FAILED}"
echo ""

if [ ${FAILED} -eq 0 ]; then
    echo "✅ All integration tests passed!"
    exit 0
else
    echo "❌ Some integration tests failed:"
    for test in "${FAILED_TESTS[@]}"; do
        echo "  - ${test}"
    done
    exit 1
fi
