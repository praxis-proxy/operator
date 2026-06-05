#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-conformance}"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"

# Namespaces that belong to the cluster infrastructure.
SYSTEM_NS="default|kube-node-lease|kube-public|kube-system|local-path-storage|metallb-system|praxis-system"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

echo "==> Cleaning conformance test resources..."

for gc in $(${KUBECTL} get gatewayclasses \
    -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
    if [ "${gc}" != "praxis" ]; then
        echo "    Deleting GatewayClass: ${gc}"
        ${KUBECTL} delete gatewayclass "${gc}" 2>/dev/null || true
    fi
done

for ns in $(${KUBECTL} get ns -o jsonpath='{.items[*].metadata.name}' \
    2>/dev/null); do
    if echo "${ns}" | grep -qE "^(${SYSTEM_NS})$"; then
        continue
    fi
    echo "    Deleting namespace: ${ns}"
    ${KUBECTL} delete namespace "${ns}" --wait=false 2>/dev/null || true
done

# ---------------------------------------------------------------------------
# Wait
# ---------------------------------------------------------------------------

echo "==> Waiting for namespace cleanup..."
for i in $(seq 1 60); do
    REMAINING=$(${KUBECTL} get ns --no-headers 2>/dev/null \
        | awk '{print $1}' \
        | grep -vcE "^(${SYSTEM_NS})$" || true)
    if [ "${REMAINING}" -eq 0 ]; then
        echo "    All test namespaces cleaned up."
        break
    fi
    if [ "${i}" -eq 60 ]; then
        echo "WARNING: ${REMAINING} namespace(s) still terminating after 60s."
    fi
    sleep 1
done

echo "==> Cluster reset complete. Ready for another test run."
