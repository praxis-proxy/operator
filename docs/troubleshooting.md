# Troubleshooting

Common problems when running the Praxis operator, and
how to diagnose them.

Start by confirming which of the four objects in the
chain is unhealthy:

```console
kubectl get gatewayclass praxis
kubectl get gateway -A
kubectl get httproute -A
kubectl -n praxis-system get pods
```

Each section below covers one failure in that chain.

## The GatewayClass is not accepted

`kubectl get gatewayclass praxis` shows `ACCEPTED` as
empty or `False`.

The operator only reconciles a `GatewayClass` whose
`spec.controllerName` matches its own:

```console
kubectl get gatewayclass praxis \
    -o jsonpath='{.spec.controllerName}'
```

It must print `praxis.sh/gateway-controller`. If it
prints anything else, the class belongs to another
implementation and this operator ignores it by design.

If the value is correct but the status is still empty,
the operator is not running or cannot reach the API:

```console
kubectl -n praxis-system logs deployment/praxis-operator
```

## A Gateway never becomes Programmed

Read the conditions first:

```console
kubectl -n <ns> get gateway <name> \
    -o jsonpath='{.status.conditions}' | jq
```

| Reason | Meaning | Fix |
|---|---|---|
| `InvalidParameters` | `spec.infrastructure.parametersRef` is set | Remove it; it is not supported |
| `ListenersNotValid` with `Accepted: False` | No listener uses `HTTP` or `HTTPS` | Change the listener protocol |
| `Pending` on `Programmed` | Data plane not ready, or no load-balancer address | See the next two subsections |
| `Invalid` on `Programmed` | No valid listener | Same as `ListenersNotValid` |

### The data-plane pod is not ready

Each Gateway gets a child Deployment named
`praxis-<gateway-name>`:

```console
kubectl -n <ns> get deployment praxis-<gateway-name>
kubectl -n <ns> describe pod -l app.kubernetes.io/instance=<gateway-name>
```

A `CrashLoopBackOff` here usually means Praxis rejected
the generated config. Read the container logs and then
the ConfigMap it was given:

```console
kubectl -n <ns> logs deployment/praxis-<gateway-name>
kubectl -n <ns> get configmap praxis-<gateway-name> \
    -o jsonpath='{.data.praxis\.yaml}'
```

`ImagePullBackOff` means `PRAXIS_IMAGE` points at an
image the cluster cannot pull. It is set on the
operator Deployment, not on the Gateway:

```console
kubectl -n praxis-system get deployment praxis-operator \
    -o jsonpath='{.spec.template.spec.containers[0].env}'
```

### There is no load-balancer address

`Programmed` stays `False` with reason `Pending` until
the child Service has an ingress IP. On KIND or
bare-metal you need a load-balancer provider such as
MetalLB:

```console
kubectl -n <ns> get service praxis-<gateway-name>
```

An `EXTERNAL-IP` stuck in `<pending>` is a cluster
problem, not an operator problem.

## An HTTPRoute is not accepted

Check the parent status for the Gateway you expect:

```console
kubectl -n <ns> get httproute <name> \
    -o jsonpath='{.status.parents}' | jq
```

| Reason | Meaning | Fix |
|---|---|---|
| `NoMatchingParent` | `parentRefs[].sectionName` names no listener | Correct the section name, or drop it |
| `NotAllowedByListeners` | The listener's `allowedRoutes.namespaces` excludes the route | Set `from: All`, or label the namespace to match the selector |
| `NoMatchingListenerHostname` | No route hostname intersects a listener hostname | Align the hostnames; note that `example.com` does **not** match `*.example.com` |
| `BackendNotFound` | The backend `Service` does not exist | Create it, or fix the name and namespace |
| `InvalidKind` | A `backendRef` is not a `core/Service` | Only Services are supported |
| `RefNotPermitted` | A cross-namespace `backendRef` has no `ReferenceGrant` | Create a grant in the backend's namespace |

### No parent status at all

If `status.parents` is empty, the operator never
matched the route to a Gateway. Verify the `parentRefs`
name and namespace, and that the Gateway's
`GatewayClass` belongs to this controller — routes
attached to another implementation's Gateway are
deliberately left alone.

## A cross-namespace reference is denied

Both TLS `certificateRefs` and `backendRefs` need a
`ReferenceGrant` in the *target* namespace. The grant's
`spec.from` describes the referrer:

```yaml
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata:
  name: allow-routes
  namespace: backend-ns
spec:
  from:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      namespace: route-ns
  to:
    - group: ""
      kind: Service
```

Common mistakes: putting the grant in the referrer's
namespace instead of the target's, and using `kind:
Gateway` for a backend reference (it must be the route
kind) or `kind: HTTPRoute` for a TLS secret (it must be
`Gateway`).

## TLS termination fails

The operator validates the Secret before programming
the listener. `ResolvedRefs: False` with reason
`InvalidCertificateRef` means one of:

- the Secret does not exist in the expected namespace;
- it lacks a `tls.crt` or `tls.key` key;
- either value is not PEM-encoded (it must begin with
  `-----BEGIN `).

```console
kubectl -n <ns> get secret <name> \
    -o jsonpath='{.data.tls\.crt}' | base64 -d | head -1
```

## Configuration changes do not take effect

The operator hashes the generated config and stores the
digest as a pod-template annotation, so a config change
triggers a rolling restart. Compare the two:

```console
kubectl -n <ns> get deployment praxis-<gateway-name> \
    -o jsonpath='{.spec.template.metadata.annotations}'
```

Route acceptance is deliberately delayed until the new
`ReplicaSet` reports `NewReplicaSetAvailable`, so a
route can sit without `Accepted: True` for the duration
of a rollout. If the rollout is stuck, the data plane
is rejecting the new config — see the pod logs above.

## Increasing log verbosity

The operator logs JSON at `info` by default. Raise it
with `RUST_LOG`:

```console
kubectl -n praxis-system set env deployment/praxis-operator \
    RUST_LOG=praxis_operator=debug
```

`debug` reports attachment decisions, cluster
resolution, weight distribution, and every status write
that was skipped as a no-op. Remember to set it back:

```console
kubectl -n praxis-system set env deployment/praxis-operator \
    RUST_LOG-
```

## Collecting a support bundle

`hack/dump-debug.sh` prints the operator pod status and
logs, every Praxis data-plane pod, and the Gateway API
objects in one pass. It targets the local KIND cluster
(`KIND_CLUSTER_NAME`, default `praxis-conformance`):

```console
./hack/dump-debug.sh > debug.txt
```

Attach `debug.txt` to any bug report.
