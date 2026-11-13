# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog], and this project adheres to
[Semantic Versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html

## [Unreleased]

### Added

- Opt-in PodMonitor generation and metrics ingress policy with ownership-checked
  cleanup when monitoring is disabled or no usable listeners remain.

- Health, readiness and metrics endpoints on the reserved admin port 9902,
  with matching probes on the operator Deployment.
- Leader election over a coordination `Lease`, so the operator can run
  more than one replica safely.
- Kubernetes events explaining why a Gateway was rejected.
- Configurable data-plane replicas via the `praxis.sh/replicas`
  annotation, a `PodDisruptionBudget`, and pod spread across nodes.
- `method` and `queryParams` route matches are now detected and
  reported rather than silently ignored.
- Listener protocol and hostname conflict detection.

### Fixed

- Gateway listeners on the reserved health/metrics port 9902 now report
  `PortUnavailable` without blocking other valid listeners.
- Generated data-plane configuration now targets Praxis 0.7.5 so health and
  metrics bind to the dedicated admin endpoint.
- Gateway listeners without routes now answer HTTP 404 with a static response
  instead of generating an empty router chain.
- Redirect and invalid-reference rules keep answering on a listener whose
  routes produce no router entries: their terminating filters run ahead of
  the routeless 404 fallback, so a redirect still redirects and an
  unresolvable reference still answers 500 instead of 404.
- Terminating filters now run only on the listeners their routes target, so
  a redirect or invalid-reference rule on one listener no longer answers
  another listener's traffic or overrides its routeless 404 fallback.
- Backend references with weight 0 contribute no endpoints, duplicate
  endpoint addresses are merged with summed weights, and per-endpoint
  weights scale into the data plane's 1..=1000 range, matching the proxy's
  cluster validation instead of crashing the data plane on rollout.

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
- A backend Service with no ready endpoints no longer blocks its whole
  Gateway: the Gateway reconciles, and routes naming only that Service
  answer `500 no backends available` until pods appear.
- Route parent status is cleared when its Gateway is deleted.
- Redirect statuses outside the set Praxis accepts no longer produce a
  config the data plane refuses to load.

### Security

- RBAC no longer grants write access to Secrets and Endpoints.
- The operator pod is hardened to match the data-plane pods it creates.
