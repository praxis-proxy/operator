// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `HTTPRoute` reconciler.

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

/// Reconciles an [`HTTPRoute`] by updating its parent status entries.
///
/// For each `parentRef` that targets a `Gateway` managed by this controller,
/// sets `Accepted` and `ResolvedRefs` conditions to `True`.
pub(crate) async fn reconcile(route: Arc<HTTPRoute>, ctx: Arc<Context>) -> Result<Action> {
    let ns = route_namespace(&route);
    let name = route.name_any();
    log_reconcile_start(ns, &name);

    let Some(parent_refs) = &route.spec.parent_refs else {
        log_no_parent_refs(ns, &name);
        return Ok(Action::await_change());
    };

    let generation = route.metadata.generation.unwrap_or(0);
    let parent_statuses = collect_parent_statuses(&route, parent_refs, ns, generation, &ctx).await;
    log_empty_parents(&parent_statuses, ns, &name);
    apply_route_status(&ctx.client, ns, &name, parent_statuses).await?;
    log_reconcile_done(ns, &name);
    Ok(Action::await_change())
}

/// Returns the namespace of an [`HTTPRoute`], defaulting to `"default"`.
fn route_namespace(route: &HTTPRoute) -> &str {
    route.metadata.namespace.as_deref().unwrap_or("default")
}

/// Logs reconcile start.
fn log_reconcile_start(ns: &str, name: &str) {
    info!("reconciling HTTPRoute {ns}/{name}");
}

/// Logs when no parent refs are present.
fn log_no_parent_refs(ns: &str, name: &str) {
    debug!("HTTPRoute {ns}/{name} has no parentRefs, skipping");
}

/// Logs when no matching parent Gateways were found.
fn log_empty_parents(statuses: &[serde_json::Value], ns: &str, name: &str) {
    if statuses.is_empty() {
        debug!("HTTPRoute {ns}/{name} has no matching parent Gateways");
    }
}

/// Logs reconcile completion.
fn log_reconcile_done(ns: &str, name: &str) {
    info!("HTTPRoute {ns}/{name} status updated");
}

/// Collects parent status entries for all matching `parentRefs`.
///
/// Skips refs that do not target a `Gateway` managed by this controller.
async fn collect_parent_statuses(
    route: &HTTPRoute,
    parent_refs: &[HttpRouteParentRefs],
    route_ns: &str,
    generation: i64,
    ctx: &Context,
) -> Vec<serde_json::Value> {
    let mut statuses = Vec::new();
    for parent_ref in parent_refs {
        if let Some(status) = build_parent_status(route, parent_ref, route_ns, generation, ctx).await {
            statuses.push(status);
        }
    }
    statuses
}

/// Patches the [`HTTPRoute`] status with the given parent status entries.
///
/// Uses server-side apply to merge the status update.
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

// -----------------------------------------------------------------------------
// Status Builders
// -----------------------------------------------------------------------------

/// Builds a parent status entry for a single `parentRef`.
///
/// Returns `None` when the parent is not a `Gateway` managed by this
/// controller, or when the referenced `Gateway` or `GatewayClass` cannot
/// be found.
async fn build_parent_status(
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

    let grants = list_reference_grants(ctx).await;
    let resolve_result = check_backend_refs(route, route_ns, &ctx.client, &grants).await;

    let accepted = build_accepted_condition(route, &gw, parent_ref, generation, &ctx.client).await;
    let resolved = build_resolved_condition(&resolve_result, generation);

    Some(parent_status_json(parent_ref, gw_ns, &accepted, &resolved))
}

/// Checks whether a `parentRef` targets a `Gateway` resource.
///
/// Returns `false` for non-Gateway parent refs.
fn is_gateway_parent_ref(parent_ref: &HttpRouteParentRefs) -> bool {
    let group = parent_ref.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    let kind = parent_ref.kind.as_deref().unwrap_or("Gateway");
    group == "gateway.networking.k8s.io" && kind == "Gateway"
}

/// Checks whether the `Gateway`'s `GatewayClass` is managed by this
/// controller.
///
/// Returns `false` when the class cannot be found or has a different
/// controller name.
async fn is_managed_gateway_class(gw: &Gateway, ctx: &Context) -> bool {
    let gc_name = &gw.spec.gateway_class_name;
    let gc_api = Api::<GatewayClass>::all(ctx.client.clone());
    gc_api
        .get(gc_name)
        .await
        .is_ok_and(|gc| gc.spec.controller_name == CONTROLLER_NAME)
}

/// Builds the parent status JSON value for a single parent ref.
///
/// Combines the `parentRef` identity with the evaluated conditions.
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
///
/// Returns `None` with a debug log when the `Gateway` is not found.
async fn lookup_parent_gateway(gw_name: &str, gw_ns: &str, route_ns: &str, ctx: &Context) -> Option<Gateway> {
    let gw_api = Api::<Gateway>::namespaced(ctx.client.clone(), gw_ns);
    if let Ok(gw) = gw_api.get(gw_name).await {
        return Some(gw);
    }
    debug!("Gateway {gw_ns}/{gw_name} not found for HTTPRoute in {route_ns}");
    None
}

