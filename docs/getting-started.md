# Getting Started

This guide walks through deploying Praxis on a
Kubernetes cluster and routing HTTP traffic through it.

## Prerequisites

- Kubernetes 1.32+
- kubectl configured for your cluster
- Gateway API CRDs installed (v1.5.1)
- A LoadBalancer provider (MetalLB for bare-metal/KIND,
  or a cloud provider)

Install the Gateway API CRDs if you haven't already:

```console
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.5.1/standard-install.yaml
```

## Install the Operator

Apply the operator manifests:

```console
kubectl apply -f https://raw.githubusercontent.com/praxis-proxy/praxis-operator/main/deploy/rbac.yaml
kubectl apply -f https://raw.githubusercontent.com/praxis-proxy/praxis-operator/main/deploy/deployment.yaml
kubectl apply -f https://raw.githubusercontent.com/praxis-proxy/praxis-operator/main/deploy/gatewayclass.yaml
```

This creates:

- A `praxis-system` namespace with the operator
  Deployment and RBAC
- A `praxis` GatewayClass registered with controller
  name `praxis.sh/gateway-controller`

Verify the operator is running:

```console
kubectl -n praxis-system rollout status \
    deployment/praxis-operator
```

Verify the GatewayClass is accepted:

```console
kubectl get gatewayclass praxis
```

The `ACCEPTED` column should show `True`.

## Deploy a Backend

Create a namespace and a simple HTTP echo service:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: demo
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: echo
  namespace: demo
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
  namespace: demo
spec:
  selector:
    app: echo
  ports:
    - port: 5678
      targetPort: 5678
```

```console
kubectl apply -f backend.yaml
kubectl -n demo rollout status deployment/echo
```

## Create a Gateway

Create a Gateway with an HTTP listener on port 8080:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: my-gateway
  namespace: demo
spec:
  gatewayClassName: praxis
  listeners:
    - name: http
      port: 8080
      protocol: HTTP
```

```console
kubectl apply -f gateway.yaml
```

Wait for the Gateway to become programmed:

```console
kubectl -n demo get gateway my-gateway
```

The `PROGRAMMED` column should show `True`. The
operator creates a Deployment, ConfigMap, and
LoadBalancer Service named `praxis-my-gateway` in the
`demo` namespace.

## Create an HTTPRoute

Route traffic from the Gateway to the echo backend:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: echo-route
  namespace: demo
spec:
  parentRefs:
    - name: my-gateway
  rules:
    - backendRefs:
        - name: echo
          port: 5678
```

```console
kubectl apply -f httproute.yaml
```

Verify the route is accepted:

```console
kubectl -n demo get httproute echo-route
```

## Send Traffic

Get the Gateway's assigned address:

```console
GW_IP=$(kubectl -n demo get gateway my-gateway \
    -o jsonpath='{.status.addresses[0].value}')
echo "${GW_IP}"
```

Send a request:

```console
curl http://${GW_IP}:8080/
```

Expected output:

```console
hello from praxis
```

## Clean Up

```console
kubectl delete namespace demo
```

The Gateway's child resources (Deployment, ConfigMap,
Service) are automatically deleted via owner references.

## Next Steps

- [Architecture](architecture.md): how the operator
  works internally
- [Gateway API Support](gateway-api-support.md):
  supported features and conformance status
- [Configuration](configuration.md): environment
  variables, RBAC, and resource tuning
- [Troubleshooting](troubleshooting.md): common
  problems and debugging
