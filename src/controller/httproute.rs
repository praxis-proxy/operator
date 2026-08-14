// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `HTTPRoute` reconciler.
//!
//! Handles rejection statuses (`Accepted = False`) for routes that fail
//! listener validation (wrong section name, namespace policy, hostname
//! mismatch). Acceptance (`Accepted = True`) is set by the Gateway
//! controller after the data-plane Deployment rollout completes.

use std::{sync::Arc, time::Duration};

use gateway_api::{
    gatewayclasses::GatewayClass,
    gateways::{Gateway, GatewayListeners, GatewayListenersAllowedRoutesNamespacesSelector},
    httproutes::{HTTPRoute, HttpRouteParentRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{api::core::v1::Namespace, apimachinery::pkg::apis::meta::v1::Condition};
use kube::{Api, ResourceExt as _, runtime::controller::Action};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::{
    context::{CONTROLLER_NAME, Context},
    error::{OperatorError, Result},
    gateway_api::{conditions, hostname, route_status},
};

// -----------------------------------------------------------------------------
// Reconciler
// -----------------------------------------------------------------------------

/// Reconciles an [`HTTPRoute`] by setting rejection statuses.
///
/// Only sets `Accepted = False` for routes that fail listener
/// validation. `Accepted = True` is set by the Gateway controller
/// after the data-plane Deployment rollout completes, preventing the
/// conformance test from sending traffic to a stale configuration.
pub(crate) async fn reconcile(route: Arc<HTTPRoute>, ctx: Arc<Context>) -> Result<Action> {
    let ns = route_status::route_namespace(&route);
    let name = route.name_any();
    info!("reconciling HTTPRoute {ns}/{name}");

    let Some(refs) = &route.spec.parent_refs else {
        debug!("HTTPRoute {ns}/{name} has no parentRefs, skipping");
        return Ok(Action::await_change());
    };

    apply_rejections(&route, refs, ns, &name, &ctx).await
}

/// Collects and applies rejection statuses for a route's parent refs.
async fn apply_rejections(
    route: &HTTPRoute,
    parent_refs: &[HttpRouteParentRefs],
    ns: &str,
    name: &str,
    ctx: &Context,
) -> Result<Action> {
    let generation = route.metadata.generation.unwrap_or(0);
    let statuses = collect_rejection_statuses(route, parent_refs, ns, generation, ctx).await;
    if statuses.is_empty() {
        return Ok(Action::await_change());
    }

    route_status::apply_parent_statuses(&ctx.client, route, &statuses).await?;
    info!("HTTPRoute {ns}/{name} rejection status applied");
    Ok(Action::await_change())
}

/// Collects rejection-only parent status entries.
///
/// Only returns entries where the route should NOT be accepted.
async fn collect_rejection_statuses(
    route: &HTTPRoute,
    parent_refs: &[HttpRouteParentRefs],
    route_ns: &str,
    generation: i64,
    ctx: &Context,
) -> Vec<Value> {
    let mut statuses = Vec::new();
    for parent_ref in parent_refs {
        if let Some(status) = build_rejection_status(route, parent_ref, route_ns, generation, ctx).await {
            statuses.push(status);
        }
    }
    statuses
}

/// Builds a rejection status for a single `parentRef`.
///
/// Returns `None` when the route should be accepted (the Gateway
/// controller handles acceptance). Returns `Some` only for rejection
/// cases: invalid section name, namespace not allowed, hostname
/// mismatch, or unresolved backend refs.
async fn build_rejection_status(
    route: &HTTPRoute,
    parent_ref: &HttpRouteParentRefs,
    route_ns: &str,
    generation: i64,
    ctx: &Context,
) -> Option<Value> {
    if !route_status::is_gateway_parent_ref(parent_ref) {
        return None;
    }

    let gw_ns = parent_ref.namespace.as_deref().unwrap_or(route_ns);
    let gw = lookup_parent_gateway(&parent_ref.name, gw_ns, route_ns, ctx).await?;

    if !is_managed_gateway_class(&gw, ctx).await {
        return None;
    }

    let rejection = validate_listener_attachment(route, &gw, parent_ref, generation, &ctx.client).await;
    let grants = list_reference_grants(ctx).await;
    let resolve_result = route_status::check_backend_refs(route, route_ns, &ctx.client, &grants).await;
    let resolved = route_status::resolved_refs_condition(&resolve_result, generation);

    let accepted = rejection.unwrap_or_else(|| conditions::accepted(generation, "route accepted"));
    if accepted.status == "True" && resolve_result.is_ok() {
        return None;
    }

    Some(route_status::parent_status_json(
        parent_ref, gw_ns, &accepted, &resolved,
    ))
}

/// Checks whether the `Gateway`'s `GatewayClass` is managed by this
/// controller.
async fn is_managed_gateway_class(gw: &Gateway, ctx: &Context) -> bool {
    let gc_name = &gw.spec.gateway_class_name;
    let gc_api = Api::<GatewayClass>::all(ctx.client.clone());
    gc_api
        .get(gc_name)
        .await
        .is_ok_and(|gc| gc.spec.controller_name == CONTROLLER_NAME)
}

/// Looks up the parent `Gateway` for a `parentRef`.
async fn lookup_parent_gateway(gw_name: &str, gw_ns: &str, route_ns: &str, ctx: &Context) -> Option<Gateway> {
    let gw_api = Api::<Gateway>::namespaced(ctx.client.clone(), gw_ns);
    if let Ok(gw) = gw_api.get(gw_name).await {
        return Some(gw);
    }
    debug!("Gateway {gw_ns}/{gw_name} not found for HTTPRoute in {route_ns}");
    None
}

/// Lists all [`ReferenceGrant`] resources in the cluster.
async fn list_reference_grants(ctx: &Context) -> Vec<ReferenceGrant> {
    let grant_api = Api::<ReferenceGrant>::all(ctx.client.clone());
    match crate::listing::list_all(&grant_api).await {
        Ok(grants) => grants,
        Err(e) => {
            warn!(%e, "failed to list ReferenceGrants");
            Vec::new()
        },
    }
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Returns a rejection condition if the route fails listener validation.
async fn validate_listener_attachment(
    route: &HTTPRoute,
    gw: &Gateway,
    parent_ref: &HttpRouteParentRefs,
    generation: i64,
    client: &kube::Client,
) -> Option<Condition> {
    if !section_name_valid(gw, parent_ref) {
        return Some(conditions::not_accepted(
            generation,
            "NoMatchingParent",
            "no listener matches sectionName",
        ));
    }
    if !namespace_allowed(route, gw, parent_ref, client).await {
        return Some(conditions::not_accepted(
            generation,
            "NotAllowedByListeners",
            "route namespace not allowed",
        ));
    }
    if !hostnames_intersect(route, gw, parent_ref.section_name.as_deref()) {
        return Some(conditions::not_accepted(
            generation,
            "NoMatchingListenerHostname",
            "no matching listener hostname",
        ));
    }
    None
}

/// Returns `true` when the `parentRef` section name matches a listener.
fn section_name_valid(gw: &Gateway, parent_ref: &HttpRouteParentRefs) -> bool {
    parent_ref
        .section_name
        .as_ref()
        .is_none_or(|s| gw.spec.listeners.iter().any(|l| l.name == *s))
}

/// Returns `true` when the route's namespace is allowed by listeners.
async fn namespace_allowed(
    route: &HTTPRoute,
    gw: &Gateway,
    parent_ref: &HttpRouteParentRefs,
    client: &kube::Client,
) -> bool {
    let route_ns = route_status::route_namespace(route);
    let gw_ns = gw.metadata.namespace.as_deref().unwrap_or("default");
    route_allowed_by_listeners(
        route_ns,
        gw_ns,
        &gw.spec.listeners,
        parent_ref.section_name.as_deref(),
        client,
    )
    .await
}

/// Checks whether a route's namespace is allowed by at least one
/// targeted listener.
async fn route_allowed_by_listeners(
    route_ns: &str,
    gw_ns: &str,
    listeners: &[GatewayListeners],
    section_name: Option<&str>,
    client: &kube::Client,
) -> bool {
    let matching = targeted_listeners(listeners, section_name);
    for listener in &matching {
        if listener_allows_namespace(listener, route_ns, gw_ns, client).await {
            return true;
        }
    }
    false
}

/// Returns listeners targeted by a section name (or all if `None`).
fn targeted_listeners<'a>(listeners: &'a [GatewayListeners], section_name: Option<&str>) -> Vec<&'a GatewayListeners> {
    match section_name {
        Some(name) => listeners.iter().filter(|l| l.name == name).collect(),
        None => listeners.iter().collect(),
    }
}

/// Checks whether a single listener allows the given route namespace.
async fn listener_allows_namespace(
    listener: &GatewayListeners,
    route_ns: &str,
    gw_ns: &str,
    client: &kube::Client,
) -> bool {
    use gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesFrom;

    let from = listener
        .allowed_routes
        .as_ref()
        .and_then(|ar| ar.namespaces.as_ref())
        .and_then(|ns| ns.from.as_ref());

    match from {
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => route_ns == gw_ns,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
            let selector = listener
                .allowed_routes
                .as_ref()
                .and_then(|ar| ar.namespaces.as_ref())
                .and_then(|ns| ns.selector.as_ref());
            namespace_matches_label_selector(client, route_ns, selector).await
        },
    }
}

