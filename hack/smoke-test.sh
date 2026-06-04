#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-conformance}"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"
NAMESPACE="smoke-test"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

echo "==> Creating test namespace..."
${KUBECTL} create namespace "${NAMESPACE}" \
    --dry-run=client -o yaml | ${KUBECTL} apply -f -

echo "==> Deploying echo backend..."
${KUBECTL} -n "${NAMESPACE}" apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: echo
spec:
  replicas: 1
  selector:
    matchLabels:
      app: echo
  template:
    metadata:
      labels:
        app: echo
    spec:
      containers:
        - name: echo
          image: hashicorp/http-echo
          args: ["-text=hello from praxis"]
          ports:
            - containerPort: 5678
---
apiVersion: v1
kind: Service
metadata:
  name: echo
spec:
  selector:
    app: echo
  ports:
    - port: 5678
      targetPort: 5678
EOF

${KUBECTL} -n "${NAMESPACE}" rollout status \
    deployment/echo --timeout=60s

echo "==> Creating Gateway..."
${KUBECTL} -n "${NAMESPACE}" apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: smoke
spec:
  gatewayClassName: praxis
  listeners:
    - name: http
      port: 8080
      protocol: HTTP
EOF

echo "==> Creating HTTPRoute..."
${KUBECTL} -n "${NAMESPACE}" apply -f - <<'EOF'
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: echo-route
spec:
  parentRefs:
    - name: smoke
  rules:
    - backendRefs:
        - name: echo
          port: 5678
EOF

# ---------------------------------------------------------------------------
# Wait for Gateway
# ---------------------------------------------------------------------------

echo "==> Waiting for Gateway to be programmed..."
for i in $(seq 1 60); do
    STATUS=$(${KUBECTL} -n "${NAMESPACE}" get gateway smoke \
        -o jsonpath='{.status.conditions[?(@.type=="Programmed")].status}' \
        2>/dev/null || true)
    if [ "${STATUS}" = "True" ]; then
        echo "    Gateway programmed after ${i}s"
        break
    fi
    if [ "${i}" -eq 60 ]; then
        echo "FAIL: Gateway not programmed after 60s"
        ${KUBECTL} -n "${NAMESPACE}" describe gateway smoke
        exit 1
    fi
    sleep 1
done

# ---------------------------------------------------------------------------
# Traffic Test
# ---------------------------------------------------------------------------

echo "==> Getting Gateway address..."
GW_IP=$(${KUBECTL} -n "${NAMESPACE}" get gateway smoke \
    -o jsonpath='{.status.addresses[0].value}' 2>/dev/null || true)

if [ -z "${GW_IP}" ]; then
    echo "FAIL: no Gateway address assigned"
    ${KUBECTL} -n "${NAMESPACE}" describe gateway smoke
    ${KUBECTL} -n "${NAMESPACE}" get svc
    exit 1
fi

echo "==> Sending traffic to ${GW_IP}:8080..."
RESPONSE=$(curl -s --max-time 10 \
    "http://${GW_IP}:8080/" || true)

echo "    Response: ${RESPONSE}"

if echo "${RESPONSE}" | grep -q "hello"; then
    echo "PASS: traffic routed through Praxis"
else
    echo "FAIL: unexpected response"
    echo "==> Debug info:"
    ${KUBECTL} -n "${NAMESPACE}" get pods -o wide
    ${KUBECTL} -n "${NAMESPACE}" logs deployment/praxis-smoke \
        -c praxis --tail=50 2>/dev/null || true
    exit 1
fi

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

echo "==> Cleanup..."
${KUBECTL} delete namespace "${NAMESPACE}"
echo "==> Smoke test passed."
