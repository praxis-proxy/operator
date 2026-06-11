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
    gateways::Gateway,
    httproutes::{HTTPRoute, HttpRouteParentRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{api::core::v1::Service, apimachinery::pkg::apis::meta::v1::Condition};
use kube::{
    Api, ResourceExt,
    api::{Patch, PatchParams},
    runtime::controller::Action,
};
use tracing::{debug, error, info, warn};

use crate::{
    context::{CONTROLLER_NAME, Context},
    error::{Error, Result},
    gateway_api::{conditions, reference_grant},
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
    let ns = route_namespace(&route);
    let name = route.name_any();
    info!("reconciling HTTPRoute {ns}/{name}");

    if let Some(refs) = &route.spec.parent_refs {
        return apply_rejections(&route, refs, ns, &name, &ctx).await;
    }
    log_no_parent_refs(ns, &name);
    Ok(Action::await_change())
}

/// Logs that an `HTTPRoute` has no parent refs and will be skipped.
fn log_no_parent_refs(ns: &str, name: &str) {
    debug!("HTTPRoute {ns}/{name} has no parentRefs, skipping");
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

    apply_route_status(&ctx.client, ns, name, statuses).await?;
    info!("HTTPRoute {ns}/{name} rejection status applied");
    Ok(Action::await_change())
}

/// Returns the namespace of an [`HTTPRoute`], defaulting to `"default"`.
fn route_namespace(route: &HTTPRoute) -> &str {
    route.metadata.namespace.as_deref().unwrap_or("default")
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
) -> Vec<serde_json::Value> {
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
) -> Option<serde_json::Value> {
    if !is_gateway_parent_ref(parent_ref) {
        return None;
    }

    let gw_ns = parent_ref.namespace.as_deref().unwrap_or(route_ns);
    let gw = lookup_parent_gateway(&parent_ref.name, gw_ns, route_ns, ctx).await?;

    if !is_managed_gateway_class(&gw, ctx).await {
        return None;
    }

    let rejection = validate_listener_attachment(route, &gw, parent_ref, generation, &ctx.client).await;
    let grants = list_reference_grants(ctx).await;
    let resolve_result = check_backend_refs(route, route_ns, &ctx.client, &grants).await;
    let resolved = build_resolved_condition(&resolve_result, generation);

    match rejection {
        Some(not_accepted) => Some(parent_status_json(parent_ref, gw_ns, &not_accepted, &resolved)),
        None => {
            if resolve_result.is_err() {
                let accepted = conditions::accepted(generation, "route accepted");
                Some(parent_status_json(parent_ref, gw_ns, &accepted, &resolved))
            } else {
                None
            }
        },
    }
}

/// Checks whether a `parentRef` targets a `Gateway` resource.
fn is_gateway_parent_ref(parent_ref: &HttpRouteParentRefs) -> bool {
    let group = parent_ref.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    let kind = parent_ref.kind.as_deref().unwrap_or("Gateway");
    group == "gateway.networking.k8s.io" && kind == "Gateway"
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

/// Builds the parent status JSON value for a single parent ref.
fn parent_status_json(
    parent_ref: &HttpRouteParentRefs,
    gw_ns: &str,
    accepted: &Condition,
    resolved: &Condition,
) -> serde_json::Value {
    let mut parent_ref_json = serde_json::json!({
        "group": "gateway.networking.k8s.io",
        "kind": "Gateway",
        "name": parent_ref.name,
        "namespace": gw_ns,
    });
    if let Some(section) = &parent_ref.section_name
        && let Some(obj) = parent_ref_json.as_object_mut()
    {
        obj.insert("sectionName".to_owned(), serde_json::json!(section));
    }

    serde_json::json!({
        "parentRef": parent_ref_json,
        "controllerName": CONTROLLER_NAME,
        "conditions": [accepted, resolved],
    })
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
    match grant_api.list(&kube::api::ListParams::default()).await {
        Ok(list) => list.items,
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
    let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
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
    listeners: &[gateway_api::gateways::GatewayListeners],
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
fn targeted_listeners<'a>(
    listeners: &'a [gateway_api::gateways::GatewayListeners],
    section_name: Option<&str>,
) -> Vec<&'a gateway_api::gateways::GatewayListeners> {
    match section_name {
        Some(name) => listeners.iter().filter(|l| l.name == name).collect(),
        None => listeners.iter().collect(),
    }
}

/// Checks whether a single listener allows the given route namespace.
async fn listener_allows_namespace(
    listener: &gateway_api::gateways::GatewayListeners,
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
    selector: Option<&gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesSelector>,
) -> bool {
    use k8s_openapi::api::core::v1::Namespace;

    let Some(selector) = selector else { return false };
    let ns_api = Api::<Namespace>::all(client.clone());
    let Ok(ns_obj) = ns_api.get(ns_name).await else {
        return false;
    };
    let ns_labels = ns_obj.metadata.labels.as_ref();

    if let Some(match_labels) = &selector.match_labels {
        let Some(labels) = ns_labels else { return false };
        if !match_labels
            .iter()
            .all(|(k, v)| labels.get(k).is_some_and(|lv| lv == v))
        {
            return false;
        }
    }

    true
}

/// Checks if any route hostname intersects with a matching listener.
fn hostnames_intersect(route: &HTTPRoute, gw: &Gateway, section_name: Option<&str>) -> bool {
    let route_hostnames = route.spec.hostnames.as_deref().unwrap_or(&[]);
    if route_hostnames.is_empty() {
        return true;
    }

    let matching_listeners: Vec<_> = match section_name {
        Some(name) => gw.spec.listeners.iter().filter(|l| l.name == name).collect(),
        None => gw.spec.listeners.iter().collect(),
    };

    for listener in &matching_listeners {
        let listener_hostname = match &listener.hostname {
            Some(h) => h.as_str(),
            None => return true,
        };
        for route_hostname in route_hostnames {
            if crate::gateway_api::hostname::hostname_matches(route_hostname, listener_hostname) {
                return true;
            }
        }
    }

    false
}

// -----------------------------------------------------------------------------
// Backend Validation
// -----------------------------------------------------------------------------

/// Reason a backend ref could not be resolved.
enum ResolveFailure {
    /// Unsupported group or kind.
    InvalidKind,

    /// Cross-namespace ref denied by `ReferenceGrant`.
    RefNotPermitted,

    /// Backend `Service` does not exist.
    BackendNotFound,
}

/// Result of checking all backend refs in a route.
type ResolveResult = std::result::Result<(), ResolveFailure>;

/// Builds the `ResolvedRefs` condition from a resolution result.
fn build_resolved_condition(result: &ResolveResult, generation: i64) -> Condition {
    match result {
        Ok(()) => conditions::resolved_refs(generation, "all backend refs resolved"),
        Err(ResolveFailure::InvalidKind) => {
            conditions::unresolved_refs(generation, "InvalidKind", "unsupported backend ref kind")
        },
        Err(ResolveFailure::RefNotPermitted) => conditions::unresolved_refs(
            generation,
            "RefNotPermitted",
            "cross-namespace backend ref not permitted",
        ),
        Err(ResolveFailure::BackendNotFound) => {
            conditions::unresolved_refs(generation, "BackendNotFound", "backend service not found")
        },
    }
}

/// Checks all backend refs in the route for validity.
async fn check_backend_refs(
    route: &HTTPRoute,
    route_ns: &str,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    let Some(rules) = &route.spec.rules else { return Ok(()) };
    for rule in rules {
        let Some(backends) = &rule.backend_refs else { continue };
        for backend in backends {
            validate_single_backend(backend, route_ns, client, grants).await?;
        }
    }
    Ok(())
}

/// Validates a single backend ref.
async fn validate_single_backend(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    validate_backend_kind(backend)?;
    validate_cross_namespace(backend, route_ns, grants)?;
    validate_service_exists(backend, route_ns, client).await
}

/// Rejects backend refs that are not `core/Service`.
fn validate_backend_kind(backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs) -> ResolveResult {
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    if !group.is_empty() || kind != "Service" {
        debug!(group, kind, "unsupported backend ref kind");
        return Err(ResolveFailure::InvalidKind);
    }
    Ok(())
}

/// Rejects cross-namespace refs not covered by a [`ReferenceGrant`].
fn validate_cross_namespace(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    if backend_ns != route_ns
        && !reference_grant::is_reference_allowed(
            route_ns,
            "gateway.networking.k8s.io",
            "HTTPRoute",
            backend_ns,
            "",
            "Service",
            Some(&backend.name),
            grants,
        )
    {
        debug!(
            backend_ns,
            service = %backend.name,
            "cross-namespace backend ref not permitted by ReferenceGrant"
        );
        return Err(ResolveFailure::RefNotPermitted);
    }
    Ok(())
}

/// Verifies the referenced `Service` exists in the cluster.
async fn validate_service_exists(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
) -> ResolveResult {
    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    let svc_api = Api::<Service>::namespaced(client.clone(), backend_ns);
    if svc_api.get(&backend.name).await.is_ok() {
        Ok(())
    } else {
        Err(ResolveFailure::BackendNotFound)
    }
}

// -----------------------------------------------------------------------------
// Route Status
// -----------------------------------------------------------------------------

/// Patches the [`HTTPRoute`] status via server-side apply.
async fn apply_route_status(
    client: &kube::Client,
    ns: &str,
    name: &str,
    parent_statuses: Vec<serde_json::Value>,
) -> Result<()> {
    let status = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": { "name": name, "namespace": ns },
        "status": { "parents": parent_statuses },
    });

    let route_api = Api::<HTTPRoute>::namespaced(client.clone(), ns);
    route_api
        .patch_status(
            name,
            &PatchParams::apply("praxis-operator").force(),
            &Patch::Apply(&status),
        )
        .await?;

    Ok(())
}

/// Error policy for `HTTPRoute` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub(crate) fn error_policy(_route: Arc<HTTPRoute>, error: &Error, _ctx: Arc<Context>) -> Action {
    error!(%error, "HTTPRoute reconciliation failed");
    Action::requeue(Duration::from_secs(30))
}
