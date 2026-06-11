// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Extracted helpers for Gateway reconciliation.
//!
//! Contains validation, namespace filtering, and label-selector matching
//! logic used by the main Gateway controller.

use std::collections::HashSet;

use gateway_api::{
    gateways::{
        Gateway, GatewayListeners, GatewayListenersAllowedRoutesNamespacesFrom,
        GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions,
    },
    httproutes::HTTPRoute,
};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Namespace, Service, ServicePort},
};
use kube::{Api, ResourceExt, api::PatchParams};
use serde_json::json;
use tracing::{debug, info};

use crate::{
    config::{
        cluster::build_cluster, filter_conversion::convert_filters, generate::assemble_config,
        listener::convert_listener, routing::convert_routes,
    },
    context::CONTROLLER_NAME,
    endpoints,
    error::{Error, Result},
    gateway_api::{attachment, conditions},
    resources::{configmap::build_configmap, deployment::build_deployment, labels::child_name, service::build_service},
};

// -----------------------------------------------------------------------------
// GatewayClass Validation
// -----------------------------------------------------------------------------

/// Validates that the Gateway's `GatewayClass` exists and belongs to this
/// controller.
///
/// Returns `Ok(true)` when the class is ours, `Ok(false)` when it belongs
/// to another controller (caller should skip), and `Err` on lookup failure
/// or missing class.
pub(super) async fn validate_gateway_class(client: &kube::Client, gw: &Gateway) -> Result<bool> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let gc_name = &gw.spec.gateway_class_name;

    let gc = fetch_gateway_class(client, gc_name).await?;

    if gc.spec.controller_name != CONTROLLER_NAME {
        debug!("ignoring Gateway {ns}/{name}: GatewayClass {gc_name} not ours");
        return Ok(false);
    }

    Ok(true)
}

/// Fetches a `GatewayClass` by name, mapping API errors.
async fn fetch_gateway_class(
    client: &kube::Client,
    gc_name: &str,
) -> Result<gateway_api::gatewayclasses::GatewayClass> {
    let api = Api::<gateway_api::gatewayclasses::GatewayClass>::all(client.clone());
    api.get(gc_name).await.map_err(|e| map_gc_error(e, gc_name))
}

/// Maps a `GatewayClass` lookup error to an operator error.
fn map_gc_error(e: kube::Error, gc_name: &str) -> Error {
    if is_api_not_found(&e) {
        log_gc_not_found(gc_name);
        return Error::GatewayClassNotFound(gc_name.to_owned());
    }
    log_gc_lookup_failure(&e);
    Error::Kube(e)
}

/// Returns `true` when the error is a 404 API response.
fn is_api_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(resp) if resp.code == 404)
}

/// Logs that a `GatewayClass` was not found.
fn log_gc_not_found(gc_name: &str) {
    tracing::debug!("GatewayClass {gc_name} not found");
}

/// Logs a non-404 kube error during `GatewayClass` lookup.
fn log_gc_lookup_failure(e: &kube::Error) {
    tracing::debug!(%e, "GatewayClass lookup failed");
}

// -----------------------------------------------------------------------------
// Route Collection
// -----------------------------------------------------------------------------

/// Collects `HTTPRoute` resources attached to the Gateway, filtered by
/// namespace policies.
pub(super) async fn collect_routes<'a>(
    client: &kube::Client,
    gw: &Gateway,
    all_routes: &'a [HTTPRoute],
) -> Vec<(&'a HTTPRoute, Vec<Option<String>>)> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let attached = attachment::attached_routes(&name, &ns, all_routes);
    filter_routes_by_allowed_namespaces(&attached, &gw.spec.listeners, &ns, client).await
}

// -----------------------------------------------------------------------------
// Praxis Config Generation
// -----------------------------------------------------------------------------

/// Intermediate values produced by [`build_praxis_config`].
pub(super) struct PraxisConfigOutput {
    /// Serialized YAML configuration.
    pub(super) config_yaml: String,

    /// TLS secret names referenced by HTTPS listeners (deduplicated).
    pub(super) tls_secret_names: Vec<String>,

    /// Deduplicated `(listener_name, port)` pairs.
    pub(super) listener_ports: Vec<(String, i32)>,
}

/// Converts Gateway listeners, attached routes, and resolved endpoints
/// into a complete Praxis YAML configuration string.
pub(super) async fn build_praxis_config(
    client: &kube::Client,
    listeners: &[GatewayListeners],
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    ns: &str,
) -> Result<PraxisConfigOutput> {
    let supported: Vec<_> = listeners
        .iter()
        .filter(|l| l.protocol == "HTTP" || l.protocol == "HTTPS")
        .collect();

    let listener_hostnames = build_listener_hostname_map(&supported);
    let praxis_listeners = merge_listeners_by_port(&supported);
    let grants = super::gateway::list_all_grants(client).await?;
    let (praxis_routes, backend_refs) = convert_attached_routes(attached, ns, &listener_hostnames, &grants);
    let extra_filters = collect_filters(attached);
    let clusters = resolve_clusters(client, &backend_refs).await?;
    let config = assemble_config(
        praxis_listeners,
        &praxis_routes,
        &clusters,
        &extra_filters,
        &listener_hostnames,
    )?;
    let config_yaml = serde_yaml::to_string(&config)?;

    Ok(PraxisConfigOutput {
        config_yaml,
        tls_secret_names: collect_tls_secret_names(listeners),
        listener_ports: collect_listener_ports(&supported),
    })
}

