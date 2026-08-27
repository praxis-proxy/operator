# Praxis Kubernetes Operator

[Kubernetes] operator that manages [Praxis] proxy
instances via the [Gateway API].

> **Note**: Supports Gateway API `v1.5.1`

[Kubernetes]: https://kubernetes.io/
[Praxis]: https://github.com/praxis-proxy/praxis
[Gateway API]: https://gateway-api.sigs.k8s.io/

## Quick Start

```console
make build       # build
make test        # test
make lint        # clippy + fmt check
make container   # container image
```

See [Getting Started](docs/getting-started.md) for
deploying to a Kubernetes cluster.

## Deployment

```console
kubectl apply -f deploy/rbac.yaml
kubectl apply -f deploy/deployment.yaml
kubectl apply -f deploy/gatewayclass.yaml
```

## Documentation

- [Getting Started](docs/getting-started.md):
  deploy and route traffic in minutes
- [Architecture](docs/architecture.md):
  how the operator works
- [Gateway API Support](docs/gateway-api-support.md):
  feature matrix and conformance status
- [Configuration](docs/configuration.md):
  environment variables, RBAC, resources
- [Troubleshooting](docs/troubleshooting.md):
  common problems and debugging
- [Development](docs/development.md):
  building, testing, contributing
- [Conventions](docs/conventions.md):
  coding standards

## Contributing

[Issues] and [pull requests] are welcome. Familiarize
yourself with the following documentation first:

- [Architecture](docs/architecture.md)
- [Conventions](docs/conventions.md)
- [Development](docs/development.md)

For larger changes, open a [discussion] and follow
the [proposal process](docs/proposals.md).

[Issues]: https://github.com/praxis-proxy/operator/issues/new
[pull requests]: https://github.com/praxis-proxy/operator/compare
[discussion]: https://github.com/praxis-proxy/operator/discussions
