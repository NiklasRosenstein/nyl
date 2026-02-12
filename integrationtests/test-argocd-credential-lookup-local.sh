#!/usr/bin/env bash
set -euo pipefail

# Integration test: ArgoCD credential lookup (local cluster)
#
# This test validates that Nyl discovers ArgoCD repository secrets when running
# in-cluster and uses them for Git authentication.
#
# Required environment variables:
#   TEST_REPO_URL       - Private Git repository URL (e.g. https://github.com/org/repo.git)
#   TEST_REPO_USERNAME  - Username for HTTPS auth
#   TEST_REPO_PASSWORD  - Token/password for HTTPS auth
#
# Optional:
#   TEST_SOURCE_PATH    - Path inside the repo to scan (default: .)
#   NYL_BIN             - Explicit path to nyl binary (default: auto-detect from cargo target dirs)
#   NYL_SKIP_BUILD      - Set to "true" to skip automatic cargo build
#   INSTALL_CA_CERTS    - true|false|auto (default: auto; installs for https:// TEST_REPO_URL)
#   REQUIRE_PRIVATE_REPO - Set to "false" to allow public-repo behavior (default: true)

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_NAME="argocd-credential-lookup-local"
ARGOCD_NAMESPACE="argocd"
RUNNER_NAMESPACE="${RUNNER_NAMESPACE:-${ARGOCD_NAMESPACE}}"
POD_NAME="nyl-credtest-runner"
SECRET_NAME="nyl-credtest-repo"
ROLE_NAME="nyl-credtest-secret-reader"
MANIFEST_FILE="/tmp/apps.yaml"
TEST_SOURCE_PATH="${TEST_SOURCE_PATH:-.}"
RUNNER_IMAGE="${RUNNER_IMAGE:-debian:bookworm-slim}"
INSTALL_CA_CERTS="${INSTALL_CA_CERTS:-auto}"
REQUIRE_PRIVATE_REPO="${REQUIRE_PRIVATE_REPO:-true}"

echo "==================================="
echo "Integration Test: ${TEST_NAME}"
echo "==================================="
if [ -z "${TEST_REPO_URL:-}" ] || [ -z "${TEST_REPO_USERNAME:-}" ] || [ -z "${TEST_REPO_PASSWORD:-}" ]; then
    echo "ERROR: Missing required environment variables."
    echo "Set TEST_REPO_URL, TEST_REPO_USERNAME, and TEST_REPO_PASSWORD."
    exit 1
fi

cleanup() {
    echo ""
    echo "Cleaning up test resources..."
    kubectl -n "${RUNNER_NAMESPACE}" delete pod "${POD_NAME}" --ignore-not-found=true >/dev/null 2>&1 || true
    kubectl -n "${ARGOCD_NAMESPACE}" delete secret "${SECRET_NAME}" --ignore-not-found=true >/dev/null 2>&1 || true
    kubectl -n "${ARGOCD_NAMESPACE}" delete role "${ROLE_NAME}" --ignore-not-found=true >/dev/null 2>&1 || true
    kubectl -n "${ARGOCD_NAMESPACE}" delete rolebinding "${ROLE_NAME}" --ignore-not-found=true >/dev/null 2>&1 || true

    if [ "${RUNNER_NAMESPACE_CREATED_BY_TEST:-false}" = "true" ]; then
        kubectl delete namespace "${RUNNER_NAMESPACE}" --ignore-not-found=true >/dev/null 2>&1 || true
    fi

    # Only delete argocd namespace if this test created it.
    if [ "${ARGOCD_NAMESPACE_CREATED_BY_TEST:-false}" = "true" ]; then
        kubectl delete namespace "${ARGOCD_NAMESPACE}" --ignore-not-found=true >/dev/null 2>&1 || true
    fi
    echo "✓ Cleanup complete"
}
trap cleanup EXIT