/// Merges Gateway listeners on the same port into a single Praxis
/// listener, combining TLS certificates from all listeners in the group.
fn merge_listeners_by_port(supported: &[&GatewayListeners]) -> Vec<crate::config::listener::PraxisListener> {
    let mut by_port: std::collections::BTreeMap<i32, Vec<&GatewayListeners>> = std::collections::BTreeMap::new();
    for l in supported {
        by_port.entry(l.port).or_default().push(l);
    }

    by_port
        .into_values()
        .filter_map(|group| {
            let first = group.first()?;
            let chain_name = format!("{}-chain", first.name);
            let mut listener = convert_listener(first, &chain_name);
            listener.merged_section_names = group.iter().map(|l| l.name.clone()).collect();
            merge_tls_certs(&mut listener, &group);
            Some(listener)
        })
        .collect()
}

/// Merges TLS certificates from all listeners in a port group.
fn merge_tls_certs(listener: &mut crate::config::listener::PraxisListener, group: &[&GatewayListeners]) {
    if group.len() <= 1 {
        return;
    }
    let mut all_certs: Vec<crate::config::listener::PraxisCertificate> = listener
        .tls
        .as_ref()
        .map(|t| t.certificates.clone())
        .unwrap_or_default();

    for l in group.iter().skip(1) {
        collect_listener_certs(l, &mut all_certs);
    }

    if !all_certs.is_empty() {
        listener.tls = Some(crate::config::listener::PraxisTls {
            certificates: all_certs,
        });
    }
}

/// Collects TLS certificates from a single listener into the cert list.
fn collect_listener_certs(l: &GatewayListeners, certs: &mut Vec<crate::config::listener::PraxisCertificate>) {
    let Some(tls) = &l.tls else { return };
    let Some(refs) = &tls.certificate_refs else { return };
    for cert_ref in refs {
        let (server_names, default) = match &l.hostname {
            Some(h) => (Some(vec![h.clone()]), None),
            None => (None, Some(true)),
        };
        certs.push(crate::config::listener::PraxisCertificate {
            cert_path: format!("/tls/{}/tls.crt", cert_ref.name),
            key_path: format!("/tls/{}/tls.key", cert_ref.name),
            server_names,
            default,
        });
    }
}

/// Builds a map from listener section name to its hostname constraint.
fn build_listener_hostname_map(listeners: &[&GatewayListeners]) -> std::collections::HashMap<String, Option<String>> {
    listeners.iter().map(|l| (l.name.clone(), l.hostname.clone())).collect()
}

/// Converts attached routes to Praxis routes and collects backend refs.
fn convert_attached_routes(
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    ns: &str,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
    grants: &[gateway_api::referencegrants::ReferenceGrant],
) -> (
    Vec<crate::config::routing::PraxisRoute>,
    Vec<crate::config::routing::BackendRef>,
) {
    let route_refs: Vec<_> = attached.iter().map(|(r, s)| (*r, s.clone())).collect();
    convert_routes(&route_refs, ns, listener_hostnames, grants)
}

/// Extracts and converts filters from all attached route rules.
fn collect_filters(attached: &[(&HTTPRoute, Vec<Option<String>>)]) -> Vec<crate::config::routing::PraxisFilterEntry> {
    let all_rules: Vec<_> = attached
        .iter()
        .flat_map(|(route, _)| route.spec.rules.as_deref().unwrap_or(&[]))
        .cloned()
        .collect();
    convert_filters(&all_rules)
}

/// Resolves Kubernetes endpoints for each backend ref into clusters.
///
/// Service-level weights are normalized across endpoints so that the
/// overall traffic split matches the configured backend weights
/// regardless of how many pods each service has.
async fn resolve_clusters(
    client: &kube::Client,
    backend_refs: &[crate::config::routing::BackendRef],
) -> Result<Vec<crate::config::cluster::PraxisCluster>> {
    let mut cluster_data: std::collections::BTreeMap<String, Vec<_>> = std::collections::BTreeMap::new();

    for backend in backend_refs {
        let eps = endpoints::resolve_endpoints(client, &backend.namespace, &backend.service, backend.port).await?;
        let weight = backend.weight.unwrap_or(1);
        cluster_data
            .entry(backend.cluster_name.clone())
            .or_default()
            .push((weight, eps));
    }

    let clusters = cluster_data
        .into_iter()
        .map(|(name, mut svc)| build_resolved_cluster(&name, &mut svc))
        .collect();
    Ok(clusters)
}

/// Builds a single cluster from resolved service endpoint data.
fn build_resolved_cluster(
    name: &str,
    service_data: &mut [(i32, Vec<String>)],
) -> crate::config::cluster::PraxisCluster {
    sort_service_endpoints(service_data);
    log_cluster_resolution(name, service_data);
    let (eps, weights) = distribute_service_weights(service_data);
    debug!(cluster = %name, endpoints = eps.len(), weights = ?weights, "distributed weights");
    let w = if weights.is_empty() { None } else { Some(weights) };
    build_cluster(name, eps, w)
}

/// Sorts endpoints within each service for deterministic config output.
///
/// Without sorting, endpoint IPs from `EndpointSlice` listings may
/// arrive in arbitrary order across reconciliations. This changes the
/// config YAML (and its SHA-256 hash), triggering unnecessary
/// Deployment rollouts and pod restarts.
fn sort_service_endpoints(service_data: &mut [(i32, Vec<String>)]) {
    for (_, eps) in service_data.iter_mut() {
        eps.sort();
    }
}

/// Logs per-service endpoint data for a cluster being resolved.
fn log_cluster_resolution(name: &str, service_data: &[(i32, Vec<String>)]) {
    debug!(cluster = %name, services = service_data.len(), "resolving cluster");
    log_service_entries(name, service_data);
}

/// Logs individual service entries within a cluster.
fn log_service_entries(name: &str, service_data: &[(i32, Vec<String>)]) {
    for (i, (w, eps)) in service_data.iter().enumerate() {
        debug!(cluster = %name, svc = i, weight = w, eps = eps.len(), "service data");
    }
}