/// Checks whether a namespace's labels match a label selector.
async fn namespace_matches_label_selector(
    client: &kube::Client,
    ns_name: &str,
    selector: Option<&GatewayListenersAllowedRoutesNamespacesSelector>,
) -> bool {
    let Some(selector) = selector else { return false };
    let ns_api = Api::<Namespace>::all(client.clone());
    let Ok(ns_obj) = ns_api.get(ns_name).await else {
        return false;
    };

    let Some(match_labels) = &selector.match_labels else {
        return true;
    };
    let Some(labels) = ns_obj.metadata.labels.as_ref() else {
        return false;
    };
    match_labels
        .iter()
        .all(|(k, v)| labels.get(k).is_some_and(|lv| lv == v))
}

/// Checks if any route hostname intersects with a matching listener.
fn hostnames_intersect(route: &HTTPRoute, gw: &Gateway, section_name: Option<&str>) -> bool {
    let route_hostnames = route.spec.hostnames.as_deref().unwrap_or(&[]);
    if route_hostnames.is_empty() {
        return true;
    }

    for listener in targeted_listeners(&gw.spec.listeners, section_name) {
        let Some(listener_hostname) = listener.hostname.as_deref() else {
            return true;
        };
        if route_hostnames
            .iter()
            .any(|rh| hostname::hostname_matches(rh, listener_hostname))
        {
            return true;
        }
    }

    false
}

