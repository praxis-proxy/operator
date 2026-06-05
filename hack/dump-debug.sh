#!/usr/bin/env bash
set -euo pipefail

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-conformance}"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"

echo ''
echo '==> DEBUG: Operator pod status'
${KUBECTL} -n praxis-system get pods -o wide 2>/dev/null || true
echo ''
echo '==> DEBUG: Operator pod describe'
${KUBECTL} -n praxis-system describe pods \
    -l app.kubernetes.io/name=praxis-operator 2>/dev/null || true
echo ''
echo '==> DEBUG: Operator logs'
${KUBECTL} -n praxis-system logs deployment/praxis-operator \
    --tail=100 2>/dev/null || true
echo ''
echo '==> DEBUG: All pods (all namespaces)'
${KUBECTL} get pods -A -o wide 2>/dev/null || true
echo ''
echo '==> DEBUG: Praxis proxy pods (all namespaces)'
for ns in $(${KUBECTL} get ns -o jsonpath='{.items[*].metadata.name}' \
    2>/dev/null); do
    for pod in $(${KUBECTL} -n "${ns}" get pods \
        -l app.kubernetes.io/name=praxis -o name 2>/dev/null); do
        echo ""
        echo "==> DEBUG: ${ns}/${pod} describe"
        ${KUBECTL} -n "${ns}" describe "${pod}" 2>/dev/null || true
        echo ""
        echo "==> DEBUG: ${ns}/${pod} logs"
        ${KUBECTL} -n "${ns}" logs "${pod}" -c praxis \
            --tail=100 2>/dev/null || true
    done
done
echo ''
echo '==> DEBUG: Events (all namespaces)'
${KUBECTL} get events -A --sort-by=.lastTimestamp 2>/dev/null \
    | tail -50 || true