/// Distributes service-level weights across endpoints.
///
/// For each service with weight `W` and `N` endpoints, assigns
/// `(W * lcm) / N` to each endpoint, where `lcm` is the least
/// common multiple of all endpoint counts. The final weights are
/// reduced by their GCD to minimise the round-robin cycle length,
/// which improves distribution accuracy for small request batches.
fn distribute_service_weights(service_data: &[(i32, Vec<String>)]) -> (Vec<String>, Vec<i32>) {
    let mut all_endpoints = Vec::new();
    let mut all_weights = Vec::new();

    let lcm_denom = service_data
        .iter()
        .filter(|(_, eps)| !eps.is_empty())
        .map(|(_, eps)| i32::try_from(eps.len()).unwrap_or(i32::MAX))
        .fold(1, lcm);

    for (service_weight, endpoints) in service_data {
        if endpoints.is_empty() {
            continue;
        }
        let n = i32::try_from(endpoints.len()).unwrap_or(i32::MAX);
        let ep_weight = (service_weight * lcm_denom) / n;
        for ep in endpoints {
            all_endpoints.push(ep.clone());
            all_weights.push(ep_weight);
        }
    }

    reduce_weights_by_gcd(&mut all_weights);
    (all_endpoints, all_weights)
}

/// Divides all positive weights by their GCD to minimise cycle length.
fn reduce_weights_by_gcd(weights: &mut [i32]) {
    let g = weights.iter().copied().filter(|w| *w > 0).fold(0, gcd);
    if g > 1 {
        for w in weights.iter_mut() {
            if *w > 0 {
                *w /= g;
            }
        }
    }
}

/// Greatest common divisor (Euclidean algorithm).
fn gcd(mut a: i32, mut b: i32) -> i32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.abs()
}

/// Least common multiple.
fn lcm(a: i32, b: i32) -> i32 {
    if a == 0 || b == 0 {
        return 0;
    }
    (a * b).abs() / gcd(a, b)
}

/// Deduplicates TLS secret names from HTTPS listeners.
fn collect_tls_secret_names(listeners: &[GatewayListeners]) -> Vec<String> {
    let mut seen = HashSet::new();
    listeners
        .iter()
        .filter(|l| l.protocol == "HTTPS")
        .filter_map(|l| l.tls.as_ref())
        .flat_map(|tls| tls.certificate_refs.as_deref().unwrap_or(&[]))
        .filter(|cert_ref| seen.insert(cert_ref.name.clone()))
        .map(|cert_ref| cert_ref.name.clone())
        .collect()
}

/// Deduplicates `(name, port)` pairs from supported listeners.
fn collect_listener_ports(listeners: &[&GatewayListeners]) -> Vec<(String, i32)> {
    let mut seen = HashSet::new();
    listeners
        .iter()
        .filter(|l| seen.insert(l.port))
        .map(|l| (l.name.clone(), l.port))
        .collect()
}

// -----------------------------------------------------------------------------
// Child Resource Application
// -----------------------------------------------------------------------------

/// Applies the `ConfigMap`, `Deployment`, and `Service` child resources
/// via SSA.
///
/// Returns the SHA-256 config hash used in the pod template annotation.
pub(super) async fn apply_child_resources(
    client: &kube::Client,
    gw: &Gateway,
    config_output: &PraxisConfigOutput,
) -> Result<String> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let child = child_name(&name);

    let cm = build_configmap(&child, &ns, gw, &config_output.config_yaml)?;
    super::gateway::apply_resource(client, &ns, &cm).await?;

    let config_hash = sha256_hex(&config_output.config_yaml);
    let deploy = build_deployment(&crate::resources::deployment::DeploymentParams {
        config_hash: &config_hash,
        name: &child,
        namespace: &ns,
        gateway: gw,
        tls_secret_names: &config_output.tls_secret_names,
        listener_ports: &config_output.listener_ports,
    })?;
    super::gateway::apply_resource(client, &ns, &deploy).await?;

    let ports = build_service_ports(&config_output.listener_ports);
    let svc = build_service(&child, &ns, gw, ports)?;
    super::gateway::apply_resource(client, &ns, &svc).await?;

    Ok(config_hash)
}

