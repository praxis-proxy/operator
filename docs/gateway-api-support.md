# Gateway API Support

Feature matrix for the Praxis operator against
[Gateway API] `v1.5.1`.

Legend:

- **Supported**: implemented and covered by tests.
- **Partial**: implemented with the stated limitation.
- **Not supported**: the field is ignored or the
  resource is not reconciled. Where the operator
  silently ignores a field, that is called out as a
  known gap.

[Gateway API]: https://gateway-api.sigs.k8s.io/

## Resources

| Resource | Status | Notes |
|---|---|---|
| `GatewayClass` | Supported | Accepted when `controllerName` is `praxis.sh/gateway-controller` |
| `Gateway` | Supported | One data-plane Deployment, ConfigMap and Service per Gateway |
| `HTTPRoute` | Supported | See the HTTPRoute sections below |
| `ReferenceGrant` | Supported | Gateway-to-Secret and HTTPRoute-to-Service |
| `GRPCRoute` | Not supported | |
| `TCPRoute` / `UDPRoute` / `TLSRoute` | Not supported | |
| `BackendTLSPolicy` | Not supported | |
| `ListenerSet` | Not supported | |

## GatewayClass

| Field | Status | Notes |
|---|---|---|
| `spec.controllerName` | Supported | Must match `praxis.sh/gateway-controller` |
| `spec.parametersRef` | Not supported | Ignored |
| `status.supportedFeatures` | Partial | Advertises a fixed `Gateway` / `HTTPRoute` list rather than the real feature set |

## Gateway

| Field | Status | Notes |
|---|---|---|
| `spec.gatewayClassName` | Supported | Gateways referencing another controller's class are skipped |
| `spec.listeners[].port` | Supported | Listeners sharing a port are merged into one data-plane listener |
| `spec.listeners[].protocol` | Partial | `HTTP` and `HTTPS` only; other protocols get `Accepted: False` / `UnsupportedProtocol` |
| `spec.listeners[].hostname` | Supported | Exact and `*.suffix` wildcards |
| `spec.listeners[].tls.mode` | Partial | `Terminate` only; `Passthrough` is not implemented |
| `spec.listeners[].tls.certificateRefs` | Supported | `core/Secret` of type `kubernetes.io/tls`; contents are validated as PEM |
| `spec.listeners[].allowedRoutes.namespaces` | Supported | `Same`, `All` and `Selector` (`matchLabels` and `matchExpressions`) |
| `spec.listeners[].allowedRoutes.kinds` | Partial | Only `HTTPRoute`; anything else yields `ResolvedRefs: False` / `InvalidRouteKinds` |
| `spec.addresses` | Not supported | **Known gap**: silently ignored instead of `Accepted: False` / `UnsupportedAddress` |
| `spec.infrastructure.parametersRef` | Not supported | Rejected with `Accepted: False` / `InvalidParameters` |

### Gateway status

| Condition | Status | Notes |
|---|---|---|
| `Accepted` | Supported | `False` / `ListenersNotValid` when no listener has a supported protocol |
| `Programmed` | Supported | Requires a ready Deployment **and** at least one load-balancer address |
| `status.addresses` | Supported | Load-balancer ingress IPs of the child Service |
| `status.listeners[].attachedRoutes` | Supported | |
| `status.listeners[].conditions` | Supported | `Accepted`, `Programmed`, `ResolvedRefs`, `Conflicted` |
| `Conflicted` | Partial | **Known gap**: always reported `False`; listener conflicts are not detected |

## HTTPRoute

| Field | Status | Notes |
|---|---|---|
| `spec.parentRefs` | Supported | `Gateway` parents only |
| `spec.parentRefs[].sectionName` | Supported | Unknown section names yield `Accepted: False` / `NoMatchingParent` |
| `spec.parentRefs[].port` | Not supported | Ignored |
| `spec.hostnames` | Supported | Intersected with listener hostnames per the spec |
| `spec.rules[].backendRefs` | Supported | `core/Service` only |
| `spec.rules[].backendRefs[].weight` | Supported | Normalised across each backend's ready endpoints |
| `spec.rules[].backendRefs[].namespace` | Supported | Requires a matching `ReferenceGrant` |
| `spec.rules[].timeouts` | Not supported | Ignored |
| `spec.rules[].sessionPersistence` | Not supported | Ignored |

### Rule matches

| Match | Status | Notes |
|---|---|---|
| `path.type: PathPrefix` | Supported | Expanded to an exact `/foo` plus a `/foo/` prefix, per the spec |
| `path.type: Exact` | Supported | |
| `path.type: RegularExpression` | Not supported | **Known gap**: degrades to the catch-all prefix `/`, which widens the route |
| `headers` (`Exact`) | Supported | |
| `headers` (`RegularExpression`) | Not supported | **Known gap**: silently dropped, which widens the route |
| `method` | Not supported | **Known gap**: silently ignored |
| `queryParams` | Not supported | **Known gap**: silently ignored |

### Rule filters

| Filter | Status | Notes |
|---|---|---|
| `RequestHeaderModifier` | Partial | `add`, `set` and `remove`; scoped by path prefix, not by route identity |
| `ResponseHeaderModifier` | Partial | Same scoping caveat as above |
| `RequestRedirect` | Supported | `scheme`, `hostname`, `port` and `statusCode` |
| `URLRewrite` | Not supported | Logged and dropped |
| `RequestMirror` | Not supported | Logged and dropped |
| `ExtensionRef` | Not supported | Logged and dropped |

> **Note**: filters are currently emitted into the
> shared filter chain of the listener, scoped only by
> the rule's path prefix. A filter on `/api` in one
> route therefore also applies to another route's
> `/api` on the same listener.

### HTTPRoute status

| Condition | Status | Notes |
|---|---|---|
| `Accepted` | Supported | Set by the Gateway controller once the data plane has rolled out |
| `Accepted: False` | Supported | `NoMatchingParent`, `NotAllowedByListeners`, `NoMatchingListenerHostname` |
| `ResolvedRefs` | Supported | `InvalidKind`, `RefNotPermitted`, `BackendNotFound` |
| Parent cleanup on Gateway deletion | Not supported | **Known gap**: stale `status.parents` entries are left behind |

## Operational features

| Feature | Status |
|---|---|
| Server-side apply for all mutations | Supported |
| Owner references on child resources | Supported |
| Finalizer on `Gateway` | Supported |
| Config-hash annotation to force data-plane rollout | Supported |
| Leader election / multi-replica HA | Not supported |
| Prometheus metrics | Not supported |
| Health and readiness endpoints | Not supported |
| Kubernetes Events | Not supported |

## Conformance

The Gateway API conformance suite runs in CI:

```console
make conformance
```

See [`.github/workflows/conformance.yaml`] for the
pinned suite version and the profiles exercised.

[`.github/workflows/conformance.yaml`]: ../.github/workflows/conformance.yaml