resolve_local_nyl_bin() {
    if [ -n "${NYL_BIN:-}" ]; then
        if [ ! -x "${NYL_BIN}" ]; then
            echo "ERROR: NYL_BIN is set but not executable: ${NYL_BIN}"
            exit 1
        fi
        echo "${NYL_BIN}"
        return
    fi

    local candidates=(
        "${PROJECT_ROOT}/target/x86_64-unknown-linux-musl/debug/nyl"
        "${PROJECT_ROOT}/target/x86_64-unknown-linux-musl/release/nyl"
        "${PROJECT_ROOT}/nyl/target/x86_64-unknown-linux-musl/debug/nyl"
        "${PROJECT_ROOT}/nyl/target/x86_64-unknown-linux-musl/release/nyl"
        "${PROJECT_ROOT}/target/debug/nyl"
        "${PROJECT_ROOT}/target/release/nyl"
        "${PROJECT_ROOT}/nyl/target/debug/nyl"
        "${PROJECT_ROOT}/nyl/target/release/nyl"
    )
    local newest=""
    local newest_mtime=0

    for bin in "${candidates[@]}"; do
        if [ -x "${bin}" ]; then
            local mtime
            mtime="$(stat -c %Y "${bin}" 2>/dev/null || echo 0)"
            if [ "${mtime}" -gt "${newest_mtime}" ]; then
                newest="${bin}"
                newest_mtime="${mtime}"
            fi
        fi
    done

    if [ -z "${newest}" ]; then
        echo "ERROR: Could not find built nyl binary in target directories."
        echo "Expected one of:"
        printf '  - %s\n' "${candidates[@]}"
        echo "Build it first, e.g.: cargo build -p nyl"
        exit 1
    fi

    echo "${newest}"
}

echo "Checking prerequisites..."
if ! command -v kubectl >/dev/null 2>&1; then
    echo "ERROR: kubectl not found in PATH"
    exit 1
fi
if ! kubectl cluster-info >/dev/null 2>&1; then
    echo "ERROR: Cannot connect to Kubernetes cluster"
    exit 1
fi