/// Converts `(name, port)` pairs into Kubernetes `ServicePort` entries.
fn build_service_ports(listener_ports: &[(String, i32)]) -> Vec<ServicePort> {
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
    listener_ports
        .iter()
        .map(|(name, port)| ServicePort {
            name: Some(name.clone()),
            port: *port,
            protocol: Some("TCP".to_owned()),
            target_port: Some(IntOrString::Int(*port)),
            ..Default::default()
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Config Hashing
// -----------------------------------------------------------------------------

/// Returns a hex-encoded SHA-256 digest of `data`.
fn sha256_hex(data: &str) -> String {
    use std::fmt::Write;
    let digest = <sha2::Sha256 as sha2::Digest>::digest(data.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// -----------------------------------------------------------------------------
// Gateway Status
// -----------------------------------------------------------------------------

/// Builds and applies the Gateway status (listener statuses + conditions).
///
/// Gates the `Programmed` condition on both Deployment readiness and
/// load-balancer address availability, per the Gateway API spec.
pub(super) async fn build_and_apply_gateway_status(
    client: &kube::Client,
    gw: &Gateway,
    listeners: &[GatewayListeners],
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
) -> Result<()> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let generation = gw.metadata.generation.unwrap_or(1);
    let child = child_name(&name);

    let addresses = resolve_lb_addresses(client, &ns, &child).await;
    let deployment_ready = is_deployment_ready(client, &ns, &child).await;
    let (listener_statuses, any_accepted, any_rejected) =
        build_listener_statuses(listeners, generation, &ns, client, attached).await;

    let data_plane_ready = deployment_ready && !addresses.is_empty();
    let accepted = gateway_accepted_condition(generation, any_accepted, any_rejected);
    let programmed = gateway_programmed_condition(generation, any_accepted, data_plane_ready);
    let status = gateway_status_json(&GatewayStatusParts {
        name: &name,
        ns: &ns,
        addresses: &addresses,
        listener_statuses: &listener_statuses,
        accepted: &accepted,
        programmed: &programmed,
    });

    apply_gateway_status(client, &ns, &name, &status).await?;
    log_gateway_reconciled(&ns, &name);
    Ok(())
}

/// Logs successful Gateway reconciliation.
fn log_gateway_reconciled(ns: &str, name: &str) {
    info!("Gateway {ns}/{name} reconciled successfully");
}

// -----------------------------------------------------------------------------
// Route Parent Status
// -----------------------------------------------------------------------------

/// Updates parent status on attached `HTTPRoutes`.
///
/// Sets `Accepted = True` and evaluates `ResolvedRefs` for each route
/// that targets this Gateway. Called by the Gateway controller **after**
/// child resources are applied and the Deployment rollout is verified,
/// so the conformance test cannot send traffic before the data plane
/// is serving the matching configuration.
pub(super) async fn update_route_parent_statuses(
    client: &kube::Client,
    gw: &Gateway,
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
) -> Result<()> {
    let gw_ns = gw.namespace().unwrap_or_default();
    let gw_name = gw.name_any();
    let grants = super::gateway::list_all_grants(client).await?;

    for (route, _) in attached {
        let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
        let route_name = route.name_any();
        let generation = route.metadata.generation.unwrap_or(0);
        let Some(parent_refs) = &route.spec.parent_refs else {
            continue;
        };

        let statuses = build_route_statuses(
            route,
            parent_refs,
            route_ns,
            &gw_name,
            &gw_ns,
            generation,
            client,
            &grants,
        )
        .await;

        if !statuses.is_empty() {
            apply_route_status(client, route_ns, &route_name, statuses).await?;
        }
    }
    Ok(())
}

/// Builds parent status entries for refs targeting this Gateway.
#[allow(clippy::too_many_arguments, reason = "route status needs full context")]
async fn build_route_statuses(
    route: &HTTPRoute,
    parent_refs: &[gateway_api::httproutes::HttpRouteParentRefs],
    route_ns: &str,
    gw_name: &str,
    gw_ns: &str,
    generation: i64,
    client: &kube::Client,
    grants: &[gateway_api::referencegrants::ReferenceGrant],
) -> Vec<serde_json::Value> {
    let mut statuses = Vec::new();
    for parent_ref in parent_refs {
        if !is_ref_targeting_gateway(parent_ref, gw_name, gw_ns, route_ns) {
            continue;
        }
        let resolved = resolve_route_backends(route, route_ns, client, grants).await;
        let accepted_cond = conditions::accepted(generation, "route accepted");
        let resolved_cond = resolved_refs_condition(&resolved, generation);
        statuses.push(route_parent_json(parent_ref, gw_ns, &accepted_cond, &resolved_cond));
    }
    statuses
}

/// Returns `true` when `parent_ref` targets the named Gateway.
fn is_ref_targeting_gateway(
    parent_ref: &gateway_api::httproutes::HttpRouteParentRefs,
    gw_name: &str,
    gw_ns: &str,
    route_ns: &str,
) -> bool {
    let group = parent_ref.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    let kind = parent_ref.kind.as_deref().unwrap_or("Gateway");
    if group != "gateway.networking.k8s.io" || kind != "Gateway" {
        return false;
    }
    let ref_ns = parent_ref.namespace.as_deref().unwrap_or(route_ns);
    parent_ref.name == gw_name && ref_ns == gw_ns
}

/// Checks all backend refs in a route for validity.
async fn resolve_route_backends(
    route: &HTTPRoute,
    route_ns: &str,
    client: &kube::Client,
    grants: &[gateway_api::referencegrants::ReferenceGrant],
) -> std::result::Result<(), RouteResolveFailure> {
    let Some(rules) = &route.spec.rules else { return Ok(()) };
    for rule in rules {
        let Some(backends) = &rule.backend_refs else { continue };
        for backend in backends {
            validate_route_backend(backend, route_ns, client, grants).await?;
        }
    }
    Ok(())
}

/// Reason a backend ref could not be resolved.
enum RouteResolveFailure {
    /// Unsupported group or kind.
    InvalidKind,

    /// Cross-namespace ref denied by `ReferenceGrant`.
    RefNotPermitted,

    /// Backend `Service` does not exist.
    BackendNotFound,
}

/// Validates a single backend ref.
async fn validate_route_backend(
    backend: &gateway_api::httproutes::HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
    grants: &[gateway_api::referencegrants::ReferenceGrant],
) -> std::result::Result<(), RouteResolveFailure> {
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    if !group.is_empty() || kind != "Service" {
        return Err(RouteResolveFailure::InvalidKind);
    }

    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    if backend_ns != route_ns
        && !crate::gateway_api::reference_grant::is_reference_allowed(
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
        return Err(RouteResolveFailure::RefNotPermitted);
    }

    let svc_api = Api::<Service>::namespaced(client.clone(), backend_ns);
    if svc_api.get(&backend.name).await.is_ok() {
        Ok(())
    } else {
        Err(RouteResolveFailure::BackendNotFound)
    }
}

/// Builds the `ResolvedRefs` condition from a resolution result.
fn resolved_refs_condition(
    result: &std::result::Result<(), RouteResolveFailure>,
    generation: i64,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    match result {
        Ok(()) => conditions::resolved_refs(generation, "all backend refs resolved"),
        Err(RouteResolveFailure::InvalidKind) => {
            conditions::unresolved_refs(generation, "InvalidKind", "unsupported backend ref kind")
        },
        Err(RouteResolveFailure::RefNotPermitted) => conditions::unresolved_refs(
            generation,
            "RefNotPermitted",
            "cross-namespace backend ref not permitted",
        ),
        Err(RouteResolveFailure::BackendNotFound) => {
            conditions::unresolved_refs(generation, "BackendNotFound", "backend service not found")
        },
    }
}

/// Builds a route parent status JSON entry.
fn route_parent_json(
    parent_ref: &gateway_api::httproutes::HttpRouteParentRefs,
    gw_ns: &str,
    accepted: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition,
    resolved: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition,
) -> serde_json::Value {
    let mut ref_json = json!({
        "group": "gateway.networking.k8s.io",
        "kind": "Gateway",
        "name": parent_ref.name,
        "namespace": gw_ns,
    });
    if let Some(section) = &parent_ref.section_name
        && let Some(obj) = ref_json.as_object_mut()
    {
        obj.insert("sectionName".to_owned(), json!(section));
    }
    json!({
        "parentRef": ref_json,
        "controllerName": CONTROLLER_NAME,
        "conditions": [accepted, resolved],
    })
}

/// Patches an [`HTTPRoute`]'s status via server-side apply.
async fn apply_route_status(
    client: &kube::Client,
    ns: &str,
    name: &str,
    parent_statuses: Vec<serde_json::Value>,
) -> Result<()> {
    let status = json!({
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
            &kube::api::Patch::Apply(&status),
        )
        .await?;
    Ok(())
}

/// Components used to build the Gateway status JSON payload.
struct GatewayStatusParts<'a> {
    /// Gateway name.
    name: &'a str,

    /// Gateway namespace.
    ns: &'a str,

    /// Load-balancer addresses.
    addresses: &'a [serde_json::Value],

    /// Per-listener status entries.
    listener_statuses: &'a [serde_json::Value],

    /// Gateway-level `Accepted` condition.
    accepted: &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition,

    /// Gateway-level `Programmed` condition.
    programmed: &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition,
}

/// Constructs the Gateway status JSON payload.
fn gateway_status_json(parts: &GatewayStatusParts<'_>) -> serde_json::Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": { "name": parts.name, "namespace": parts.ns },
        "status": {
            "addresses": parts.addresses,
            "conditions": [parts.accepted, parts.programmed],
            "listeners": parts.listener_statuses,
        },
    })
}

/// Patches the Gateway status via server-side apply.
async fn apply_gateway_status(client: &kube::Client, ns: &str, name: &str, status: &serde_json::Value) -> Result<()> {
    let gw_api = Api::<Gateway>::namespaced(client.clone(), ns);
    gw_api
        .patch_status(
            name,
            &PatchParams::apply("praxis-operator").force(),
            &kube::api::Patch::Apply(status),
        )
        .await?;
    Ok(())
}

/// Queries the child Service for load-balancer ingress IP addresses.
async fn resolve_lb_addresses(client: &kube::Client, ns: &str, child: &str) -> Vec<serde_json::Value> {
    Api::<Service>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|svc| svc.status)
        .and_then(|s| s.load_balancer)
        .and_then(|lb| lb.ingress)
        .map(|ingress| {
            ingress
                .iter()
                .filter_map(|i| i.ip.as_ref().map(|ip| json!({ "type": "IPAddress", "value": ip })))
                .collect()
        })
        .unwrap_or_default()
}

/// Checks whether the child Deployment has at least one ready replica.
///
/// Used for the Gateway `Programmed` condition, which reflects whether
/// the data plane can serve traffic at all (even with a stale config).
async fn is_deployment_ready(client: &kube::Client, ns: &str, child: &str) -> bool {
    Api::<Deployment>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|d| d.status)
        .is_some_and(|s| s.ready_replicas.unwrap_or(0) > 0)
}

