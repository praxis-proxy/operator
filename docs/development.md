# Development

Guide for building, testing, and contributing to the
Praxis operator.

## Prerequisites

- Rust stable 1.94+ (edition 2024)
- Rust nightly (for `rustfmt`; `group_imports` and
  `imports_granularity` are nightly-only)
- Docker or Podman (container builds)
- [KIND] (local Kubernetes clusters)
- [Go] 1.24+ (conformance tests only)

[KIND]: https://kind.sigs.k8s.io/
[Go]: https://go.dev/

## Build Commands

```console
make build       # cargo build --workspace
make release     # cargo build --workspace --release
make test        # cargo test --workspace
make lint        # clippy + nightly fmt check
make fmt         # cargo +nightly fmt
make doc         # rustdoc
make audit       # cargo audit + cargo deny check
make container   # build container image
```

Run a single test:

```console
cargo test -p praxis-operator -- test_name
```

## Project Structure

```
src/
  main.rs              # Entry point, controller startup
  context.rs           # Shared state, constants
  error.rs             # Error types (thiserror)
  endpoints.rs         # Service endpoint resolution
  controller/
    gateway.rs         # Gateway reconciler (primary)
    gateway_class.rs   # GatewayClass reconciler
    gateway_helpers.rs # Config generation, child resources
    httproute.rs       # HTTPRoute reconciler
  gateway_api/
    attachment.rs      # Route-to-Gateway attachment
    conditions.rs      # Status condition builders
    hostname.rs        # Hostname intersection logic
    reference_grant.rs # Cross-namespace authorization
    validation.rs      # Listener validation
  config/
    cluster.rs         # Backend cluster config
    filter_conversion.rs # HTTPRoute filter translation
    generate.rs        # Full config assembly
    listener.rs        # Listener config
    routing.rs         # Route config
  resources/
    configmap.rs       # ConfigMap builder
    deployment.rs      # Deployment builder
    labels.rs          # Standard labels, owner refs
    service.rs         # Service builder
```

## Local Development with KIND

Set up a full local environment:

```console
make kind-up
```

This creates a KIND cluster with Gateway API CRDs,
MetalLB, and the operator deployed. The cluster is
named `praxis-conformance` by default.

Run the end-to-end smoke test:

```console
make smoke-test
```

Run Gateway API conformance tests:

```console
make conformance
```

Tear down:

```console
make kind-down
```

### Environment Variables

| Variable            | Default                         |
|---------------------|---------------------------------|
| `KIND_CLUSTER_NAME` | `praxis-conformance`            |
| `PRAXIS_IMAGE`      | `ghcr.io/praxis-proxy/praxis:0.3.0` |
| `OPERATOR_IMAGE`    | `praxis-operator:dev`           |

## Testing

### Unit Tests

```console
make test
```

87 unit tests covering config generation, route
attachment, hostname matching, condition builders,
reference grant validation, and resource builders.

### Integration Tests

Require a running cluster with the operator deployed:

```console
make test-integration
```

Gated behind the `integration` feature flag. Tests
verify GatewayClass acceptance in a real cluster.

### Conformance Tests

Gateway API GATEWAY-HTTP core profile:

```console
make conformance
```

Current status: 7 of 33 tests passing. See
[Gateway API Support](gateway-api-support.md) for
details. Report output: `/tmp/conformance-report.yaml`.

## Project Management

All repositories in the `praxis-proxy` organization
use a consistent workflow for planning, prioritizing,
and tracking work.

### Milestones

Milestones represent a body of work toward a shared
goal (e.g. a release, a feature area, or a hardening
pass). Every issue and pull request should belong to
a milestone. Milestones provide scope boundaries and
help answer "what ships together?"

### Priority Labels

Priority labels indicate the order in which work
within a milestone should be addressed. Every issue
should have exactly one priority label:

| Label | Description |
| --- | --- |
| `priority/critical` | Must be worked on immediately before anything else |
| `priority/high` | Needs to be worked on immediately, defer to criticals |
| `priority/medium` | Resolve after high and critical |
| `priority/low` | Resolve after all other priority levels |

When picking up work, address issues in priority
order: critical first, then high, medium, and low.

### Project Boards

GitHub project boards visualize the state of work
across milestones. Use boards to track issues through
their lifecycle (backlog, in progress, in review,
done). Boards are the primary tool for stand-ups and
status checks.

## Coding Conventions

See [conventions.md](conventions.md) for the full
coding standards, including file ordering, test
conventions, separator comment format, and documentation
requirements.
