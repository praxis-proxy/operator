# Architecture

The Praxis operator is a Kubernetes controller that
watches [Gateway API] resources and translates them
into running [Praxis] proxy instances.

[Gateway API]: https://gateway-api.sigs.k8s.io/
[Praxis]: https://github.com/praxis-proxy/praxis

## Overview

The operator follows the standard Kubernetes controller
pattern: watch resources, compare desired state against
actual state, and reconcile the difference. It uses the
[kube-rs] runtime for watch streams, leader election,
and event-driven reconciliation.

There are no custom CRDs. The operator works entirely
with standard Gateway API resources (GatewayClass,
Gateway, HTTPRoute, ReferenceGrant) and core Kubernetes
resources (Deployment, ConfigMap, Service, Secret).

[kube-rs]: https://kube.rs/

## Controllers

### GatewayClass Controller

Sets the `Accepted` condition on any GatewayClass
whose `controllerName` matches
`praxis.sh/gateway-controller`. Also declares
`supportedFeatures` (Gateway, HTTPRoute,
ReferenceGrant). Unrelated GatewayClasses are ignored.

### Gateway Controller

The primary reconciliation loop. This is where most of
the work happens:

1. Verify the Gateway's GatewayClass is accepted by
   this controller.
2. Reject Gateways with `parametersRef` (not
   supported).
3. List all HTTPRoutes across all namespaces.
4. Filter routes to those attached to this Gateway,
   checking namespace policies (Same, All, Selector)
   and ReferenceGrant authorization.
5. Generate a complete Praxis configuration from the
   Gateway's listeners and attached routes.
6. Apply child resources (ConfigMap, Deployment,
   Service) via server-side apply (SSA).
7. Update the Gateway's status conditions (Accepted,
   Programmed) and listener conditions (Accepted,
   Programmed, Conflicted, ResolvedRefs).

The controller uses a finalizer
(`gateway.praxis.sh/finalizer`) for lifecycle
management. Cleanup relies on owner references for
automatic garbage collection of child resources.

### HTTPRoute Controller

Validates route parent references and backend
references, then updates status:

- Checks that each parent ref points to a Gateway
  managed by this controller.
- Validates that backend Services exist.
- For cross-namespace backends, verifies a
  ReferenceGrant authorizes the reference.
- Sets `Accepted` and `ResolvedRefs` conditions per
  parent ref.

The HTTPRoute controller also triggers Gateway
reconciliation when routes change, using a watch
mapper that extracts the target Gateway from parent
refs.

## Child Resources

When a Gateway is reconciled, the operator creates
three child resources in the Gateway's namespace, all
named `praxis-{gateway-name}`:

### ConfigMap

Contains a single key `config.yaml` with the generated
Praxis proxy configuration. The config includes
listeners, routing rules, clusters (backend endpoints),
and filters.

### Deployment

A single-replica Deployment running the Praxis proxy
container. Key properties:

- **Image:** controlled by the `PRAXIS_IMAGE`
  environment variable on the operator
- **Config volume:** mounts the ConfigMap at
  `/etc/praxis/config.yaml`
- **TLS volumes:** mounts each referenced TLS Secret
  as a read-only volume
- **Health probes:** liveness (`/healthy`) and
  readiness (`/ready`) on admin port 9901
- **Security:** runs as UID 100, read-only filesystem,
  drops all capabilities except `NET_BIND_SERVICE`,
  RuntimeDefault seccomp profile
- **Config hash:** a `praxis.sh/config-hash` annotation
  on the pod template triggers a rolling update
  whenever the configuration changes

### Service

A LoadBalancer Service exposing the Gateway's listener
ports. The Gateway's status is updated with the
Service's assigned external addresses.

## Config Generation

The operator translates Gateway API resources into
Praxis's native YAML configuration:

| Gateway API Concept   | Praxis Config Element                          |
| --------------------- | ---------------------------------------------- |
| Listener (HTTP)       | Listener (no protocol field)                   |
| Listener (HTTPS)      | Listener with `protocol: http` and `tls` block |
| HTTPRoute rule        | Router filter route entry                      |
| PathPrefix match      | Route `path_prefix`                            |
| Exact match           | Route `path`                                   |
| Hostname match        | Route `domains`                                |
| Backend Service       | Cluster with resolved endpoint addresses       |
| Header modifier       | `request_headers` / `response_headers` filter  |
| Request redirect      | `redirect` filter                              |

The config always includes a `request_id` filter
(generates trace IDs) and a `router` filter (handles
routing decisions). Backend load balancing defaults to
round-robin.

## Labeling

All child resources carry standard Kubernetes labels:

| Label                              | Value                 |
| ---------------------------------- | --------------------- |
| `app.kubernetes.io/name`           | `praxis`              |
| `app.kubernetes.io/instance`       | `{gateway-name}`      |
| `app.kubernetes.io/managed-by`     | `praxis-operator`     |