/// Reads the current config hash from the Deployment's pod template.
///
/// Returns `None` if the Deployment doesn't exist or has no hash.
pub(super) async fn current_deployment_hash(client: &kube::Client, ns: &str, child: &str) -> Option<String> {
    Api::<Deployment>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|d| {
            d.spec?
                .template
                .metadata?
                .annotations?
                .get("praxis.sh/config-hash")
                .cloned()
        })
}

/// Returns `true` when the Deployment's rollout is complete.
///
/// Uses the `Progressing` condition reason `NewReplicaSetAvailable`,
/// which the deployment controller sets only after the new
/// `ReplicaSet` has all desired pods ready. This is immune to
/// stale-status races in back-to-back reconciliations.
pub(super) async fn is_deployment_rolled_out(client: &kube::Client, ns: &str, child: &str) -> bool {
    let Ok(d) = Api::<Deployment>::namespaced(client.clone(), ns).get(child).await else {
        return false;
    };
    let generation = d.metadata.generation.unwrap_or(0);
    let Some(status) = d.status.as_ref() else {
        return false;
    };
    if status.observed_generation.unwrap_or(0) < generation {
        return false;
    }
    is_new_rs_available(status)
}

/// Returns `true` when the `Progressing` condition has reason
/// `NewReplicaSetAvailable`.
fn is_new_rs_available(status: &k8s_openapi::api::apps::v1::DeploymentStatus) -> bool {
    status
        .conditions
        .as_ref()
        .and_then(|c| c.iter().find(|c| c.type_ == "Progressing"))
        .is_some_and(|c| c.status == "True" && c.reason.as_deref() == Some("NewReplicaSetAvailable"))
}

/// Builds per-listener status entries.
///
/// Returns `(statuses, any_accepted, any_rejected)`.
async fn build_listener_statuses(
    listeners: &[GatewayListeners],
    generation: i64,
    gateway_ns: &str,
    client: &kube::Client,
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
) -> (Vec<serde_json::Value>, bool, bool) {
    let mut statuses = Vec::new();
    let mut any_accepted = false;
    let mut any_rejected = false;

    for l in listeners {
        let protocol_supported = l.protocol == "HTTP" || l.protocol == "HTTPS";

        if !protocol_supported {
            any_rejected = true;
            statuses.push(unsupported_listener_status(l, generation));
            continue;
        }

        any_accepted = true;
        let count = count_attached_routes(attached, l);
        let status = accepted_listener_status(l, generation, gateway_ns, client, count).await;
        statuses.push(status);
    }

    (statuses, any_accepted, any_rejected)
}