/// Lists all [`ReferenceGrant`] resources in the cluster.
///
/// Returns an empty vec when the listing fails.
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

/// Builds the `Accepted` condition based on listener matching.
///
/// Checks section name validity, namespace allowance, and hostname
/// intersection. Returns the first failing condition found.
async fn build_accepted_condition(
    route: &HTTPRoute,
    gw: &Gateway,
    parent_ref: &HttpRouteParentRefs,
    generation: i64,
    client: &kube::Client,
) -> Condition {
    if let Some(section) = &parent_ref.section_name
        && !gw.spec.listeners.iter().any(|l| l.name == *section)
    {
        return conditions::not_accepted(generation, "NoMatchingParent", "no listener matches sectionName");
    }

    let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
    let gw_ns = gw.metadata.namespace.as_deref().unwrap_or("default");
    if !route_allowed_by_listeners(
        route_ns,
        gw_ns,
        &gw.spec.listeners,
        parent_ref.section_name.as_deref(),
        client,
    )
    .await
    {
        return conditions::not_accepted(generation, "NotAllowedByListeners", "route namespace not allowed");
    }

    if !hostnames_intersect(route, gw, parent_ref.section_name.as_deref()) {
        return conditions::not_accepted(
            generation,
            "NoMatchingListenerHostname",
            "no matching listener hostname",
        );
    }

    conditions::accepted(generation, "route accepted")
}

/// Checks whether a route's namespace is allowed by at least one
/// targeted listener's `allowedRoutes.namespaces` policy.
async fn route_allowed_by_listeners(
    route_ns: &str,
    gw_ns: &str,
    listeners: &[gateway_api::gateways::GatewayListeners],
    section_name: Option<&str>,
    client: &kube::Client,
) -> bool {
    let matching: Vec<_> = match section_name {
        Some(name) => listeners.iter().filter(|l| l.name == name).collect(),
        None => listeners.iter().collect(),
    };

    for listener in &matching {
        let from = listener
            .allowed_routes
            .as_ref()
            .and_then(|ar| ar.namespaces.as_ref())
            .and_then(|ns| ns.from.as_ref());

        let allowed = match from {
            None | Some(gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesFrom::Same) => route_ns == gw_ns,
            Some(gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
            Some(gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
                let selector = listener
                    .allowed_routes
                    .as_ref()
                    .and_then(|ar| ar.namespaces.as_ref())
                    .and_then(|ns| ns.selector.as_ref());
                namespace_matches_label_selector(client, route_ns, selector).await
            },
        };
        if allowed {
            return true;
        }
    }
    false
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

/// Reason a backend ref could not be resolved.
enum ResolveFailure {
    /// Backend ref has an unsupported group or kind.
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

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Checks if any route hostname intersects with a matching listener hostname.
///
/// Returns `true` when the route has no hostnames (wildcard), or when at
/// least one route hostname matches at least one targeted listener hostname.
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

/// Checks all backend refs in the route for validity.
///
/// Validates group/kind, cross-namespace `ReferenceGrant` authorization,
/// and Service existence.
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

/// Validates a single backend ref for group/kind, cross-namespace
/// authorization, and Service existence.
async fn validate_single_backend(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    if !is_core_service_backend(backend) {
        return Err(ResolveFailure::InvalidKind);
    }

    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    if !is_cross_ns_backend_allowed(backend, route_ns, backend_ns, grants) {
        return Err(ResolveFailure::RefNotPermitted);
    }

    let svc_api = Api::<Service>::namespaced(client.clone(), backend_ns);
    if svc_api.get(&backend.name).await.is_ok() {
        Ok(())
    } else {
        Err(ResolveFailure::BackendNotFound)
    }
}

/// Returns `true` when the backend ref targets a core `Service`.
fn is_core_service_backend(backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs) -> bool {
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    if !group.is_empty() || kind != "Service" {
        debug!(group, kind, "unsupported backend ref kind");
        return false;
    }
    true
}

/// Checks whether a cross-namespace backend ref is authorized by a
/// [`ReferenceGrant`].
///
/// Same-namespace refs are always allowed.
fn is_cross_ns_backend_allowed(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    backend_ns: &str,
    grants: &[ReferenceGrant],
) -> bool {
    if backend_ns == route_ns {
        return true;
    }
    if reference_grant::is_reference_allowed(
        route_ns,
        "gateway.networking.k8s.io",
        "HTTPRoute",
        backend_ns,
        "",
        "Service",
        Some(&backend.name),
        grants,
    ) {
        return true;
    }
    debug!(
        backend_ns,
        service = %backend.name,
        "cross-namespace backend ref not permitted by ReferenceGrant"
    );
    false
}

/// Error policy for `HTTPRoute` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub(crate) fn error_policy(_route: Arc<HTTPRoute>, error: &Error, _ctx: Arc<Context>) -> Action {
    error!(%error, "HTTPRoute reconciliation failed");
    Action::requeue(Duration::from_secs(30))
}
