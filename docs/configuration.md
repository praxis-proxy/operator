# Configuration

The operator is configured through environment
variables on its Deployment and the RBAC manifests in
`deploy/`. There are no custom CRDs or configuration
files for the operator itself.

## Environment Variables

### `PRAXIS_IMAGE`

Container image used for data-plane pods. Set on the
operator Deployment.

- **Default:** `ghcr.io/praxis-proxy/praxis:latest`
- **Deploy manifest:** `ghcr.io/praxis-proxy/praxis:0.3.0`

```yaml
env:
  - name: PRAXIS_IMAGE
    value: "ghcr.io/praxis-proxy/praxis:0.3.0"
```

### `RUST_LOG`

Controls operator log verbosity using the standard
[`tracing`] env filter syntax.

- **Default:** `praxis_operator=info`
- **Debug:** `RUST_LOG=praxis_operator=debug`
- **Trace:** `RUST_LOG=praxis_operator=trace`

[`tracing`]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html

## RBAC

The operator requires a ClusterRole with these
permissions:

### Gateway API Resources

| Resource                | Verbs                |
|-------------------------|----------------------|
| gatewayclasses          | get, list, watch     |
| gatewayclasses/status   | get, patch, update   |
| gateways                | get, list, watch, update, patch |
| gateways/status         | get, patch, update   |
| gateways/finalizers     | update               |
| httproutes              | get, list, watch     |
| httproutes/status       | get, patch, update   |
| referencegrants         | get, list, watch     |

### Core Resources

| Resource    | Verbs                                     |
|-------------|-------------------------------------------|
| namespaces  | get, list, watch                          |
| services    | get, list, watch, create, update, patch, delete |
| endpoints   | get, list, watch, create, update, patch, delete |
| configmaps  | get, list, watch, create, update, patch, delete |
| secrets     | get, list, watch, create, update, patch, delete |
| events      | create, patch                             |

### Discovery Resources

| Resource       | Verbs              |
|----------------|--------------------|
| endpointslices | get, list, watch   |

### Apps Resources

| Resource    | Verbs                                     |
|-------------|-------------------------------------------|
| deployments | get, list, watch, create, update, patch, delete |

### Coordination

| Resource | Verbs                                     |
|----------|-------------------------------------------|
| leases   | get, list, watch, create, update, patch, delete |

## Resource Requirements

### Operator

| Resource | Request | Limit |
|----------|---------|-------|
| CPU      | 50m     | 200m  |
| Memory   | 64Mi    | 128Mi |

### Data-Plane Pods

| Resource | Request | Limit |
|----------|---------|-------|
| CPU      | 100m    | none  |
| Memory   | 64Mi    | 256Mi |

These are defaults in the deploy manifests and
resource builders. Adjust for your workload.

## Security

### Operator Pod

- Runs as UID 65534 (nobody)
- Read-only root filesystem
- No privilege escalation
- Non-root enforced

### Data-Plane Pods

- Runs as UID 100
- Read-only root filesystem
- Drops all capabilities except `NET_BIND_SERVICE`
- RuntimeDefault seccomp profile
- `automountServiceAccountToken: false`
- No privilege escalation

### Codebase

- `#![deny(unsafe_code)]` in all crate roots
- Clippy with `-D warnings`
- `cargo audit` and `cargo deny check` in CI

## GatewayClass

The default GatewayClass is named `praxis`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: praxis
spec:
  controllerName: praxis.sh/gateway-controller
```

The operator accepts any GatewayClass with controller
name `praxis.sh/gateway-controller`. You can create
multiple GatewayClasses with this controller name;
each will be accepted.

Declared supported features:
- `Gateway`
- `HTTPRoute`
- `ReferenceGrant`

## Data-Plane Configuration

The operator generates the complete Praxis
configuration. Users do not write Praxis config
directly.

- **ConfigMap:** `praxis-{gateway-name}` with key
  `config.yaml`
- **Admin port:** 9901 (health probes, diagnostics)
- **Config changes:** a SHA-256 hash annotation
  (`praxis.sh/config-hash`) on the pod template
  triggers a rolling update when config changes

See [Architecture](architecture.md) for details on
how Gateway API resources are translated to Praxis
configuration.