/// Builds a status entry for an unsupported-protocol listener.
fn unsupported_listener_status(l: &GatewayListeners, generation: i64) -> serde_json::Value {
    json!({
        "name": l.name,
        "attachedRoutes": 0,
        "supportedKinds": [],
        "conditions": [
            conditions::not_accepted(
                generation,
                "UnsupportedProtocol",
                "protocol not supported",
            ),
            conditions::not_programmed(
                generation, "Invalid", "unsupported protocol",
            ),
        ],
    })
}

/// Counts routes attached to a specific listener.
fn count_attached_routes(attached: &[(&HTTPRoute, Vec<Option<String>>)], listener: &GatewayListeners) -> usize {
    attached
        .iter()
        .filter(|(route, sections)| {
            let section_matches = sections
                .iter()
                .any(|s| s.is_none() || s.as_deref() == Some(&listener.name));
            if !section_matches {
                return false;
            }
            let route_hostnames = route.spec.hostnames.as_deref().unwrap_or(&[]);
            if route_hostnames.is_empty() {
                return true;
            }
            match &listener.hostname {
                None => true,
                Some(lh) => route_hostnames
                    .iter()
                    .any(|rh| crate::gateway_api::hostname::hostname_matches(rh, lh)),
            }
        })
        .count()
}

/// Builds a status entry for an accepted listener.
async fn accepted_listener_status(
    l: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    client: &kube::Client,
    count: usize,
) -> serde_json::Value {
    let (supported_kinds, resolved_refs_condition) = listener_resolved_refs(l, generation, gateway_ns, client).await;

    let refs_resolved = resolved_refs_condition.status == "True";
    let programmed_condition = if refs_resolved {
        conditions::programmed(generation, "listener programmed")
    } else {
        conditions::not_programmed(generation, "Invalid", "listener has unresolved refs")
    };

    json!({
        "name": l.name,
        "attachedRoutes": count,
        "supportedKinds": supported_kinds,
        "conditions": [
            conditions::accepted(generation, "listener accepted"),
            programmed_condition,
            conditions::no_conflicts(generation),
            resolved_refs_condition,
        ],
    })
}

/// Returns the `Accepted` condition for the Gateway.
fn gateway_accepted_condition(
    generation: i64,
    any_accepted: bool,
    any_rejected: bool,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    if !any_accepted {
        conditions::not_accepted(
            generation,
            "ListenersNotValid",
            "no listeners have a supported protocol",
        )
    } else if any_rejected {
        conditions::make_condition(
            "Accepted",
            "True",
            "ListenersNotValid",
            "some listeners are invalid",
            generation,
        )
    } else {
        conditions::accepted(generation, "Gateway accepted")
    }
}

/// Returns the `Programmed` condition for the Gateway.
///
/// Requires accepted listeners, a ready Deployment, and at least one
/// load-balancer address before reporting `True`.
fn gateway_programmed_condition(
    generation: i64,
    any_accepted: bool,
    data_plane_ready: bool,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    if !any_accepted {
        return conditions::not_programmed(generation, "Invalid", "no valid listeners");
    }
    if !data_plane_ready {
        return conditions::not_programmed(generation, "Pending", "data plane not ready");
    }
    conditions::programmed(generation, "Data plane ready")
}

// -----------------------------------------------------------------------------
// Listener Validation
// -----------------------------------------------------------------------------

/// Determines `supportedKinds` and `ResolvedRefs` for a listener.
///
/// Checks `allowedRoutes.kinds` for unsupported route kinds and validates
/// TLS certificate refs (group, kind, existence, format).
async fn listener_resolved_refs(
    listener: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    client: &kube::Client,
) -> (
    Vec<serde_json::Value>,
    k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition,
) {
    let (supported, kinds_invalid) = validate_route_kinds(listener);

    if kinds_invalid {
        return (
            supported,
            conditions::unresolved_refs(generation, "InvalidRouteKinds", "unsupported route kinds specified"),
        );
    }

    if let Some(condition) = validate_tls_cert_refs(listener, generation, gateway_ns, client).await {
        return (supported, condition);
    }

    (supported, conditions::resolved_refs(generation, "all refs resolved"))
}

/// Validates the configured `allowedRoutes.kinds` on a listener.
///
/// Returns `(supported_kinds_json, has_invalid_kinds)`.
fn validate_route_kinds(listener: &GatewayListeners) -> (Vec<serde_json::Value>, bool) {
    let configured = listener.allowed_routes.as_ref().and_then(|ar| ar.kinds.as_ref());
    let Some(kinds) = configured else {
        return (httproute_supported_kinds(), false);
    };

    let has_httproute = kinds.iter().any(is_httproute_kind);
    let has_unsupported = kinds.iter().any(|k| !is_httproute_kind(k));
    let supported = if has_httproute {
        httproute_supported_kinds()
    } else {
        Vec::new()
    };
    (supported, has_unsupported)
}

/// Returns the default `supportedKinds` JSON for `HTTPRoute`.
fn httproute_supported_kinds() -> Vec<serde_json::Value> {
    vec![json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})]
}

/// Checks whether a route kind ref is `HTTPRoute` in the Gateway API group.
fn is_httproute_kind(k: &gateway_api::gateways::GatewayListenersAllowedRoutesKinds) -> bool {
    let group = k.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    group == "gateway.networking.k8s.io" && k.kind == "HTTPRoute"
}

