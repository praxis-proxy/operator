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
    listing,
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
                FinalizerEvent::Apply(gw) => Box::pin(apply(gw, &ctx)).await,
                FinalizerEvent::Cleanup(gw) => {
                    cleanup(&gw, &ctx.client).await;
                    Ok(Action::await_change())
                },
            }
        })
    })
    .await
    .map_err(|e| OperatorError::Finalizer(Box::new(e)))
}

/// Error policy for Gateway reconciliation failures.
///
/// Uses differentiated backoff: shorter for transient API errors,
/// longer for configuration or logic errors.
pub fn error_policy(_gw: Arc<Gateway>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    let delay = match error {
        OperatorError::Kube(_) | OperatorError::Finalizer(_) => Duration::from_secs(15),
        _ => Duration::from_secs(30),
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

    let routes = list_all_routes(&ctx.client).await?;
    let attached = ownership::collect_routes(&ctx.client, &gw, &routes).await;
    let ns = gw.namespace().unwrap_or_default();
    let grants = list_all_grants(&ctx.client).await?;
    let config_changed = apply_config_if_supported(&ctx.client, &gw, &attached, &ns, &grants).await?;

    gateway_status::build_and_apply_gateway_status(&ctx.client, &gw, &gw.spec.listeners, &attached).await?;

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
        .any(|l| ListenerProtocol::is_supported(&l.protocol));
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

/// Lists all `HTTPRoute` resources across all namespaces.
async fn list_all_routes(client: &kube::Client) -> Result<Vec<HTTPRoute>> {
    listing::list_all(&Api::<HTTPRoute>::all(client.clone())).await
}

/// Lists all `ReferenceGrant` resources across all namespaces.
async fn list_all_grants(client: &kube::Client) -> Result<Vec<ReferenceGrant>> {
    listing::list_all(&Api::<ReferenceGrant>::all(client.clone())).await
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

    if let Err(e) = ctx.recorder.publish(&event, &gw.object_ref(&())).await {
        debug!(%e, "could not publish rejection event");
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

/// Checks whether a `Gateway` requests specific addresses.
fn has_requested_addresses(gw: &Gateway) -> bool {
    gw.spec.addresses.as_ref().is_some_and(|a| !a.is_empty())
}

// -----------------------------------------------------------------------------
// Cleanup
// -----------------------------------------------------------------------------

/// Cleanup path: owner references handle child deletion automatically.
async fn cleanup(gw: &Gateway, client: &kube::Client) {
    let name = gw.name_any();
    let ns = gw.namespace().unwrap_or_else(|| {
        tracing::warn!(gateway = %name, "Gateway has no namespace during cleanup");
        String::new()
    });
    info!("cleaning up Gateway {ns}/{name} (owner refs handle child deletion)");

    clear_route_parent_statuses(client, &name, &ns).await;
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
async fn clear_route_parent_statuses(client: &kube::Client, gw_name: &str, gw_ns: &str) {
    let routes = match list_all_routes(client).await {
        Ok(routes) => routes,
        Err(e) => {
            tracing::warn!(%e, "could not list routes to clear parent status for {gw_ns}/{gw_name}");
            return;
        },
    };

    for route in &routes {
        if let Err(e) = route_status::clear_parent_statuses(client, route, gw_name, gw_ns).await {
            tracing::warn!(%e, route = route.name_any(), "could not clear parent status");
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
        httproutes::{HTTPRoute, HttpRouteParentRefs, HttpRouteSpec},
        referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo},
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

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
}
