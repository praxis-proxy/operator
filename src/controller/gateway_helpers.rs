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
use k8s_openapi::api::core::v1::{Namespace, Service, ServicePort};
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

    let mut clusters = Vec::new();
    for (name, service_data) in cluster_data {
        let (all_eps, all_weights) = distribute_service_weights(&service_data);
        let weights = if all_weights.is_empty() {
            None
        } else {
            Some(all_weights)
        };
        clusters.push(build_cluster(&name, all_eps, weights));
    }
    Ok(clusters)
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
pub(super) async fn apply_child_resources(
    client: &kube::Client,
    gw: &Gateway,
    config_output: &PraxisConfigOutput,
) -> Result<()> {
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

    Ok(())
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
pub(super) async fn build_and_apply_gateway_status(
    client: &kube::Client,
    gw: &Gateway,
    listeners: &[GatewayListeners],
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
) -> Result<()> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let generation = gw.metadata.generation.unwrap_or(1);

    let addresses = resolve_lb_addresses(client, &ns, &child_name(&name)).await;
    let listener_result = build_listener_statuses(listeners, generation, &ns, client, attached).await;
    let status = build_gateway_conditions(&name, &ns, &addresses, &listener_result, generation);

    apply_gateway_status(client, &ns, &name, &status).await?;
    log_gateway_reconciled(&ns, &name);
    Ok(())
}

/// Logs successful Gateway reconciliation.
fn log_gateway_reconciled(ns: &str, name: &str) {
    info!("Gateway {ns}/{name} reconciled successfully");
}

/// Combines listener statuses with gateway-level conditions into a status
/// JSON payload.
fn build_gateway_conditions(
    name: &str,
    ns: &str,
    addresses: &[serde_json::Value],
    listener_result: &(Vec<serde_json::Value>, bool, bool),
    generation: i64,
) -> serde_json::Value {
    let (ref listener_statuses, any_accepted, any_rejected) = *listener_result;
    let accepted = gateway_accepted_condition(generation, any_accepted, any_rejected);
    let programmed = gateway_programmed_condition(generation, any_accepted);
    gateway_status_json(&GatewayStatusParts {
        name,
        ns,
        addresses,
        listener_statuses,
        accepted: &accepted,
        programmed: &programmed,
    })
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
fn gateway_programmed_condition(
    generation: i64,
    any_accepted: bool,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
    if any_accepted {
        conditions::programmed(generation, "Data plane configured")
    } else {
        conditions::not_programmed(generation, "Invalid", "no valid listeners")
    }
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