/// Validates TLS certificate refs on a listener.
///
/// Returns `Some(condition)` on the first validation failure, `None` when
/// all refs are valid.
async fn validate_tls_cert_refs(
    listener: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    client: &kube::Client,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    let cert_refs = listener.tls.as_ref()?.certificate_refs.as_ref()?;

    for cert_ref in cert_refs {
        if !is_secret_cert_ref(cert_ref) {
            return Some(conditions::unresolved_refs(
                generation,
                "InvalidCertificateRef",
                "unsupported certificate ref",
            ));
        }
        let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gateway_ns);
        if let Some(c) = check_cross_ns_grant(client, generation, gateway_ns, secret_ns, &cert_ref.name).await {
            return Some(c);
        }
        if let Some(c) = check_secret_contents(client, generation, secret_ns, &cert_ref.name).await {
            return Some(c);
        }
    }
    None
}

/// Returns `true` when the cert ref points to a core `Secret`.
fn is_secret_cert_ref(cert_ref: &gateway_api::gateways::GatewayListenersTlsCertificateRefs) -> bool {
    let group = cert_ref.group.as_deref().unwrap_or("");
    let kind = cert_ref.kind.as_deref().unwrap_or("Secret");
    group.is_empty() && kind == "Secret"
}

/// Checks cross-namespace `ReferenceGrant` authorization for a TLS secret.
///
/// Returns `Some(condition)` when the reference is denied, `None` when
/// allowed or same-namespace.
async fn check_cross_ns_grant(
    client: &kube::Client,
    generation: i64,
    gateway_ns: &str,
    secret_ns: &str,
    secret_name: &str,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    if secret_ns == gateway_ns {
        return None;
    }

    let Ok(grants) = list_reference_grants(client, secret_ns).await else {
        return Some(conditions::unresolved_refs(
            generation,
            "RefNotPermitted",
            "cannot verify cross-namespace grant",
        ));
    };

    if is_secret_ref_granted(gateway_ns, secret_ns, secret_name, &grants) {
        return None;
    }

    Some(conditions::unresolved_refs(
        generation,
        "RefNotPermitted",
        "cross-namespace secret reference requires a valid ReferenceGrant",
    ))
}

/// Lists `ReferenceGrant` resources in the given namespace.
async fn list_reference_grants(
    client: &kube::Client,
    ns: &str,
) -> std::result::Result<Vec<gateway_api::referencegrants::ReferenceGrant>, kube::Error> {
    let api = Api::<gateway_api::referencegrants::ReferenceGrant>::namespaced(client.clone(), ns);
    let list = api.list(&kube::api::ListParams::default()).await?;
    Ok(list.items)
}

/// Checks whether a Gateway-to-Secret cross-namespace ref is allowed.
fn is_secret_ref_granted(
    gateway_ns: &str,
    secret_ns: &str,
    secret_name: &str,
    grants: &[gateway_api::referencegrants::ReferenceGrant],
) -> bool {
    crate::gateway_api::reference_grant::is_reference_allowed(
        gateway_ns,
        "gateway.networking.k8s.io",
        "Gateway",
        secret_ns,
        "",
        "Secret",
        Some(secret_name),
        grants,
    )
}

/// Validates that a TLS Secret exists and contains valid PEM data.
///
/// Returns `Some(condition)` on failure, `None` when the secret is valid.
async fn check_secret_contents(
    client: &kube::Client,
    generation: i64,
    secret_ns: &str,
    secret_name: &str,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    let secret_api = Api::<k8s_openapi::api::core::v1::Secret>::namespaced(client.clone(), secret_ns);

    let Ok(secret) = secret_api.get(secret_name).await else {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "secret not found",
        ));
    };
    validate_tls_secret_data(secret.data.as_ref(), generation)
}

/// Validates that a Secret's data contains well-formed TLS PEM entries.
fn validate_tls_secret_data(
    data: Option<&std::collections::BTreeMap<String, k8s_openapi::ByteString>>,
    generation: i64,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    let has_keys = data.is_some_and(|d| d.contains_key("tls.crt") && d.contains_key("tls.key"));
    if !has_keys {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "malformed secret",
        ));
    }

    let is_pem = data.is_some_and(|d| is_pem_entry(d, "tls.crt") && is_pem_entry(d, "tls.key"));
    if !is_pem {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "invalid PEM data",
        ));
    }

    None
}

/// Checks whether a Secret data entry starts with a PEM header.
fn is_pem_entry(data: &std::collections::BTreeMap<String, k8s_openapi::ByteString>, key: &str) -> bool {
    data.get(key)
        .is_some_and(|v| String::from_utf8_lossy(&v.0).starts_with("-----BEGIN "))
}

// -----------------------------------------------------------------------------
// Namespace Filtering
// -----------------------------------------------------------------------------

/// Filters attached routes by the `allowedRoutes.namespaces` policy on
/// each listener.
///
/// A route is retained if at least one listener it targets allows its
/// namespace. The default policy (when unspecified) is `Same`.
async fn filter_routes_by_allowed_namespaces<'a>(
    attached: &[(&'a HTTPRoute, Vec<Option<String>>)],
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    client: &kube::Client,
) -> Vec<(&'a HTTPRoute, Vec<Option<String>>)> {
    let all_namespaces = fetch_all_namespaces(client).await;

    attached
        .iter()
        .filter(|(route, section_names)| {
            route_allowed_by_any_listener(route, section_names, listeners, gateway_ns, all_namespaces.as_ref())
        })
        .cloned()
        .collect()
}

/// Fetches all namespaces from the cluster, returning `None` on error.
async fn fetch_all_namespaces(client: &kube::Client) -> Option<kube::api::ObjectList<Namespace>> {
    match Api::<Namespace>::all(client.clone())
        .list(&kube::api::ListParams::default())
        .await
    {
        Ok(list) => Some(list),
        Err(e) => {
            tracing::warn!(
                %e, "failed to list namespaces for route filtering"
            );
            None
        },
    }
}

/// Checks whether a route is allowed by at least one targeted listener.
fn route_allowed_by_any_listener(
    route: &HTTPRoute,
    section_names: &[Option<String>],
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    all_namespaces: Option<&kube::api::ObjectList<Namespace>>,
) -> bool {
    let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
    section_names.iter().any(|section| {
        let matching: Vec<&GatewayListeners> = match section {
            Some(name) => listeners.iter().filter(|l| l.name == *name).collect(),
            None => listeners.iter().collect(),
        };
        matching
            .iter()
            .any(|listener| is_namespace_allowed(listener, route_ns, gateway_ns, all_namespaces))
    })
}

/// Checks whether a route namespace is allowed by a listener's policy.
///
/// Defaults to `Same` when `allowedRoutes` is unspecified.
fn is_namespace_allowed(
    listener: &GatewayListeners,
    route_ns: &str,
    gateway_ns: &str,
    all_namespaces: Option<&kube::api::ObjectList<Namespace>>,
) -> bool {
    let from = listener
        .allowed_routes
        .as_ref()
        .and_then(|ar| ar.namespaces.as_ref())
        .and_then(|ns| ns.from.as_ref());

    match from {
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => route_ns == gateway_ns,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
            namespace_matches_selector(listener, route_ns, all_namespaces)
        },
    }
}

