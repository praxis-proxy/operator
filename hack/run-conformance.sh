#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-conformance}"
GWAPI_VERSION="${GWAPI_VERSION:-v1.5.1}"
GWAPI_CONFORMANCE_TAG="${GWAPI_CONFORMANCE_TAG:-monthly-2026.05}"
GWAPI_DIR="/tmp/gateway-api"
GATEWAY_CLASS="${GATEWAY_CLASS:-praxis}"
MAX_CONSISTENCY="${MAX_CONSISTENCY:-120}"
NS_READY="${NS_READY:-600}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_FILTER="${SCRIPT_DIR}/filter-conformance-logs.sh"

# Unfiltered suite output. The log filter exists to keep the console
# readable, which means it is not a place to look for the failure list.
RAW_LOG="${RAW_LOG:-/tmp/conformance-raw.log}"

# ---------------------------------------------------------------------------
# Isolated KUBECONFIG
# ---------------------------------------------------------------------------

KUBECONFIG_FILE="/tmp/kind-${CLUSTER_NAME}.kubeconfig"
kind get kubeconfig --name "${CLUSTER_NAME}" > "${KUBECONFIG_FILE}"
export KUBECONFIG="${KUBECONFIG_FILE}"
trap "rm -f ${KUBECONFIG_FILE}" EXIT

# ---------------------------------------------------------------------------
# Gateway API Source
# ---------------------------------------------------------------------------

if [ ! -d "${GWAPI_DIR}" ]; then
    echo "==> Cloning gateway-api ${GWAPI_CONFORMANCE_TAG}..."
    # Retried, and the directory is removed first: a clone that dies partway
    # leaves one behind, and the next attempt would fail on it rather than on
    # whatever went wrong.
    "${SCRIPT_DIR}/retry.sh" bash -c \
        "rm -rf '${GWAPI_DIR}' && git clone --depth 1 --branch '${GWAPI_CONFORMANCE_TAG}' \
            https://github.com/kubernetes-sigs/gateway-api.git '${GWAPI_DIR}'"
else
    echo "==> Using cached gateway-api at ${GWAPI_DIR}"
fi

# ---------------------------------------------------------------------------
# Run Tests
# ---------------------------------------------------------------------------

echo "==> Running conformance tests (context: kind-${CLUSTER_NAME})..."
cd "${GWAPI_DIR}"
# A failing suite is an expected outcome here, not a reason to abort: the
# summary below is the whole point of running it. Errexit goes back on
# once the status is captured, and the script exits with it at the end.
set +e
# The suite needs headroom: it already ran ~17 minutes against the old 20m
# ceiling, so any CI slowdown aborted it with no report written. Raised to
# 45m so a genuine hang is still caught while normal variance is not.
go test ./conformance -run TestConformance \
    -timeout 45m -v \
    -args \
    --gateway-class="${GATEWAY_CLASS}" \
    --conformance-profiles=GATEWAY-HTTP \
    --timeout-config-overrides="MaxTimeToConsistency:${MAX_CONSISTENCY};NamespacesMustBeReady:${NS_READY}" \
    --allow-crds-mismatch \
    --report-output=/tmp/conformance-report.yaml \
    --organization=praxis-proxy \
    --project=praxis-operator \
    --version=v0.1.0 \
    --url=https://github.com/praxis-proxy/praxis-operator \
    --contact=@shaneutt \
    2>&1 | tee "${RAW_LOG}" | "${LOG_FILTER}"
status="${PIPESTATUS[0]}"
set -e

# ---------------------------------------------------------------------------
# Failure Summary
# ---------------------------------------------------------------------------

# The suite prints thousands of lines and the job then appends a cluster
# dump, so on CI the names of the tests that actually failed end up
# buried far from either end of the log. Restate them last, where
# anyone reading the tail of a failed job will see them without
# downloading the whole thing.
if [ "${status}" -ne 0 ]; then
    echo
    echo "==> FAILED TESTS"
    # Collected into a variable rather than tested through a pipeline:
    # the exit status of `grep | sed | sort` is sort's, which succeeds on
    # empty input, so piping would silently never take the fallback.
    failures="$(grep -E '^[[:space:]]*--- FAIL: ' "${RAW_LOG}" || true)"
    if [ -n "${failures}" ]; then
        printf '%s\n' "${failures}" | sed 's/^[[:space:]]*/    /' | sort -u
    else
        echo "    (no '--- FAIL' lines; the suite failed before running tests)"
        echo "==> LAST 40 LINES OF RAW OUTPUT"
        tail -n 40 "${RAW_LOG}" | sed 's/^/    /'
    fi
    echo
fi

if [ -f /tmp/conformance-report.yaml ]; then
    echo "==> Conformance report: /tmp/conformance-report.yaml"
    sed 's/^/    /' /tmp/conformance-report.yaml
fi

exit "${status}"
