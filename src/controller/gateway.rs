// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway reconciler with finalizer-based lifecycle management.

use std::{fmt::Debug, sync::Arc, time::Duration};

use gateway_api::{gateways::Gateway, httproutes::HTTPRoute, referencegrants::ReferenceGrant};
use kube::{
    Api, Resource, ResourceExt as _,
    api::{Patch, PatchParams},
    runtime::{
        controller::Action,
        events::{Event, EventType},
        finalizer::{self, Event as FinalizerEvent},
        reflector::ObjectRef,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use tracing::{debug, error, info};

use super::{gateway_status, ownership, praxis_config, rollout, route_parent_status};
use crate::{
    context::{Context, GATEWAY_FINALIZER},
    error::{OperatorError, Result},
    gateway_api::{attachment::AttachedRoute, conditions, protocol::ListenerProtocol, route_status},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// API group owning `Gateway` and `HTTPRoute`.
const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

// -----------------------------------------------------------------------------
// Reconciler
// -----------------------------------------------------------------------------

/// Reconciles a [`Gateway`] through its full lifecycle.
///
/// Uses a finalizer to ensure cleanup runs before deletion. On apply,
/// generates Praxis configuration and applies child `Deployment`,
/// `ConfigMap`, and `Service` resources via server-side apply.
///
/// # Errors
///
/// Returns an error if the finalizer cannot be maintained, if any API
/// read the reconciliation depends on fails, or if applying a child
/// resource or the Gateway status is rejected. The error reaches
/// [`error_policy`], which requeues with backoff.
pub async fn reconcile(gw: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    info!("reconciling Gateway {ns}/{name}");

    let api = Api::<Gateway>::namespaced(ctx.client.clone(), &ns);
    finalizer::finalizer(&api, GATEWAY_FINALIZER, gw, |event| {
        Box::pin(async {
            match event {
                FinalizerEvent::Apply(gateway) => Box::pin(apply(gateway, &ctx)).await,
                FinalizerEvent::Cleanup(gateway) => {
                    cleanup(&gateway, &ctx).await;
                    Ok(Action::await_change())
                },
            }
        })
    })
    .await
    .map_err(|err| OperatorError::Finalizer(Box::new(err)))
}

/// Error policy for Gateway reconciliation failures.
///
/// Uses differentiated backoff: shorter for transient API errors,
/// longer for configuration or logic errors.
pub fn error_policy(_gw: Arc<Gateway>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    let delay = match error {
        OperatorError::Kube(_) | OperatorError::Finalizer(_) => Duration::from_secs(15),
        OperatorError::MissingObjectKey(_)
        | OperatorError::GatewayClassNotFound(_)
        | OperatorError::LeadershipLost
        | OperatorError::CacheSync(_)
        | OperatorError::Serialization(_)
        | OperatorError::YamlSerialization(_) => Duration::from_secs(30),
    };
    error!(
        %error, "Gateway reconciliation failed, retrying in {delay:?}"
    );
    Action::requeue(delay)
}

// -----------------------------------------------------------------------------
// Apply
// -----------------------------------------------------------------------------

/// Full apply path: validate, generate config, apply child resources,
/// update Gateway and route statuses.
///
/// Route parent statuses are set **after** child resources are applied
/// and the Deployment rollout is verified, preventing the conformance
/// test from sending traffic before the data plane has the latest
/// configuration.
async fn apply(gw: Arc<Gateway>, ctx: &Context) -> Result<Action> {
    if !ownership::validate_gateway_class(&ctx.client, &gw).await? || reject_unsupported_spec(ctx, &gw).await? {
        return Ok(Action::await_change());
    }

    let routes = ctx.stores.routes();
    let attached = ownership::collect_routes(&gw, &routes, &ctx.stores);
    let ns = gw.namespace().unwrap_or_default();
    let grants = ctx.stores.grants();
    let config_changed = apply_config_if_supported(&ctx.client, &gw, &attached, &ns, &grants).await?;

    gateway_status::build_and_apply_gateway_status(ctx, &gw, &gw.spec.listeners, &attached).await?;

    let can_accept = can_accept_routes(&ctx.client, &gw, &ns, config_changed).await;
    if can_accept {
        route_parent_status::update_route_parent_statuses(&ctx.client, &gw, &attached, &grants).await?;
    }

    let requeue_secs = if can_accept { 15 } else { 2 };
    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
}

/// Generates and applies Praxis config for supported listeners.
///
/// Returns `true` when the config hash changed (new Deployment rollout
/// was triggered).
async fn apply_config_if_supported(
    client: &kube::Client,
    gw: &Gateway,
    attached: &[AttachedRoute<'_>],
    ns: &str,
    grants: &[ReferenceGrant],
) -> Result<bool> {
    let has_supported = gw
        .spec
        .listeners
        .iter()
        .any(|listener| ListenerProtocol::is_supported(&listener.protocol));
    if !has_supported {
        return Ok(false);
    }

    let child = crate::resources::labels::child_name(&gw.name_any());
    let prev_hash = rollout::current_deployment_hash(client, ns, &child).await;
    let config = praxis_config::build_praxis_config(client, &gw.spec.listeners, attached, grants).await?;
    let new_hash = Box::pin(praxis_config::apply_child_resources(client, gw, &config)).await?;

    let changed = prev_hash.as_deref() != Some(&new_hash);
    debug!(
        gateway = %gw.name_any(),
        config_changed = changed,
        routes = attached.len(),
        "config apply result"
    );
    Ok(changed)
}

/// Returns `true` when the config is stable and the Deployment is fully
/// rolled out, so attached routes may be marked accepted.
async fn can_accept_routes(client: &kube::Client, gw: &Gateway, ns: &str, config_changed: bool) -> bool {
    let child = crate::resources::labels::child_name(&gw.name_any());
    let rolled_out = rollout::is_deployment_rolled_out(client, ns, &child).await;

    let can_accept = !config_changed && rolled_out;
    debug!(gateway = %gw.name_any(), can_accept, "route acceptance decision");
    can_accept
}

/// Rejects a Gateway whose spec this operator cannot honour.
///
/// Returns `true` when the Gateway was rejected and the caller should
/// stop reconciling it.
async fn reject_unsupported_spec(ctx: &Context, gw: &Gateway) -> Result<bool> {
    let Some((reason, message)) = unsupported_spec_reason(gw) else {
        return Ok(false);
    };

    let generation = gw.metadata.generation.unwrap_or(1);
    reject_gateway(&ctx.client, gw, generation, reason, message).await?;
    Box::pin(publish_rejection(ctx, gw, reason, message)).await;

    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    info!("Gateway {ns}/{name} rejected: {message}");
    Ok(true)
}

/// Emits a warning event describing why a Gateway was rejected.
///
/// A rejected Gateway is otherwise inert, and its condition is easy to
/// miss; an event puts the reason in `kubectl describe`. The recorder
/// deduplicates within a TTL window, so a Gateway that stays rejected
/// does not accumulate an event per reconcile.
///
/// A failure to publish is logged rather than propagated: losing an
/// event must not turn a clean rejection into a reconcile error.
async fn publish_rejection(ctx: &Context, gw: &Gateway, reason: &str, message: &str) {
    let event = Event {
        action: "Reject".to_owned(),
        note: Some(message.to_owned()),
        reason: reason.to_owned(),
        secondary: None,
        type_: EventType::Warning,
    };

    if let Err(err) = ctx.recorder.publish(&event, &gw.object_ref(&())).await {
        debug!(%err, "could not publish rejection event");
    }
}

/// Returns the `(reason, message)` for a Gateway spec this operator
/// cannot honour.
///
/// `parametersRef` carries implementation-specific configuration this
/// operator does not define, and requested addresses cannot be
/// satisfied because the data-plane Service takes whatever address its
/// provider assigns.
fn unsupported_spec_reason(gw: &Gateway) -> Option<(&'static str, &'static str)> {
    if has_parameters_ref(gw) {
        return Some(("InvalidParameters", "parametersRef is not supported"));
    }

    if has_requested_addresses(gw) {
        return Some((
            "UnsupportedAddress",
            "spec.addresses is not supported; the data-plane Service address is assigned by the cluster",
        ));
    }

    None
}

/// Checks whether a `Gateway` asks for a specific address.
///
/// An entry carrying no `value` asks for nothing in particular — the
/// Gateway API defines it as "assign an address matching the requested
/// type", which is what the data-plane Service does anyway. Only an
/// entry naming an address is a request this operator cannot honour.
fn has_requested_addresses(gw: &Gateway) -> bool {
    gw.spec
        .addresses
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|address| address.value.is_some())
}

// -----------------------------------------------------------------------------
// Cleanup
// -----------------------------------------------------------------------------

/// Cleanup path: owner references handle child deletion automatically.
async fn cleanup(gw: &Gateway, ctx: &Context) {
    let name = gw.name_any();
    let ns = gw.namespace().unwrap_or_else(|| {
        tracing::warn!(gateway = %name, "Gateway has no namespace during cleanup");
        String::new()
    });
    info!("cleaning up Gateway {ns}/{name} (owner refs handle child deletion)");

    clear_route_parent_statuses(ctx, &name, &ns).await;
}

/// Removes this Gateway's entries from every route that referenced it.
///
/// Child resources are reclaimed by owner references, but route status
/// is not owned by the Gateway, so a stale `Accepted` entry naming a
/// deleted parent would survive indefinitely.
///
/// Failures are logged rather than propagated: a Gateway must always be
/// able to finish deleting, and a route left with a stale entry is a
/// smaller problem than a finalizer that never releases.
async fn clear_route_parent_statuses(ctx: &Context, gw_name: &str, gw_ns: &str) {
    let client = &ctx.client;

    for route in &ctx.stores.routes() {
        if let Err(err) = route_status::clear_parent_statuses(client, route, gw_name, gw_ns).await {
            tracing::warn!(%err, route = route.name_any(), "could not clear parent status");
        }
    }
}

// -----------------------------------------------------------------------------
// Server-side Apply
// -----------------------------------------------------------------------------

/// Applies a namespaced Kubernetes resource via server-side apply.
pub(super) async fn apply_resource<K>(client: &kube::Client, ns: &str, resource: &K) -> Result<()>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + Send
        + Sync,
    <K as Resource>::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), ns);
    let name = resource
        .meta()
        .name
        .as_deref()
        .ok_or(OperatorError::MissingObjectKey(".metadata.name"))?;
    api.patch(
        name,
        &PatchParams::apply("praxis-operator").force(),
        &Patch::Apply(resource),
    )
    .await?;
    debug!("applied {name}");
    Ok(())
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Checks whether a `Gateway` has `spec.infrastructure.parametersRef` set.
fn has_parameters_ref(gw: &Gateway) -> bool {
    gw.spec
        .infrastructure
        .as_ref()
        .is_some_and(|infra| infra.parameters_ref.is_some())
}

/// Rejects a Gateway by setting `Accepted: False` with the given reason.
async fn reject_gateway(
    client: &kube::Client,
    gw: &Gateway,
    generation: i64,
    reason: &str,
    message: &str,
) -> Result<()> {
    let status = serde_json::json!({
        "conditions": [
            conditions::not_accepted(generation, reason, message),
            conditions::not_programmed(generation, "Invalid", message),
        ],
    });

    gateway_status::apply_gateway_status(client, gw, &status).await
}

// -----------------------------------------------------------------------------
// Watch Mappers
// -----------------------------------------------------------------------------

/// Extracts a [`Gateway`] [`ObjectRef`] from an [`HTTPRoute`]'s parent refs.
///
/// Finds the first parentRef targeting a `Gateway` and returns an
/// [`ObjectRef`] pointing to it. Used by [`Controller::watches`] to trigger
/// Gateway reconciliation on route changes.
///
/// [`Controller::watches`]: kube::runtime::controller::Controller::watches
pub fn map_route_to_gateway(route: &HTTPRoute) -> Option<ObjectRef<Gateway>> {
    let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
    let parent_refs = route.spec.parent_refs.as_deref()?;
    find_gateway_parent_ref(parent_refs, route_ns)
}

/// Maps a [`ReferenceGrant`] change to the Gateways it can affect.
///
/// `known_gateways` is the controller's own cache, so the mapper needs
/// no extra API calls. A grant trusting `Gateway` sources only affects
/// Gateways in the trusted namespace (cross-namespace TLS secrets); a
/// grant trusting route sources can affect any Gateway, because a route
/// in the trusted namespace may attach anywhere.
pub fn map_grant_to_gateways(grant: &ReferenceGrant, known_gateways: &[Arc<Gateway>]) -> Vec<ObjectRef<Gateway>> {
    let mut refs = Vec::new();

    for gw in known_gateways {
        let gw_ns = gw.namespace().unwrap_or_default();
        if grant_affects_namespace(grant, &gw_ns) {
            refs.push(ObjectRef::new(&gw.name_any()).within(&gw_ns));
        }
    }

    debug!(
        grant = %grant.name_any(),
        gateways = refs.len(),
        "mapped ReferenceGrant change to Gateways"
    );
    refs
}

/// Returns `true` when a grant's `from` list can influence Gateways in
/// `gateway_ns`.
fn grant_affects_namespace(grant: &ReferenceGrant, gateway_ns: &str) -> bool {
    grant.spec.from.iter().any(|from| {
        if from.group != GATEWAY_GROUP {
            return false;
        }
        from.kind != "Gateway" || from.namespace == gateway_ns
    })
}

/// Finds the first Gateway parent ref and returns an [`ObjectRef`] for it.
fn find_gateway_parent_ref(
    parent_refs: &[gateway_api::httproutes::HttpRouteParentRefs],
    route_ns: &str,
) -> Option<ObjectRef<Gateway>> {
    for parent in parent_refs {
        let group = parent.group.as_deref().unwrap_or(GATEWAY_GROUP);
        let kind = parent.kind.as_deref().unwrap_or("Gateway");
        if group == GATEWAY_GROUP && kind == "Gateway" {
            let gw_ns = parent.namespace.as_deref().unwrap_or(route_ns);
            return Some(ObjectRef::new(&parent.name).within(gw_ns));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::default_trait_access, reason = "tests")]
mod tests {
    use gateway_api::{
        gateways::GatewayAddresses,
        httproutes::{HttpRouteParentRefs, HttpRouteSpec},
        referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo},
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use serde_json::Value;

    use super::*;
    use crate::testing;

    #[test]
    fn test_map_route_to_gateway_basic() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-gateway".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let result = map_route_to_gateway(&route);
        assert!(result.is_some(), "should map route to gateway");

        let obj_ref = result.unwrap();
        assert_eq!(obj_ref.name, "my-gateway", "gateway name should match");
    }

    #[test]
    fn test_map_route_to_gateway_no_parent_refs() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: None,
                ..Default::default()
            },
            status: None,
        };

        assert!(
            map_route_to_gateway(&route).is_none(),
            "should return None with no parent refs"
        );
    }

    #[test]
    fn test_map_route_to_gateway_non_gateway_parent() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-svc".to_owned(),
                    kind: Some("Service".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        assert!(
            map_route_to_gateway(&route).is_none(),
            "should return None for non-Gateway parent"
        );
    }

    #[test]
    fn test_map_route_to_gateway_cross_namespace() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("app-ns".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-gateway".to_owned(),
                    namespace: Some("gateway-ns".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let result = map_route_to_gateway(&route);
        assert!(result.is_some(), "should map cross-namespace route");
    }

    #[test]
    fn test_has_parameters_ref_none() {
        let gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        assert!(
            !has_parameters_ref(&gw),
            "default gateway should have no parameters ref"
        );
    }

    #[test]
    fn test_unsupported_spec_reason_accepts_a_plain_gateway() {
        let gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        assert!(
            unsupported_spec_reason(&gw).is_none(),
            "a Gateway with neither parametersRef nor addresses is supported"
        );
    }

    #[test]
    fn test_unsupported_spec_reason_rejects_requested_addresses() {
        let mut gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        gw.spec.addresses = Some(vec![GatewayAddresses {
            value: Some("192.0.2.1".to_owned()),
            ..Default::default()
        }]);

        let (reason, _) = unsupported_spec_reason(&gw).expect("requested addresses should be rejected");
        assert_eq!(
            reason, "UnsupportedAddress",
            "the Gateway API reason for an address this operator cannot assign is UnsupportedAddress"
        );
    }

    #[test]
    fn test_an_address_entry_without_a_value_asks_for_nothing() {
        let mut gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        gw.spec.addresses = Some(vec![GatewayAddresses {
            r#type: Some("IPAddress".to_owned()),
            value: None,
        }]);

        assert!(
            unsupported_spec_reason(&gw).is_none(),
            "the Gateway API reads a valueless entry as `assign an address of this type`, which is \
             what the data-plane Service does regardless — rejecting it refuses a Gateway that \
             asked for nothing this operator cannot give"
        );
    }

    #[test]
    fn test_one_named_address_rejects_the_whole_gateway() {
        let mut gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        gw.spec.addresses = Some(vec![
            GatewayAddresses {
                r#type: Some("IPAddress".to_owned()),
                value: None,
            },
            GatewayAddresses {
                value: Some("192.0.2.1".to_owned()),
                ..Default::default()
            },
        ]);

        assert!(
            unsupported_spec_reason(&gw).is_some(),
            "a valueless entry beside a named one does not excuse the named one, which the \
             operator still cannot assign"
        );
    }

    #[test]
    fn test_unsupported_spec_reason_ignores_an_empty_address_list() {
        let mut gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        gw.spec.addresses = Some(Vec::new());

        assert!(
            unsupported_spec_reason(&gw).is_none(),
            "an empty address list requests nothing and must not be rejected"
        );
    }

    #[test]
    fn test_map_grant_to_gateways_returns_matching_gateway() {
        let gateways = [gateway("gw", "infra")];
        let refs = map_grant_to_gateways(&grant("Gateway", "infra"), &gateways);

        assert_eq!(refs.len(), 1, "a Gateway in the trusted namespace should be enqueued");
        assert_eq!(refs[0].name, "gw", "the enqueued ref should name the Gateway");
        assert_eq!(
            refs[0].namespace.as_deref(),
            Some("infra"),
            "the enqueued ref should carry the Gateway namespace"
        );
    }

    #[test]
    fn test_map_grant_to_gateways_skips_untrusted_namespaces() {
        let gateways = [gateway("gw", "other")];

        assert!(
            map_grant_to_gateways(&grant("Gateway", "infra"), &gateways).is_empty(),
            "a Gateway-sourced grant must not enqueue Gateways from other namespaces"
        );
    }

    #[test]
    fn test_map_grant_to_gateways_route_source_reaches_every_gateway() {
        let gateways = [gateway("a", "infra"), gateway("b", "edge")];
        let refs = map_grant_to_gateways(&grant("HTTPRoute", "apps"), &gateways);

        assert_eq!(
            refs.len(),
            2,
            "a route-sourced grant can affect any Gateway the route attaches to"
        );
    }

    #[test]
    fn test_map_grant_to_gateways_ignores_foreign_groups() {
        let gateways = [gateway("gw", "infra")];
        let mut foreign = grant("Gateway", "infra");
        foreign.spec.from[0].group = "example.com".to_owned();

        assert!(
            map_grant_to_gateways(&foreign, &gateways).is_empty(),
            "a grant trusting a non-Gateway-API source is irrelevant to this controller"
        );
    }

    #[test]
    fn test_map_grant_to_gateways_without_gateways_is_empty() {
        assert!(
            map_grant_to_gateways(&grant("HTTPRoute", "apps"), &[]).is_empty(),
            "an empty cache enqueues nothing"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds a Gateway cache entry with the given name and namespace.
    fn gateway(name: &str, namespace: &str) -> Arc<Gateway> {
        Arc::new(Gateway {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        })
    }

    /// Builds a `ReferenceGrant` trusting `from_kind` sources in `from_ns`.
    fn grant(from_kind: &str, from_ns: &str) -> ReferenceGrant {
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow".to_owned()),
                namespace: Some("data".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: GATEWAY_GROUP.to_owned(),
                    kind: from_kind.to_owned(),
                    namespace: from_ns.to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Secret".to_owned(),
                    name: None,
                }],
            },
        }
    }

    // -----------------------------------------------------------------------
    // Reconciliation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_full_apply_writes_children_status_and_route_status() {
        let route = attachable_route();
        let (ctx, journal) = testing::fake_context(
            reconcile_responses(),
            testing::Cached {
                routes: vec![route],
                ..Default::default()
            },
        );

        let action = Box::pin(apply(Arc::new(reconcilable_gateway()), &ctx))
            .await
            .expect("every call is answered");

        for kind in ["/configmaps", "/deployments", "/services", "/poddisruptionbudgets"] {
            assert!(
                !journal.matching(kind).is_empty(),
                "the data plane is only complete once every child is applied: {kind}"
            );
        }
        assert!(
            !journal.matching("/gateways/gw/status").is_empty(),
            "a Gateway with no status tells conformance nothing about whether it is serving"
        );
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(2)),
            "a Deployment that has not finished rolling out is re-checked quickly, not in fifteen \
             seconds"
        );
    }

    #[tokio::test]
    async fn test_a_finished_rollout_with_unchanged_config_admits_routes() {
        let (client, _) = testing::fake_client(vec![rolled_out_deployment()]);

        assert!(
            can_accept_routes(&client, &reconcilable_gateway(), "infra", false).await,
            "a settled data plane serving the config that is already applied is exactly when a \
             route may be reported accepted"
        );
    }

    #[tokio::test]
    async fn test_a_changed_config_holds_routes_back() {
        let (client, _) = testing::fake_client(vec![rolled_out_deployment()]);

        assert!(
            !can_accept_routes(&client, &reconcilable_gateway(), "infra", true).await,
            "the pods are still running the previous config, so accepting the route would invite \
             traffic the data plane cannot route yet"
        );
    }

    #[tokio::test]
    async fn test_an_unfinished_rollout_holds_routes_back() {
        let (client, _) = testing::fake_client(vec![]);

        assert!(
            !can_accept_routes(&client, &reconcilable_gateway(), "infra", false).await,
            "a Deployment that does not exist yet has certainly not rolled out"
        );
    }

    #[tokio::test]
    async fn test_a_gateway_from_another_class_is_left_alone() {
        let (ctx, journal) = testing::fake_context(
            vec![testing::Canned::ok(
                "/gatewayclasses/praxis",
                serde_json::json!({
                    "apiVersion": "gateway.networking.k8s.io/v1",
                    "kind": "GatewayClass",
                    "metadata": { "name": "praxis" },
                    "spec": { "controllerName": "example.com/other" },
                }),
            )],
            testing::Cached::default(),
        );

        let action = Box::pin(apply(Arc::new(reconcilable_gateway()), &ctx))
            .await
            .expect("skipping is not a failure");

        assert_eq!(action, Action::await_change(), "there is nothing to requeue for");
        assert_eq!(
            journal.requests().len(),
            1,
            "the class lookup is the only call another controller's Gateway should provoke"
        );
    }

    #[tokio::test]
    async fn test_a_gateway_requesting_an_address_is_rejected() {
        let (ctx, journal) = testing::fake_context(reconcile_responses(), testing::Cached::default());
        let mut gw = reconcilable_gateway();
        gw.spec.addresses = Some(vec![GatewayAddresses {
            value: Some("1.2.3.4".to_owned()),
            ..Default::default()
        }]);

        let action = Box::pin(apply(Arc::new(gw), &ctx))
            .await
            .expect("a rejection is a clean outcome");

        let status = journal
            .matching("/gateways/gw/status")
            .pop()
            .and_then(|request| request.body)
            .expect("the rejection has to be written where the author will see it");
        assert_eq!(
            status.pointer("/status/conditions/0/reason").and_then(Value::as_str),
            Some("UnsupportedAddress"),
            "the data-plane Service takes whatever address its provider assigns, and silently \
             ignoring the request would look like it had been honoured"
        );
        assert_eq!(action, Action::await_change(), "a rejected Gateway waits for an edit");
        assert!(
            journal.matching("/deployments").is_empty(),
            "a rejected Gateway must not get a data plane"
        );
    }

    #[tokio::test]
    async fn test_cleanup_clears_this_gateways_route_entries() {
        let route = route_with_parent_status();
        let (ctx, journal) = testing::fake_context(
            vec![route_response()],
            testing::Cached {
                routes: vec![route],
                ..Default::default()
            },
        );

        cleanup(&reconcilable_gateway(), &ctx).await;

        assert!(
            !journal.matching("/httproutes/route/status").is_empty(),
            "child resources go with owner references, but route status does not — a stale entry \
             naming a deleted parent would outlive the Gateway"
        );
    }

    #[tokio::test]
    async fn test_cleanup_survives_a_route_it_cannot_patch() {
        let route = attachable_route();
        let (ctx, _) = testing::fake_context(
            vec![testing::Canned::server_error("")],
            testing::Cached {
                routes: vec![route],
                ..Default::default()
            },
        );

        cleanup(&reconcilable_gateway(), &ctx).await;
        // Reaching here is the assertion: a Gateway that cannot finish
        // deleting because one route refused a patch would hold its
        // finalizer forever.
    }

    #[tokio::test]
    async fn test_apply_resource_reports_a_refused_patch() {
        let (client, _) = testing::failing_client();
        let cm = k8s_openapi::api::core::v1::ConfigMap {
            metadata: ObjectMeta {
                name: Some("praxis-gw".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = apply_resource(&client, "infra", &cm)
            .await
            .expect_err("a 500 is not an applied resource");

        assert!(
            matches!(error, OperatorError::Kube(_)),
            "an apply the API server refused has to fail the reconcile, or the Gateway reports a \
             data plane it never built: {error}"
        );
    }

    #[tokio::test]
    async fn test_apply_resource_needs_a_name() {
        let (client, _) = testing::fake_client(vec![]);
        let cm = k8s_openapi::api::core::v1::ConfigMap::default();

        let error = apply_resource(&client, "infra", &cm)
            .await
            .expect_err("an unnamed resource cannot be applied");

        assert!(
            matches!(error, OperatorError::MissingObjectKey(".metadata.name")),
            "the missing field is named, because the alternative is a 404 with no explanation: \
             {error}"
        );
    }

    #[tokio::test]
    async fn test_error_policy_retries_a_transient_failure_sooner() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());
        let gw = Arc::new(reconcilable_gateway());

        let transient = error_policy(
            Arc::clone(&gw),
            &OperatorError::Kube(kube::Error::LinesCodecMaxLineLengthExceeded),
            Arc::clone(&ctx),
        );
        let logic = error_policy(gw, &OperatorError::MissingObjectKey(".metadata.uid"), ctx);

        assert_eq!(
            transient,
            Action::requeue(Duration::from_secs(15)),
            "an API server that blinked is worth retrying soon"
        );
        assert_eq!(
            logic,
            Action::requeue(Duration::from_secs(30)),
            "a malformed object will not fix itself in fifteen seconds, and retrying it as fast \
             only burns the API budget"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a Gateway this operator owns and can serve.
    fn reconcilable_gateway() -> Gateway {
        use gateway_api::gateways::{GatewayListeners, GatewaySpec};

        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("infra".to_owned()),
                uid: Some("uid".to_owned()),
                generation: Some(1),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners: vec![GatewayListeners {
                    name: "http".to_owned(),
                    port: 80,
                    protocol: "HTTP".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds a route already carrying this controller's status entry
    /// for [`reconcilable_gateway`].
    fn route_with_parent_status() -> HTTPRoute {
        let mut route = attachable_route();
        route.status = serde_json::from_value(serde_json::json!({
            "parents": [{
                "parentRef": {
                    "group": GATEWAY_GROUP,
                    "kind": "Gateway",
                    "name": "gw",
                    "namespace": "infra",
                },
                "controllerName": crate::context::CONTROLLER_NAME,
                "conditions": [],
            }],
        }))
        .expect("the entry is the shape the operator writes");
        route
    }

    /// Builds a route that attaches to [`reconcilable_gateway`].
    fn attachable_route() -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "gw".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        }
    }

    /// Every response a clean reconcile asks for.
    fn reconcile_responses() -> Vec<testing::Canned> {
        let mut responses = vec![owned_class_response()];
        responses.extend(child_apply_responses());
        responses.push(gateway_response());
        responses.push(route_response());
        responses
    }

    /// The `GatewayClass` this operator owns.
    fn owned_class_response() -> testing::Canned {
        testing::Canned::ok(
            "/gatewayclasses/praxis",
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "GatewayClass",
                "metadata": { "name": "praxis" },
                "spec": { "controllerName": crate::context::CONTROLLER_NAME },
            }),
        )
    }

    /// An accepting answer for each child resource apply.
    fn child_apply_responses() -> Vec<testing::Canned> {
        vec![
            testing::Canned::ok("/configmaps", serde_json::json!({ "kind": "ConfigMap" })),
            testing::Canned::ok("/deployments", serde_json::json!({ "kind": "Deployment" })),
            testing::Canned::ok("/services", serde_json::json!({ "kind": "Service" })),
            testing::Canned::ok(
                "/poddisruptionbudgets",
                serde_json::json!({ "kind": "PodDisruptionBudget" }),
            ),
        ]
    }

    /// The object the API server hands back from a Gateway status apply.
    fn gateway_response() -> testing::Canned {
        testing::Canned::ok(
            "/gateways/gw",
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "Gateway",
                "metadata": { "name": "gw", "namespace": "infra" },
                "spec": { "gatewayClassName": "praxis", "listeners": [] },
            }),
        )
    }

    /// The object the API server hands back from a route status apply.
    fn route_response() -> testing::Canned {
        testing::Canned::ok(
            "/httproutes/route",
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": { "name": "route", "namespace": "infra" },
                "spec": {},
            }),
        )
    }

    /// A child Deployment whose rollout has finished.
    ///
    /// Placed ahead of the generic `/deployments` apply response so the
    /// GET that reads rollout state sees a finished one.
    fn rolled_out_deployment() -> testing::Canned {
        testing::Canned::ok(
            "/deployments/praxis-gw",
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": { "name": "praxis-gw", "namespace": "infra", "generation": 1 },
                "spec": { "selector": { "matchLabels": {} }, "template": {} },
                "status": {
                    "observedGeneration": 1,
                    "readyReplicas": 1,
                    "conditions": [{
                        "type": "Progressing",
                        "status": "True",
                        "reason": "NewReplicaSetAvailable",
                        "lastTransitionTime": "2026-01-01T00:00:00Z",
                    }],
                },
            }),
        )
    }
}
