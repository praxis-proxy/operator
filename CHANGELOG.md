# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog], and this project adheres to
[Semantic Versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html

## [Unreleased]

### Added

- A five-second preStop delay for data-plane pods so Service endpoints can
  be removed before the proxy stops accepting connections.

- Health, readiness and metrics endpoints on port 8080, with matching
  probes on the operator Deployment.
- Leader election over a coordination `Lease`, so the operator can run
  more than one replica safely.
- Kubernetes events explaining why a Gateway was rejected.
- Configurable positive data-plane replica counts via the
  `praxis.sh/replicas` annotation, a `PodDisruptionBudget`, zero-unavailable
  rolling updates, and zone and hostname pod spread.
- Kubernetes reference and rollout-test assets, plus the GitHub Actions
  workflow that runs the Praxis rollout acceptance suite against them.
- `method` and `queryParams` route matches are now detected and
  reported rather than silently ignored.
- Listener protocol and hostname conflict detection.

### Fixed

- Return HTTP 404 for unmatched requests on Gateway listeners without routes.

- Remove stale proxy children when no usable Gateway listeners remain, including
  conflict-only configurations, and refresh HTTPRoute parent rejection statuses
  after listeners move to or from the reserved metrics port.

- Gateway listeners on the reserved health/metrics port 9902 now report
  `PortUnavailable` without blocking other valid listeners.
- Generated data-plane configuration now targets Praxis 0.7.5, including its
  metrics-only admin endpoint and routeless-listener support.
- Status writes no longer restamp unchanged conditions, which had kept
  the operator in a permanent reconcile loop.
- Endpoint weight distribution no longer overflows `i32` and aborts the
  process.
- The Gateway and HTTPRoute controllers no longer overwrite each other's
  entries in `status.parents`.
- Unsupported route matchers and filters are rejected instead of being
  widened into something the author did not ask for.
- Hostname matching is case-insensitive, per RFC 1123.
- Named `targetPort`s resolve by port name instead of picking an
  arbitrary port on multi-port Services.
- Terminating endpoints are excluded from the data-plane config.
- Route parent status is cleared when its Gateway is deleted.
- Redirect statuses outside the set Praxis accepts no longer produce a
  config the data plane refuses to load.

### Security

- RBAC no longer grants write access to Secrets and Endpoints.
- The operator pod is hardened to match the data-plane pods it creates.