if [ -z "${NYL_BIN:-}" ] && [ "${NYL_SKIP_BUILD:-false}" != "true" ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "ERROR: cargo not found in PATH (required for automatic build)."
        echo "Either install cargo or set NYL_BIN=/path/to/nyl."
        exit 1
    fi

    if ! command -v rustup >/dev/null 2>&1; then
        echo "ERROR: rustup not found in PATH."
        echo "Install rustup or set NYL_BIN=/path/to/nyl to skip automatic build."
        exit 1
    fi

    # Ensure musl target + linker are available for reproducible in-pod execution.
    if ! rustup target list --installed | grep -q '^x86_64-unknown-linux-musl$'; then
        echo "Installing Rust target: x86_64-unknown-linux-musl"
        if ! rustup target add x86_64-unknown-linux-musl; then
            echo "ERROR: Failed to install rust target x86_64-unknown-linux-musl."
            echo "Run manually: rustup target add x86_64-unknown-linux-musl"
            exit 1
        fi
    fi

    if command -v musl-gcc >/dev/null 2>&1; then
        export CC_x86_64_unknown_linux_musl="musl-gcc"
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="musl-gcc"
    elif command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
        export CC_x86_64_unknown_linux_musl="x86_64-linux-musl-gcc"
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="x86_64-linux-musl-gcc"
    else
        echo "ERROR: musl linker not found (need musl-gcc or x86_64-linux-musl-gcc)."
        echo "Install it, for example on Debian/Ubuntu:"
        echo "  sudo apt-get update && sudo apt-get install -y musl-tools"
        echo ""
        echo "Or set NYL_BIN to a prebuilt musl binary and rerun."
        exit 1
    fi

    echo "Building nyl binary for musl target (x86_64-unknown-linux-musl)..."
    if ! (cd "${PROJECT_ROOT}" && cargo build -p nyl --target x86_64-unknown-linux-musl); then
        echo "ERROR: Failed to build nyl for x86_64-unknown-linux-musl."
        echo "Using linker: ${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-unset}"
        echo "Set NYL_BIN to a prebuilt binary to bypass automatic build."
        exit 1
    fi
    echo "✓ Build complete"
fi

LOCAL_NYL_BIN="$(resolve_local_nyl_bin)"
echo "✓ Prerequisites verified"
echo "  nyl binary: ${LOCAL_NYL_BIN}"
NYL_VERSION="$("${LOCAL_NYL_BIN}" --version 2>/dev/null || true)"
if [ -z "${NYL_VERSION}" ]; then
    NYL_VERSION="unknown (could not query via --version)"
fi
echo "  nyl version: ${NYL_VERSION}"
echo ""

ARGOCD_NAMESPACE_CREATED_BY_TEST=false
if ! kubectl get namespace "${ARGOCD_NAMESPACE}" >/dev/null 2>&1; then
    kubectl create namespace "${ARGOCD_NAMESPACE}" >/dev/null
    ARGOCD_NAMESPACE_CREATED_BY_TEST=true
fi

RUNNER_NAMESPACE_CREATED_BY_TEST=false
if ! kubectl get namespace "${RUNNER_NAMESPACE}" >/dev/null 2>&1; then
    kubectl create namespace "${RUNNER_NAMESPACE}" >/dev/null
    RUNNER_NAMESPACE_CREATED_BY_TEST=true
fi

echo "Granting ${RUNNER_NAMESPACE}/default secret-read access in ${ARGOCD_NAMESPACE}..."
kubectl -n "${ARGOCD_NAMESPACE}" apply -f - <<EOF
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: ${ROLE_NAME}
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get", "list"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: ${ROLE_NAME}
subjects:
  - kind: ServiceAccount
    name: default
    namespace: ${RUNNER_NAMESPACE}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: ${ROLE_NAME}
EOF
echo "✓ RBAC configured"
echo ""

echo "Starting test runner pod..."
kubectl -n "${RUNNER_NAMESPACE}" run "${POD_NAME}" --image="${RUNNER_IMAGE}" --restart=Never --command -- sleep 3600
kubectl -n "${RUNNER_NAMESPACE}" wait --for=condition=Ready "pod/${POD_NAME}" --timeout=120s
echo "✓ Runner pod ready"
echo ""

# For static musl builds no runtime libc packages are required.
# Ensure git is available for worktree operations. Install CA certs only when explicitly
# requested or when auto-detected for HTTPS.
SHOULD_INSTALL_CA_CERTS=false
case "${INSTALL_CA_CERTS}" in
    true)
        SHOULD_INSTALL_CA_CERTS=true
        ;;
    false)
        SHOULD_INSTALL_CA_CERTS=false
        ;;
    auto)
        if [[ "${TEST_REPO_URL}" == https://* ]]; then
            SHOULD_INSTALL_CA_CERTS=true
        fi
        ;;
    *)
        echo "ERROR: INSTALL_CA_CERTS must be one of: true, false, auto"
        exit 1
        ;;
esac

echo "Ensuring git is available in pod..."
set +e
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- env WANT_CA_CERTS="${SHOULD_INSTALL_CA_CERTS}" sh -c '
if command -v git >/dev/null 2>&1; then
  exit 0
fi
pkgs="git"
if [ "${WANT_CA_CERTS}" = "true" ]; then
  pkgs="${pkgs} ca-certificates"
fi
if command -v apt-get >/dev/null 2>&1; then
  apt-get update && apt-get install -y --no-install-recommends ${pkgs} && rm -rf /var/lib/apt/lists/*
elif command -v apk >/dev/null 2>&1; then
  apk add --no-cache ${pkgs}
elif command -v microdnf >/dev/null 2>&1; then
  microdnf install -y ${pkgs} && microdnf clean all
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y ${pkgs} && dnf clean all
elif command -v yum >/dev/null 2>&1; then
  yum install -y ${pkgs} && yum clean all
else
  echo "No supported package manager found to install: ${pkgs}" >&2
  exit 127
fi
'
INSTALL_DEPS_RC=$?
set -e
if [ "${INSTALL_DEPS_RC}" -ne 0 ]; then
    echo "ERROR: Failed to ensure git in runner pod."
    echo "The runner image '${RUNNER_IMAGE}' may not include a package manager (common with BusyBox)."
    echo "Use a runner image with package manager support, e.g.:"
    echo "  RUNNER_IMAGE=debian:bookworm-slim ./integrationtests/test-argocd-credential-lookup-local.sh"
    kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- sh -c "cat /etc/os-release 2>/dev/null || true"
    exit 1
fi
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- git --version >/dev/null 2>&1 || {
    echo "ERROR: git is still not available in runner pod after installation attempt."
    exit 1
}
echo "✓ git available in pod"
if [ "${SHOULD_INSTALL_CA_CERTS}" = "true" ]; then
    echo "✓ CA certificates requested for HTTPS repositories"
else
    echo "CA certificate installation not requested"
fi
echo ""

echo "Copying local nyl binary into pod..."
kubectl -n "${RUNNER_NAMESPACE}" exec -i "${POD_NAME}" -- sh -c "cat > /tmp/nyl && chmod +x /tmp/nyl" < "${LOCAL_NYL_BIN}"
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- test -x /tmp/nyl
echo "✓ Nyl binary copied"
echo ""

echo "Preflight: validating /tmp/nyl runs in pod..."
set +e
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- /tmp/nyl --help >/tmp/${TEST_NAME}-preflight.log 2>&1
PREFLIGHT_RC=$?
set -e
if [ "${PREFLIGHT_RC}" -ne 0 ]; then
    echo "ERROR: /tmp/nyl is not runnable in runner pod."
    echo "This is usually a libc mismatch. Prefer a musl build, e.g.:"
    echo "  cargo build -p nyl --target x86_64-unknown-linux-musl"
    cat /tmp/${TEST_NAME}-preflight.log || true
    exit 1
fi
echo "✓ /tmp/nyl is runnable"
echo ""

echo "Creating ApplicationGenerator manifest inside pod..."
MANIFEST_TMP="$(mktemp)"
cat > "${MANIFEST_TMP}" <<EOF
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: cred-test
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: ${TEST_REPO_URL}
    targetRevision: HEAD
    path: ${TEST_SOURCE_PATH}
EOF
kubectl -n "${RUNNER_NAMESPACE}" exec -i "${POD_NAME}" -- sh -c "cat > '${MANIFEST_FILE}'" < "${MANIFEST_TMP}"
rm -f "${MANIFEST_TMP}"
echo "✓ Manifest created"
if ! kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- sh -c "test -s '${MANIFEST_FILE}'"; then
    echo "ERROR: Manifest file was not created or is empty: ${MANIFEST_FILE}"
    kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- sh -c "ls -la /tmp" || true
    exit 1
fi
if ! kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- sh -c \
    "grep -q '^apiVersion: argocd.nyl.niklasrosenstein.github.com/v1$' '${MANIFEST_FILE}' && grep -q '^kind: ApplicationGenerator$' '${MANIFEST_FILE}'"; then
    echo "ERROR: Generated manifest does not look like an ApplicationGenerator."
    kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- sh -c "sed -n '1,120p' '${MANIFEST_FILE}'" || true
    exit 1
fi
echo "✓ Manifest shape validated"
echo ""

echo "Case A: no ArgoCD repository secret (expected: failure)"
set +e
CASE_A_OUTPUT="$(
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- env \
    ARGOCD_APP_NAME=cred-test \
    ARGOCD_APP_NAMESPACE=argocd \
    RUST_LOG=nyl=trace,nyl::git=trace \
    /tmp/nyl render "${MANIFEST_FILE}" 2>&1
)"
CASE_A_RC=${PIPESTATUS[0]}
set -e
printf '%s\n' "${CASE_A_OUTPUT}"

CASE_A_OUTCOME="failed_without_secret"
if [ "${CASE_A_RC}" -eq 0 ]; then
    echo "⚠️  Case A succeeded without secret."
    echo "   This usually means ${TEST_REPO_URL} is publicly readable from the pod."
    CASE_A_OUTCOME="succeeded_without_secret"
    if [ "${REQUIRE_PRIVATE_REPO}" = "true" ]; then
        echo "ERROR: REQUIRE_PRIVATE_REPO=true but Case A succeeded."
        echo "Use a private repository URL for TEST_REPO_URL."
        exit 1
    fi
    echo "   Continuing because REQUIRE_PRIVATE_REPO=false."
else
    if ! printf '%s\n' "${CASE_A_OUTPUT}" | grep -E -q "No credentials|Authentication failed|Failed to discover ArgoCD credentials|git fetch failed"; then
        echo "ERROR: Case A failed, but not due to expected auth/credential path."
        echo "Log excerpt:"
        printf '%s\n' "${CASE_A_OUTPUT}" | sed -n '1,120p' || true
        exit 1
    fi
    echo "✓ Failed as expected without secret"
fi
echo ""

echo "Creating ArgoCD repository secret in ${ARGOCD_NAMESPACE}..."
kubectl -n "${ARGOCD_NAMESPACE}" create secret generic "${SECRET_NAME}" \
    --from-literal=url="${TEST_REPO_URL}" \
    --from-literal=username="${TEST_REPO_USERNAME}" \
    --from-literal=password="${TEST_REPO_PASSWORD}" \
    --from-literal=type=git \
    --dry-run=client -o yaml \
    | kubectl -n "${ARGOCD_NAMESPACE}" label --local -f - argocd.argoproj.io/secret-type=repository -o yaml \
    | kubectl apply -f - >/dev/null
echo "✓ Repository secret created"
echo ""

echo "Case B: ArgoCD repository secret present (expected: success)"
set +e
CASE_B_OUTPUT="$(
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- env \
    ARGOCD_APP_NAME=cred-test \
    ARGOCD_APP_NAMESPACE=argocd \
    RUST_LOG=nyl=trace,nyl::git=trace \
    /tmp/nyl render "${MANIFEST_FILE}" 2>&1
)"
CASE_B_RC=${PIPESTATUS[0]}
set -e
printf '%s\n' "${CASE_B_OUTPUT}"
if [ "${CASE_B_RC}" -ne 0 ]; then
    echo "ERROR: Expected success with secret, but command failed."
    exit 1
fi
echo "✓ Succeeded with secret"
echo ""

echo "Rendered manifests from Case B (RUST_LOG=error):"
RENDERED_OUTPUT="$(
kubectl -n "${RUNNER_NAMESPACE}" exec "${POD_NAME}" -- env \
    ARGOCD_APP_NAME=cred-test \
    ARGOCD_APP_NAMESPACE=argocd \
    RUST_LOG=error \
    /tmp/nyl render "${MANIFEST_FILE}" 2>/dev/null || true
)"
if [ -z "${RENDERED_OUTPUT}" ]; then
    echo "  (no manifests rendered)"
else
    echo "----- BEGIN RENDERED MANIFESTS -----"
    printf '%s\n' "${RENDERED_OUTPUT}"
    echo "----- END RENDERED MANIFESTS -----"
    KIND_COUNT="$(printf '%s\n' "${RENDERED_OUTPUT}" | grep -c '^kind:' || true)"
    echo "  Rendered resource count (by 'kind:' lines): ${KIND_COUNT}"
fi
echo ""

echo "==================================="
echo "✅ TEST PASSED: ${TEST_NAME}"
echo "==================================="
echo "Summary:"
echo "  - In-cluster execution path activated via ARGOCD_APP_* env vars"
if [ "${CASE_A_OUTCOME}" = "failed_without_secret" ]; then
    echo "  - Render failed without ArgoCD repository secret"
else
    echo "  - Render succeeded without secret (repo likely public)"
fi
echo "  - Render succeeded after creating repository secret"
echo ""
