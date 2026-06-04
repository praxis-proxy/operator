#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-conformance}"
PRAXIS_IMAGE="${PRAXIS_IMAGE:-praxis:dev}"
OPERATOR_IMAGE="${OPERATOR_IMAGE:-praxis-operator:dev}"
CONTAINER_ENGINE="${CONTAINER_ENGINE:-docker}"
GWAPI_VERSION="v1.5.1"
METALLB_VERSION="v0.14.9"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"

# ---------------------------------------------------------------------------
# KIND Cluster
# ---------------------------------------------------------------------------

create_cluster() {
    if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
        echo "==> Cluster '${CLUSTER_NAME}' already exists, reusing."
    else
        echo "==> Creating KIND cluster '${CLUSTER_NAME}'..."
        kind create cluster \
            --name "${CLUSTER_NAME}" \
            --config "${SCRIPT_DIR}/kind-config.yaml" \
            --wait 60s
    fi
}

# ---------------------------------------------------------------------------
# Gateway API CRDs
# ---------------------------------------------------------------------------

install_gateway_api() {
    if ${KUBECTL} get crd gateways.gateway.networking.k8s.io \
        &>/dev/null; then
        echo "==> Gateway API CRDs already installed, skipping."
        return
    fi
    echo "==> Installing Gateway API CRDs ${GWAPI_VERSION}..."
    ${KUBECTL} apply -f \
        "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GWAPI_VERSION}/standard-install.yaml"
}

# ---------------------------------------------------------------------------
# MetalLB
# ---------------------------------------------------------------------------

install_metallb() {
    if ${KUBECTL} -n metallb-system get deployment controller \
        &>/dev/null; then
        echo "==> MetalLB already installed, skipping."
        return
    fi
    echo "==> Installing MetalLB ${METALLB_VERSION}..."
    ${KUBECTL} apply -f \
        "https://raw.githubusercontent.com/metallb/metallb/${METALLB_VERSION}/config/manifests/metallb-native.yaml"
    ${KUBECTL} wait --namespace metallb-system \
        --for=condition=ready pod \
        --selector=app=metallb \
        --timeout=300s
}

configure_metallb_pool() {
    if ${KUBECTL} -n metallb-system get ipaddresspool kind-pool \
        &>/dev/null; then
        echo "==> MetalLB IP pool already configured, skipping."
        return
    fi
    echo "==> Configuring MetalLB IP pool..."
    SUBNET=$(${CONTAINER_ENGINE} network inspect kind \
        -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' \
        | tr ' ' '\n' | grep '\.' | head -1)
    IFS='.' read -r a b c d <<< "${SUBNET%%/*}"
    cat <<EOF | ${KUBECTL} apply -f -
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: kind-pool
  namespace: metallb-system
spec:
  addresses:
    - ${a}.${b}.255.200-${a}.${b}.255.210
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: l2
  namespace: metallb-system
EOF
}

# ---------------------------------------------------------------------------
# Container Images
# ---------------------------------------------------------------------------

load_images() {
    echo "==> Loading container images..."
    kind load docker-image "${OPERATOR_IMAGE}" --name "${CLUSTER_NAME}"
    kind load docker-image "${PRAXIS_IMAGE}" --name "${CLUSTER_NAME}"
}

# ---------------------------------------------------------------------------
# Operator Deployment
# ---------------------------------------------------------------------------

deploy_operator() {
    echo "==> Deploying operator..."
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/rbac.yaml"
    sed "s|__PRAXIS_IMAGE__|${PRAXIS_IMAGE}|g; s|__OPERATOR_IMAGE__|${OPERATOR_IMAGE}|g" \
        "${ROOT_DIR}/deploy/deployment.yaml" | ${KUBECTL} apply -f -
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/gatewayclass.yaml"

    echo "==> Waiting for operator..."
    ${KUBECTL} -n praxis-system rollout status \
        deployment/praxis-operator --timeout=120s
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

for cmd in kind kubectl "${CONTAINER_ENGINE}"; do
    if ! command -v "${cmd}" &>/dev/null; then
        echo "ERROR: ${cmd} is required but not found"
        exit 1
    fi
done

trap 'bash "${SCRIPT_DIR}/dump-debug.sh"' ERR

create_cluster
install_gateway_api
install_metallb
configure_metallb_pool
load_images
deploy_operator

echo "==> KIND cluster ready."
echo ""
echo "    Cluster:  ${CLUSTER_NAME}"
echo "    Context:  kind-${CLUSTER_NAME}"
echo ""
echo "    Run conformance:  make dev-conformance"
echo "    Run integration:  make dev-integration"
echo "    Update operator:  make dev-push"
echo "    Full dev cycle:   make dev-cycle"