/// Error policy for `HTTPRoute` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub(crate) fn error_policy(_route: Arc<HTTPRoute>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    error!(%error, "HTTPRoute reconciliation failed");
    Action::requeue(Duration::from_secs(30))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::allow_attributes,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::default_trait_access,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_assert_message,
    reason = "tests"
)]
mod tests {
    use gateway_api::{gateways::GatewaySpec, httproutes::HttpRouteSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_section_name_valid_without_section_name() {
        let gw = gateway(vec![listener("http", None)]);

        assert!(
            section_name_valid(&gw, &parent_ref(None)),
            "a parentRef without a sectionName attaches to every listener"
        );
    }

    #[test]
    fn test_section_name_valid_matching_listener() {
        let gw = gateway(vec![listener("http", None), listener("https", None)]);

        assert!(
            section_name_valid(&gw, &parent_ref(Some("https"))),
            "a sectionName naming an existing listener is valid"
        );
    }

    #[test]
    fn test_section_name_valid_rejects_unknown_listener() {
        let gw = gateway(vec![listener("http", None)]);

        assert!(
            !section_name_valid(&gw, &parent_ref(Some("grpc"))),
            "a sectionName with no matching listener is invalid"
        );
    }

    #[test]
    fn test_targeted_listeners_without_section_name_returns_all() {
        let listeners = vec![listener("http", None), listener("https", None)];

        assert_eq!(
            targeted_listeners(&listeners, None).len(),
            2,
            "no sectionName targets every listener"
        );
    }

    #[test]
    fn test_targeted_listeners_filters_by_section_name() {
        let listeners = vec![listener("http", None), listener("https", None)];
        let targeted = targeted_listeners(&listeners, Some("https"));

        assert_eq!(targeted.len(), 1, "a sectionName targets exactly one listener");
        assert_eq!(targeted[0].name, "https", "the named listener should be selected");
    }

    #[test]
    fn test_targeted_listeners_unknown_section_name_returns_none() {
        let listeners = vec![listener("http", None)];

        assert!(
            targeted_listeners(&listeners, Some("grpc")).is_empty(),
            "an unknown sectionName targets no listener"
        );
    }

    #[test]
    fn test_hostnames_intersect_route_without_hostnames() {
        let gw = gateway(vec![listener("http", Some("example.com"))]);

        assert!(
            hostnames_intersect(&route(&[]), &gw, None),
            "a route without hostnames attaches to any listener"
        );
    }

    #[test]
    fn test_hostnames_intersect_unconstrained_listener() {
        let gw = gateway(vec![listener("http", None)]);

        assert!(
            hostnames_intersect(&route(&["a.example.com"]), &gw, None),
            "a listener without a hostname accepts every route hostname"
        );
    }

    #[test]
    fn test_hostnames_intersect_wildcard_listener() {
        let gw = gateway(vec![listener("http", Some("*.example.com"))]);

        assert!(
            hostnames_intersect(&route(&["a.example.com"]), &gw, None),
            "a subdomain should intersect a wildcard listener"
        );
    }

    #[test]
    fn test_hostnames_intersect_rejects_disjoint_hostnames() {
        let gw = gateway(vec![listener("http", Some("example.com"))]);

        assert!(
            !hostnames_intersect(&route(&["other.org"]), &gw, None),
            "disjoint hostnames must not attach"
        );
    }

    #[test]
    fn test_hostnames_intersect_honours_section_name() {
        let gw = gateway(vec![
            listener("http", Some("example.com")),
            listener("https", Some("other.org")),
        ]);

        assert!(
            !hostnames_intersect(&route(&["other.org"]), &gw, Some("http")),
            "only the named listener's hostname should be considered"
        );
        assert!(
            hostnames_intersect(&route(&["other.org"]), &gw, Some("https")),
            "the named listener's hostname should match"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds a Gateway in the `infra` namespace with the given listeners.
    fn gateway(listeners: Vec<GatewayListeners>) -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners,
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds an HTTP listener with an optional hostname constraint.
    fn listener(name: &str, hostname: Option<&str>) -> GatewayListeners {
        GatewayListeners {
            name: name.to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            hostname: hostname.map(str::to_owned),
            ..Default::default()
        }
    }

    /// Builds a `parentRef` with an optional `sectionName`.
    fn parent_ref(section_name: Option<&str>) -> HttpRouteParentRefs {
        HttpRouteParentRefs {
            name: "gw".to_owned(),
            section_name: section_name.map(str::to_owned),
            ..Default::default()
        }
    }

    /// Builds an `HTTPRoute` carrying the given hostnames.
    fn route(hostnames: &[&str]) -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some("apps".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(hostnames.iter().map(|h| (*h).to_owned()).collect()),
                ..Default::default()
            },
            status: None,
        }
    }
}