/// Checks whether a route namespace matches the listener's label selector.
fn namespace_matches_selector(
    listener: &GatewayListeners,
    route_ns: &str,
    all_namespaces: Option<&kube::api::ObjectList<Namespace>>,
) -> bool {
    let selector = listener
        .allowed_routes
        .as_ref()
        .and_then(|ar| ar.namespaces.as_ref())
        .and_then(|ns| ns.selector.as_ref());

    let Some(selector) = selector else {
        return false;
    };
    let Some(all_ns) = all_namespaces else {
        return false;
    };

    all_ns.items.iter().any(|ns_obj| {
        let ns_name = ns_obj.metadata.name.as_deref().unwrap_or("");
        ns_name == route_ns && matches_label_selector(ns_obj, selector)
    })
}

/// Checks whether a namespace's labels satisfy a label selector.
///
/// Evaluates both `matchLabels` and `matchExpressions`.
fn matches_label_selector(
    ns_obj: &Namespace,
    selector: &gateway_api::gateways::GatewayListenersAllowedRoutesNamespacesSelector,
) -> bool {
    let ns_labels = ns_obj.metadata.labels.as_ref();

    if let Some(match_labels) = &selector.match_labels {
        let Some(labels) = ns_labels else {
            return false;
        };
        if !match_labels
            .iter()
            .all(|(k, v)| labels.get(k).is_some_and(|lv| lv == v))
        {
            return false;
        }
    }

    if let Some(expressions) = &selector.match_expressions {
        let labels = ns_labels.cloned().unwrap_or_default();
        for expr in expressions {
            if !evaluate_match_expression(expr, &labels) {
                return false;
            }
        }
    }

    true
}

/// Evaluates a single label-selector match expression against a label set.
fn evaluate_match_expression(
    expr: &GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions,
    labels: &std::collections::BTreeMap<String, String>,
) -> bool {
    let key = &expr.key;
    let op = expr.operator.as_str();
    let values = expr.values.as_deref().unwrap_or(&[]);
    let has_key = labels.contains_key(key);
    let label_val = labels.get(key).map(String::as_str);

    match op {
        "In" => label_val.is_some_and(|v| values.iter().any(|ev| ev == v)),
        "NotIn" => label_val.is_none_or(|v| !values.iter().any(|ev| ev == v)),
        "Exists" => has_key,
        "DoesNotExist" => !has_key,
        _ => false,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
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
    use super::*;

    #[test]
    fn test_gateway_programmed_all_ready() {
        let cond = gateway_programmed_condition(1, true, true);
        assert_eq!(cond.type_, "Programmed", "type should be Programmed");
        assert_eq!(cond.status, "True", "should be True when all ready");
        assert_eq!(cond.reason, "Programmed", "reason should be Programmed");
        assert_eq!(cond.observed_generation, Some(1), "generation should match");
    }

    #[test]
    fn test_gateway_programmed_no_accepted_listeners() {
        let cond = gateway_programmed_condition(2, false, false);
        assert_eq!(cond.status, "False", "should be False without accepted listeners");
        assert_eq!(cond.reason, "Invalid", "reason should be Invalid");
    }

    #[test]
    fn test_gateway_programmed_deployment_not_ready() {
        let cond = gateway_programmed_condition(3, true, false);
        assert_eq!(cond.status, "False", "should be False when data plane not ready");
        assert_eq!(cond.reason, "Pending", "reason should be Pending");
    }

    #[test]
    fn test_gateway_programmed_invalid_takes_precedence() {
        let cond = gateway_programmed_condition(4, false, true);
        assert_eq!(cond.status, "False", "should be False without accepted listeners");
        assert_eq!(
            cond.reason, "Invalid",
            "Invalid should take precedence over data plane readiness"
        );
    }

    #[test]
    fn test_gateway_accepted_all_valid() {
        let cond = gateway_accepted_condition(1, true, false);
        assert_eq!(cond.type_, "Accepted", "type should be Accepted");
        assert_eq!(cond.status, "True", "should be True when all accepted");
        assert_eq!(cond.reason, "Accepted", "reason should be Accepted");
    }

    #[test]
    fn test_gateway_accepted_none_valid() {
        let cond = gateway_accepted_condition(1, false, true);
        assert_eq!(cond.status, "False", "should be False with no accepted listeners");
    }

    #[test]
    fn test_gateway_accepted_mixed_listeners() {
        let cond = gateway_accepted_condition(1, true, true);
        assert_eq!(cond.status, "True", "should be True when some listeners are accepted");
        assert_eq!(
            cond.reason, "ListenersNotValid",
            "reason should indicate some listeners are invalid"
        );
    }
}
