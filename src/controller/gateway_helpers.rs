// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Extracted helpers for Gateway reconciliation.
//!
//! Contains validation, namespace filtering, and label-selector matching
//! logic used by the main Gateway controller.

use std::collections::{BTreeMap, HashMap, HashSet};

use futures::future::try_join_all;
use gateway_api::{
    gatewayclasses::GatewayClass,
    gateways::{
        Gateway, GatewayListeners, GatewayListenersAllowedRoutesKinds, GatewayListenersAllowedRoutesNamespacesFrom,
        GatewayListenersAllowedRoutesNamespacesSelector,
        GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions, GatewayListenersTlsCertificateRefs,
    },
    httproutes::{HTTPRoute, HttpRouteParentRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{
    ByteString,
    api::{
        apps::v1::{Deployment, DeploymentStatus},
        core::v1::{Namespace, Secret, Service, ServicePort},
    },
    apimachinery::pkg::{apis::meta::v1::Condition, util::intstr::IntOrString},
};
use kube::{
    Api, ResourceExt as _,
    api::{Patch, PatchParams},
};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::{
    config::{
        cluster::{PraxisCluster, build_cluster},
        filter_conversion::convert_filters,
        generate::assemble_config,
        listener::{PraxisCertificate, PraxisListener, PraxisTls, convert_listener},
        routing::{BackendRef, PraxisFilterEntry, PraxisRoute, convert_routes},
    },
    context::CONTROLLER_NAME,
    endpoints,
    error::{OperatorError, Result},
    gateway_api::{
        attachment, conditions, hostname, listener_conflict, protocol::ListenerProtocol, reference_grant, route_status,
        route_validation, status,
    },
    listing,
    observability::metrics,
    resources::{
        configmap::build_configmap,
        deployment::{DeploymentParams, build_deployment},
        disruption::build_pod_disruption_budget,
        labels::child_name,
        service::build_service,
    },
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Field manager used for every server-side apply the operator issues.
const FIELD_MANAGER: &str = "praxis-operator";

/// Backend `Service` lookups issued concurrently while resolving clusters.
const MAX_CONCURRENT_BACKEND_LOOKUPS: usize = 16;

// -----------------------------------------------------------------------------
// Type Aliases
// -----------------------------------------------------------------------------

/// A resolved backend: its Gateway API weight and its ready endpoints.
type ResolvedBackend = (i32, Vec<String>);

/// Ceiling on the least-common-multiple denominator used to spread a
/// service weight across its endpoints.
///
/// Gateway API allows `backendRef.weight` up to 1,000,000; without a
/// ceiling, coprime endpoint counts drive the denominator high enough to
/// overflow the weight arithmetic.
const MAX_LCM_DENOMINATOR: i64 = 1_000_000; // 1e6

/// Largest weight the generated data-plane config can carry.
const MAX_ENDPOINT_WEIGHT: i64 = 2_147_483_647; // i32::MAX

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
async fn fetch_gateway_class(client: &kube::Client, gc_name: &str) -> Result<GatewayClass> {
    let api = Api::<GatewayClass>::all(client.clone());
    api.get(gc_name).await.map_err(|e| map_gc_error(e, gc_name))
}

/// Maps a `GatewayClass` lookup error to an operator error.
fn map_gc_error(e: kube::Error, gc_name: &str) -> OperatorError {
    if is_api_not_found(&e) {
        debug!("GatewayClass {gc_name} not found");
        return OperatorError::GatewayClassNotFound(gc_name.to_owned());
    }

    debug!(%e, "GatewayClass lookup failed");
    OperatorError::Kube(e)
}

/// Returns `true` when the error is a 404 API response.
fn is_api_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(resp) if resp.code == 404)
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

    /// Deduplicated `(listener_name, port)` pairs.
    pub(super) listener_ports: Vec<(String, i32)>,

    /// TLS secret names referenced by HTTPS listeners (deduplicated).
    pub(super) tls_secret_names: Vec<String>,
}

/// Converts Gateway listeners, attached routes, and resolved endpoints
/// into a complete Praxis YAML configuration string.
pub(super) async fn build_praxis_config(
    client: &kube::Client,
    listeners: &[GatewayListeners],
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    grants: &[ReferenceGrant],
) -> Result<PraxisConfigOutput> {
    let conflicts = listener_conflict::detect_conflicts(listeners);
    let supported: Vec<_> = listeners
        .iter()
        .filter(|l| ListenerProtocol::is_supported(&l.protocol))
        .filter(|l| !conflicts.contains_key(&l.name))
        .collect();

    let listener_hostnames = build_listener_hostname_map(&supported);
    let praxis_listeners = merge_listeners_by_port(&supported);
    let (praxis_routes, backend_refs) = convert_attached_routes(attached, &listener_hostnames, grants);
    let extra_filters = collect_filters(attached);
    let clusters = resolve_clusters(client, &backend_refs).await?;
    let config = assemble_config(
        praxis_listeners,
        &praxis_routes,
        &clusters,
        &extra_filters,
        &listener_hostnames,
    )?;

    Ok(PraxisConfigOutput {
        config_yaml: yaml_serde::to_string(&config)?,
        listener_ports: collect_listener_ports(&supported),
        tls_secret_names: collect_tls_secret_names(&supported),
    })
}

/// Merges Gateway listeners on the same port into a single Praxis
/// listener, combining TLS certificates from all listeners in the group.
fn merge_listeners_by_port(supported: &[&GatewayListeners]) -> Vec<PraxisListener> {
    let mut by_port: BTreeMap<i32, Vec<&GatewayListeners>> = BTreeMap::new();
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
fn merge_tls_certs(listener: &mut PraxisListener, group: &[&GatewayListeners]) {
    if group.len() <= 1 {
        return;
    }
    let mut all_certs: Vec<PraxisCertificate> = listener
        .tls
        .as_ref()
        .map(|t| t.certificates.clone())
        .unwrap_or_default();

    for l in group.iter().skip(1) {
        collect_listener_certs(l, &mut all_certs);
    }

    if !all_certs.is_empty() {
        listener.tls = Some(PraxisTls {
            certificates: all_certs,
        });
    }
}

/// Collects TLS certificates from a single listener into the cert list.
fn collect_listener_certs(l: &GatewayListeners, certs: &mut Vec<PraxisCertificate>) {
    let Some(tls) = &l.tls else { return };
    let Some(refs) = &tls.certificate_refs else { return };
    for cert_ref in refs {
        let (server_names, default) = match &l.hostname {
            Some(h) => (Some(vec![h.clone()]), None),
            None => (None, Some(true)),
        };
        certs.push(PraxisCertificate {
            cert_path: format!("/tls/{}/tls.crt", cert_ref.name),
            key_path: format!("/tls/{}/tls.key", cert_ref.name),
            server_names,
            default,
        });
    }
}

/// Builds a map from listener section name to its hostname constraint.
fn build_listener_hostname_map(listeners: &[&GatewayListeners]) -> HashMap<String, Option<String>> {
    listeners.iter().map(|l| (l.name.clone(), l.hostname.clone())).collect()
}

/// Converts attached routes to Praxis routes and collects backend refs.
fn convert_attached_routes(
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    listener_hostnames: &HashMap<String, Option<String>>,
    grants: &[ReferenceGrant],
) -> (Vec<PraxisRoute>, Vec<BackendRef>) {
    let route_refs: Vec<_> = attached.iter().map(|(r, s)| (*r, s.clone())).collect();
    convert_routes(&route_refs, listener_hostnames, grants)
}

/// Extracts and converts filters from all attached route rules.
fn collect_filters(attached: &[(&HTTPRoute, Vec<Option<String>>)]) -> Vec<PraxisFilterEntry> {
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
async fn resolve_clusters(client: &kube::Client, backend_refs: &[BackendRef]) -> Result<Vec<PraxisCluster>> {
    let resolved = resolve_backends(client, backend_refs).await?;

    let mut cluster_data: BTreeMap<String, Vec<ResolvedBackend>> = BTreeMap::new();
    for (backend, entry) in backend_refs.iter().zip(resolved) {
        cluster_data
            .entry(backend.cluster_name.clone())
            .or_default()
            .push(entry);
    }

    Ok(cluster_data
        .into_iter()
        .map(|(name, mut svc)| build_resolved_cluster(&name, &mut svc))
        .collect())
}

/// Resolves every backend ref concurrently, preserving input order.
///
/// Order matters: it decides where each service's endpoints land in the
/// generated config, and therefore whether the config hash is stable.
async fn resolve_backends(client: &kube::Client, backend_refs: &[BackendRef]) -> Result<Vec<ResolvedBackend>> {
    let mut resolved = Vec::with_capacity(backend_refs.len());

    for chunk in backend_refs.chunks(MAX_CONCURRENT_BACKEND_LOOKUPS) {
        let lookups = chunk.iter().map(|backend| resolve_backend(client, backend));
        resolved.extend(try_join_all(lookups).await?);
    }

    Ok(resolved)
}

/// Resolves one backend ref into its weight and ready endpoint addresses.
async fn resolve_backend(client: &kube::Client, backend: &BackendRef) -> Result<ResolvedBackend> {
    let eps = endpoints::resolve_endpoints(client, &backend.namespace, &backend.service, backend.port).await?;
    Ok((backend.weight.unwrap_or(1), eps))
}

/// Builds a single cluster from resolved service endpoint data.
fn build_resolved_cluster(name: &str, service_data: &mut [ResolvedBackend]) -> PraxisCluster {
    sort_service_endpoints(service_data);
    debug!(cluster = %name, services = service_data.len(), "resolving cluster");

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
fn sort_service_endpoints(service_data: &mut [ResolvedBackend]) {
    for (_, eps) in service_data.iter_mut() {
        eps.sort();
    }
}

/// Distributes service-level weights across endpoints.
///
/// For each service with weight `W` and `N` endpoints, assigns
/// `(W * lcm) / N` to each endpoint, where `lcm` is the least
/// common multiple of all endpoint counts. The final weights are
/// reduced by their GCD to minimise the round-robin cycle length,
/// which improves distribution accuracy for small request batches.
///
/// All arithmetic runs in `i64` and saturates. The release profile
/// combines `overflow-checks` with `panic = "abort"`, so an overflow
/// here would kill the operator rather than mis-route a request.
fn distribute_service_weights(service_data: &[ResolvedBackend]) -> (Vec<String>, Vec<i32>) {
    let lcm_denominator = endpoint_count_lcm(service_data);
    let mut all_endpoints = Vec::new();
    let mut all_weights = Vec::new();

    for (service_weight, endpoints) in service_data {
        if endpoints.is_empty() {
            continue;
        }

        let count = endpoint_count(endpoints);
        let ep_weight = i64::from(*service_weight).saturating_mul(lcm_denominator) / count;
        for ep in endpoints {
            all_endpoints.push(ep.clone());
            all_weights.push(ep_weight);
        }
    }

    reduce_weights_by_gcd(&mut all_weights);
    (all_endpoints, scale_weights_into_range(&all_weights))
}

/// Least common multiple of every non-empty endpoint count.
fn endpoint_count_lcm(service_data: &[ResolvedBackend]) -> i64 {
    service_data
        .iter()
        .filter(|(_, eps)| !eps.is_empty())
        .map(|(_, eps)| endpoint_count(eps))
        .fold(1, lcm)
}

/// Returns an endpoint count as a positive `i64`.
fn endpoint_count(endpoints: &[String]) -> i64 {
    i64::try_from(endpoints.len()).unwrap_or(i64::MAX).max(1)
}

/// Divides all positive weights by their GCD to minimise cycle length.
fn reduce_weights_by_gcd(weights: &mut [i64]) {
    let g = weights.iter().copied().filter(|w| *w > 0).fold(0, gcd);
    if g > 1 {
        for w in weights.iter_mut() {
            if *w > 0 {
                *w /= g;
            }
        }
    }
}

/// Scales weights down until each one fits the config's `i32` field.
///
/// Positive weights stay positive so an endpoint is never silently
/// dropped from the load-balancing rotation.
fn scale_weights_into_range(weights: &[i64]) -> Vec<i32> {
    let largest = weights.iter().copied().max().unwrap_or(0);
    let divisor = (largest.saturating_add(MAX_ENDPOINT_WEIGHT - 1) / MAX_ENDPOINT_WEIGHT).max(1);

    weights.iter().map(|w| scale_weight(*w, divisor)).collect()
}

/// Scales a single weight into `i32` range.
fn scale_weight(weight: i64, divisor: i64) -> i32 {
    let scaled = weight / divisor;
    let floored = if weight > 0 { scaled.max(1) } else { scaled };
    i32::try_from(floored).unwrap_or(i32::MAX)
}

/// Greatest common divisor (Euclidean algorithm).
fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.saturating_abs()
}

/// Least common multiple, capped at [`MAX_LCM_DENOMINATOR`].
fn lcm(a: i64, b: i64) -> i64 {
    if a == 0 || b == 0 {
        return 0;
    }

    (a / gcd(a, b))
        .checked_mul(b)
        .map_or(MAX_LCM_DENOMINATOR, i64::saturating_abs)
        .min(MAX_LCM_DENOMINATOR)
}

/// Deduplicates TLS secret names from HTTPS listeners.
fn collect_tls_secret_names(listeners: &[&GatewayListeners]) -> Vec<String> {
    let mut seen = HashSet::new();
    listeners
        .iter()
        .filter(|l| ListenerProtocol::terminates_tls(&l.protocol))
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
    let deploy = build_deployment(&DeploymentParams {
        name: &child,
        config_hash: &config_hash,
        gateway: gw,
        listener_ports: &config_output.listener_ports,
        namespace: &ns,
        tls_secret_names: &config_output.tls_secret_names,
    })?;
    super::gateway::apply_resource(client, &ns, &deploy).await?;

    let ports = build_service_ports(&config_output.listener_ports);
    let svc = build_service(&child, &ns, gw, ports)?;
    super::gateway::apply_resource(client, &ns, &svc).await?;

    let budget = build_pod_disruption_budget(&child, &ns, gw)?;
    super::gateway::apply_resource(client, &ns, &budget).await?;

    Ok(config_hash)
}

/// Converts `(name, port)` pairs into Kubernetes `ServicePort` entries.
fn build_service_ports(listener_ports: &[(String, i32)]) -> Vec<ServicePort> {
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
    let digest = <sha2::Sha256 as sha2::Digest>::digest(data.as_bytes());
    format!("{digest:x}")
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
    let status = gateway_status_json(&GatewayStatusParts {
        accepted: &gateway_accepted_condition(generation, any_accepted, any_rejected),
        addresses: &addresses,
        listener_statuses: &listener_statuses,
        programmed: &gateway_programmed_condition(generation, any_accepted, data_plane_ready),
    });

    apply_gateway_status(client, gw, &status).await?;
    info!("Gateway {ns}/{name} reconciled successfully");
    Ok(())
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
    grants: &[ReferenceGrant],
) -> Result<()> {
    let gw_ns = gw.namespace().unwrap_or_default();
    let gw_name = gw.name_any();

    for (route, _) in attached {
        let route_ns = route_status::route_namespace(route);
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
            grants,
        )
        .await;

        if !statuses.is_empty() {
            route_status::apply_parent_statuses(client, route, &statuses).await?;
        }
    }
    Ok(())
}

/// Builds parent status entries for refs targeting this Gateway.
#[expect(clippy::too_many_arguments, reason = "route status needs full context")]
async fn build_route_statuses(
    route: &HTTPRoute,
    parent_refs: &[HttpRouteParentRefs],
    route_ns: &str,
    gw_name: &str,
    gw_ns: &str,
    generation: i64,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> Vec<Value> {
    let validation = route_validation::validate_route(route);

    let mut statuses = Vec::new();
    for parent_ref in parent_refs {
        if !route_status::is_ref_targeting_gateway(parent_ref, gw_name, gw_ns, route_ns) {
            continue;
        }

        let resolved = route_status::check_backend_refs(route, route_ns, client, grants).await;
        let resolved_cond = route_status::resolved_refs_condition(&resolved, generation);
        let mut route_conditions = validation_conditions(&validation, generation);
        route_conditions.push(resolved_cond);

        statuses.push(route_status::parent_status_with_conditions(
            parent_ref,
            gw_ns,
            &route_conditions,
        ));
    }
    statuses
}

/// Builds the `Accepted` condition, plus `PartiallyInvalid` when only
/// some rules were dropped.
fn validation_conditions(validation: &route_validation::RouteValidation, generation: i64) -> Vec<Condition> {
    let detail = validation.message().unwrap_or_default();

    if validation.is_fully_rejected() {
        return vec![conditions::not_accepted(generation, "UnsupportedValue", &detail)];
    }

    let accepted = conditions::accepted(generation, "route accepted");
    if validation.is_partially_rejected() {
        return vec![accepted, conditions::partially_invalid(generation, &detail)];
    }

    vec![accepted]
}

/// Components used to build the Gateway status JSON payload.
struct GatewayStatusParts<'a> {
    /// Gateway-level `Accepted` condition.
    accepted: &'a Condition,

    /// Load-balancer addresses.
    addresses: &'a [Value],

    /// Per-listener status entries.
    listener_statuses: &'a [Value],

    /// Gateway-level `Programmed` condition.
    programmed: &'a Condition,
}

/// Constructs the `status` sub-object of the Gateway status patch.
fn gateway_status_json(parts: &GatewayStatusParts<'_>) -> Value {
    json!({
        "addresses": parts.addresses,
        "conditions": [parts.accepted, parts.programmed],
        "listeners": parts.listener_statuses,
    })
}

/// Patches the Gateway status via server-side apply.
///
/// Carries condition transition times forward and returns without
/// contacting the API server when the computed status already matches
/// the live object. Writing an unchanged status re-triggers the
/// controller's own watch, which would keep an idle Gateway reconciling
/// forever.
pub(super) async fn apply_gateway_status(client: &kube::Client, gw: &Gateway, status_json: &Value) -> Result<()> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let observed = serde_json::to_value(&gw.status)?;
    let mut desired = status_json.clone();
    status::preserve_condition_times(&mut desired, &observed);

    if status::is_status_unchanged(&desired, &observed) {
        metrics::global().record_status_skipped();
        debug!("Gateway {ns}/{name} status unchanged, skipping patch");
        return Ok(());
    }
    metrics::global().record_status_written();

    let payload = json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": { "name": name, "namespace": ns },
        "status": desired,
    });

    Api::<Gateway>::namespaced(client.clone(), &ns)
        .patch_status(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&payload),
        )
        .await?;
    Ok(())
}

/// Queries the child Service for load-balancer ingress IP addresses.
async fn resolve_lb_addresses(client: &kube::Client, ns: &str, child: &str) -> Vec<Value> {
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
fn is_new_rs_available(status: &DeploymentStatus) -> bool {
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
) -> (Vec<Value>, bool, bool) {
    let conflicts = listener_conflict::detect_conflicts(listeners);
    let mut statuses = Vec::new();
    let mut any_accepted = false;
    let mut any_rejected = false;

    for l in listeners {
        if let Some(reason) = conflicts.get(&l.name) {
            any_rejected = true;
            statuses.push(conflicted_listener_status(l, generation, *reason));
            continue;
        }

        let protocol_supported = ListenerProtocol::is_supported(&l.protocol);
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

/// Builds a status entry for a listener conflicting with another.
///
/// A conflicted listener is not accepted, not programmed, and attaches
/// no routes: it never reaches the data plane, so claiming otherwise
/// would misreport what is serving traffic.
fn conflicted_listener_status(
    l: &GatewayListeners,
    generation: i64,
    reason: listener_conflict::ConflictReason,
) -> Value {
    json!({
        "name": l.name,
        "attachedRoutes": 0,
        "supportedKinds": [],
        "conditions": [
            conditions::not_accepted(generation, reason.as_str(), reason.message()),
            conditions::conflicted(generation, reason.as_str(), reason.message()),
            conditions::not_programmed(generation, reason.as_str(), reason.message()),
        ],
    })
}

/// Builds a status entry for an unsupported-protocol listener.
fn unsupported_listener_status(l: &GatewayListeners, generation: i64) -> Value {
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
                Some(lh) => route_hostnames.iter().any(|rh| hostname::hostname_matches(rh, lh)),
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
) -> Value {
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
///
/// `ListenersNotValid` is only a valid reason alongside `Accepted:
/// False`, so a Gateway with a mix of valid and invalid listeners
/// reports `Accepted`/`Accepted` and carries the partial failure in the
/// message; the per-listener conditions describe which ones failed.
fn gateway_accepted_condition(generation: i64, any_accepted: bool, any_rejected: bool) -> Condition {
    if !any_accepted {
        return conditions::not_accepted(
            generation,
            "ListenersNotValid",
            "no listeners have a supported protocol",
        );
    }

    if any_rejected {
        return conditions::accepted(generation, "Gateway accepted, but some listeners are invalid");
    }

    conditions::accepted(generation, "Gateway accepted")
}

/// Returns the `Programmed` condition for the Gateway.
///
/// Requires accepted listeners, a ready Deployment, and at least one
/// load-balancer address before reporting `True`.
fn gateway_programmed_condition(generation: i64, any_accepted: bool, data_plane_ready: bool) -> Condition {
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
) -> (Vec<Value>, Condition) {
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
fn validate_route_kinds(listener: &GatewayListeners) -> (Vec<Value>, bool) {
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
fn httproute_supported_kinds() -> Vec<Value> {
    vec![json!({"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"})]
}

/// Checks whether a route kind ref is `HTTPRoute` in the Gateway API group.
fn is_httproute_kind(k: &GatewayListenersAllowedRoutesKinds) -> bool {
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
) -> Option<Condition> {
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
fn is_secret_cert_ref(cert_ref: &GatewayListenersTlsCertificateRefs) -> bool {
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
) -> Option<Condition> {
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
async fn list_reference_grants(client: &kube::Client, ns: &str) -> Result<Vec<ReferenceGrant>> {
    let api = Api::<ReferenceGrant>::namespaced(client.clone(), ns);
    listing::list_all(&api).await
}

/// Checks whether a Gateway-to-Secret cross-namespace ref is allowed.
fn is_secret_ref_granted(gateway_ns: &str, secret_ns: &str, secret_name: &str, grants: &[ReferenceGrant]) -> bool {
    reference_grant::is_reference_allowed(
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
) -> Option<Condition> {
    let secret_api = Api::<Secret>::namespaced(client.clone(), secret_ns);

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
fn validate_tls_secret_data(data: Option<&BTreeMap<String, ByteString>>, generation: i64) -> Option<Condition> {
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
fn is_pem_entry(data: &BTreeMap<String, ByteString>, key: &str) -> bool {
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
            route_allowed_by_any_listener(route, section_names, listeners, gateway_ns, all_namespaces.as_deref())
        })
        .cloned()
        .collect()
}

/// Fetches all namespaces from the cluster, returning `None` on error.
async fn fetch_all_namespaces(client: &kube::Client) -> Option<Vec<Namespace>> {
    match listing::list_all(&Api::<Namespace>::all(client.clone())).await {
        Ok(namespaces) => Some(namespaces),
        Err(e) => {
            warn!(%e, "failed to list namespaces for route filtering");
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
    all_namespaces: Option<&[Namespace]>,
) -> bool {
    let route_ns = route_status::route_namespace(route);
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
    all_namespaces: Option<&[Namespace]>,
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
    all_namespaces: Option<&[Namespace]>,
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

    all_ns.iter().any(|ns_obj| {
        let ns_name = ns_obj.metadata.name.as_deref().unwrap_or("");
        ns_name == route_ns && matches_label_selector(ns_obj, selector)
    })
}

/// Checks whether a namespace's labels satisfy a label selector.
///
/// Evaluates both `matchLabels` and `matchExpressions`.
fn matches_label_selector(ns_obj: &Namespace, selector: &GatewayListenersAllowedRoutesNamespacesSelector) -> bool {
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
    labels: &BTreeMap<String, String>,
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
    use gateway_api::{
        gateways::{GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesNamespaces, GatewayListenersTls},
        httproutes::HttpRouteSpec,
    };
    use k8s_openapi::{
        api::apps::v1::DeploymentCondition,
        apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time},
        jiff::Timestamp,
    };

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
            cond.reason, "Accepted",
            "Accepted: True must not carry ListenersNotValid, which is a False-only reason"
        );
        assert!(
            cond.message.contains("some listeners are invalid"),
            "the partial failure belongs in the message: {}",
            cond.message
        );
    }

    // -----------------------------------------------------------------------------
    // Route Validation Conditions
    // -----------------------------------------------------------------------------

    #[test]
    fn test_validation_conditions_accepts_a_supported_route() {
        let conds = validation_conditions(&route_validation::RouteValidation::default(), 1);

        assert_eq!(conds.len(), 1, "a supported route needs only an Accepted condition");
        assert_eq!(conds[0].type_, "Accepted", "the condition should be Accepted");
        assert_eq!(conds[0].status, "True", "a supported route is accepted");
    }

    #[test]
    fn test_validation_conditions_rejects_a_fully_invalid_route() {
        let route = regex_route(1);
        let conds = validation_conditions(&route_validation::validate_route(&route), 1);

        assert_eq!(conds[0].type_, "Accepted", "the first condition should be Accepted");
        assert_eq!(
            conds[0].status, "False",
            "a route whose every rule is unsupported must not be accepted"
        );
        assert_eq!(
            conds[0].reason, "UnsupportedValue",
            "the Gateway API reason for an unrepresentable value is UnsupportedValue"
        );
    }

    #[test]
    fn test_validation_conditions_marks_a_partially_invalid_route() {
        let route = regex_route(2);
        let conds = validation_conditions(&route_validation::validate_route(&route), 1);

        assert_eq!(conds.len(), 2, "a partially invalid route carries a second condition");
        assert_eq!(conds[0].status, "True", "surviving rules keep the route accepted");
        assert_eq!(
            conds[1].type_, "PartiallyInvalid",
            "dropped rules must be signalled with PartiallyInvalid"
        );
        assert_eq!(conds[1].status, "True", "PartiallyInvalid should be True");
    }

    // -----------------------------------------------------------------------------
    // Weight Distribution
    // -----------------------------------------------------------------------------

    #[test]
    fn test_gcd_basics() {
        assert_eq!(gcd(0, 0), 0, "gcd of zeros is zero");
        assert_eq!(gcd(12, 0), 12, "gcd with zero returns the other operand");
        assert_eq!(gcd(12, 18), 6, "gcd(12, 18) is 6");
        assert_eq!(gcd(17, 5), 1, "coprime operands have gcd 1");
    }

    #[test]
    fn test_gcd_of_extremes_does_not_overflow() {
        assert_eq!(
            gcd(i64::MIN, 0),
            i64::MAX,
            "gcd must saturate rather than overflow on i64::MIN"
        );
    }

    #[test]
    fn test_lcm_basics() {
        assert_eq!(lcm(0, 5), 0, "lcm with zero is zero");
        assert_eq!(lcm(4, 6), 12, "lcm(4, 6) is 12");
        assert_eq!(lcm(lcm(lcm(7, 11), 13), 17), 17_017, "coprime counts multiply out");
    }

    #[test]
    fn test_lcm_is_capped() {
        assert_eq!(
            lcm(MAX_LCM_DENOMINATOR, 999_983),
            MAX_LCM_DENOMINATOR,
            "the denominator must never exceed its ceiling"
        );
    }

    #[test]
    fn test_lcm_of_large_coprimes_does_not_overflow() {
        assert!(
            lcm(i64::MAX, i64::MAX - 1) <= MAX_LCM_DENOMINATOR,
            "an lcm that cannot be represented must fall back to the ceiling"
        );
    }

    #[test]
    fn test_distribute_weights_single_service_is_uniform() {
        let data = [(1, endpoints(&["10.0.0.1:80", "10.0.0.2:80"]))];
        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(eps.len(), 2, "every endpoint should be emitted");
        assert_eq!(weights, vec![1, 1], "a single service splits evenly across its pods");
    }

    #[test]
    fn test_distribute_weights_respects_service_ratio() {
        let data = [(3, endpoints(&["10.0.0.1:80"])), (1, endpoints(&["10.0.1.1:80"]))];
        let (_, weights) = distribute_service_weights(&data);

        assert_eq!(
            weights,
            vec![3, 1],
            "endpoint weights should mirror the backend weights"
        );
    }

    #[test]
    fn test_distribute_weights_normalises_uneven_replica_counts() {
        let data = [
            (1, endpoints(&["10.0.0.1:80", "10.0.0.2:80"])),
            (1, endpoints(&["10.0.1.1:80"])),
        ];
        let (_, weights) = distribute_service_weights(&data);

        assert_eq!(
            weights,
            vec![1, 1, 2],
            "a one-pod service must carry the same total share as a two-pod service"
        );
    }

    #[test]
    fn test_distribute_weights_skips_services_without_endpoints() {
        let data = [(5, endpoints(&[])), (1, endpoints(&["10.0.1.1:80"]))];
        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(
            eps,
            vec!["10.0.1.1:80".to_owned()],
            "an empty service contributes nothing"
        );
        assert_eq!(weights, vec![1], "only the resolved service is weighted");
    }

    #[test]
    fn test_distribute_weights_survives_adversarial_endpoint_counts() {
        let data = [
            (1_000_000, endpoints(&["10.0.0.1:80"; 7])),
            (1_000_000, endpoints(&["10.0.1.1:80"; 11])),
            (1_000_000, endpoints(&["10.0.2.1:80"; 13])),
            (1_000_000, endpoints(&["10.0.3.1:80"; 17])),
        ];

        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(eps.len(), 48, "every pod of every backend should be emitted");
        assert_eq!(weights.len(), 48, "each endpoint needs a weight");
        assert!(
            weights.iter().all(|w| *w > 0),
            "coprime pod counts at the maximum Gateway API weight must not zero out or abort"
        );
    }

    #[test]
    fn test_distribute_weights_saturates_at_the_config_ceiling() {
        let data = [
            (i32::MAX, endpoints(&["10.0.0.1:80"])),
            (1, endpoints(&["10.0.1.1:80"])),
        ];

        let (_, weights) = distribute_service_weights(&data);

        assert!(
            weights.iter().all(|w| *w > 0),
            "extreme weights must stay representable instead of overflowing"
        );
    }

    #[test]
    fn test_reduce_weights_by_gcd() {
        let mut weights = vec![4, 8, 12];
        reduce_weights_by_gcd(&mut weights);

        assert_eq!(weights, vec![1, 2, 3], "weights should be reduced by their gcd");
    }

    #[test]
    fn test_reduce_weights_ignores_zero_weights() {
        let mut weights = vec![0, 4, 8];
        reduce_weights_by_gcd(&mut weights);

        assert_eq!(weights, vec![0, 1, 2], "a zero weight must stay zero");
    }

    #[test]
    fn test_scale_weight_keeps_positive_weights_positive() {
        assert_eq!(scale_weight(1, 1_000), 1, "a positive weight never scales to zero");
        assert_eq!(scale_weight(0, 1_000), 0, "a zero weight stays zero");
        assert_eq!(scale_weight(2_000, 1_000), 2, "scaling divides by the divisor");
    }

    #[test]
    fn test_scale_weights_into_range_fits_i32() {
        let weights = [i64::from(i32::MAX) * 4, i64::from(i32::MAX) * 2];
        let scaled = scale_weights_into_range(&weights);

        assert_eq!(scaled.len(), 2, "every weight should be scaled");
        assert!(
            scaled.iter().all(|w| *w > 0),
            "scaling must keep every endpoint in the rotation"
        );
    }

    #[test]
    fn test_sort_service_endpoints_is_deterministic() {
        let mut data = [(1, endpoints(&["10.0.0.3:80", "10.0.0.1:80", "10.0.0.2:80"]))];
        sort_service_endpoints(&mut data);

        assert_eq!(
            data[0].1,
            endpoints(&["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]),
            "endpoint order must be stable so the config hash does not churn"
        );
    }

    #[test]
    fn test_endpoint_count_never_returns_zero() {
        assert_eq!(endpoint_count(&[]), 1, "an empty list must not produce a zero divisor");
        assert_eq!(
            endpoint_count(&endpoints(&["a", "b"])),
            2,
            "count should match the list"
        );
    }

    // -----------------------------------------------------------------------------
    // Config Hashing
    // -----------------------------------------------------------------------------

    #[test]
    fn test_sha256_hex_of_empty_string() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "SHA-256 of the empty string is a known constant"
        );
    }

    #[test]
    fn test_sha256_hex_is_stable_and_lowercase() {
        let digest = sha256_hex("listeners: []\n");

        assert_eq!(digest.len(), 64, "a SHA-256 digest is 64 hex characters");
        assert_eq!(digest, sha256_hex("listeners: []\n"), "hashing must be deterministic");
        assert!(
            digest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "the digest should be lowercase hex"
        );
    }

    // -----------------------------------------------------------------------------
    // Listener Aggregation
    // -----------------------------------------------------------------------------

    #[test]
    fn test_collect_listener_ports_deduplicates_by_port() {
        let listeners = [listener("http", 80, "HTTP"), listener("http-2", 80, "HTTP")];
        let refs: Vec<&GatewayListeners> = listeners.iter().collect();

        assert_eq!(
            collect_listener_ports(&refs),
            vec![("http".to_owned(), 80)],
            "listeners sharing a port collapse into one Service port"
        );
    }

    #[test]
    fn test_collect_listener_ports_keeps_distinct_ports() {
        let listeners = [listener("http", 80, "HTTP"), listener("https", 443, "HTTPS")];
        let refs: Vec<&GatewayListeners> = listeners.iter().collect();

        assert_eq!(
            collect_listener_ports(&refs),
            vec![("http".to_owned(), 80), ("https".to_owned(), 443)],
            "distinct ports should each appear"
        );
    }

    #[test]
    fn test_collect_tls_secret_names_deduplicates() {
        let listeners = [https_listener("a", 443, "cert"), https_listener("b", 8443, "cert")];
        let refs: Vec<_> = listeners.iter().collect();

        assert_eq!(
            collect_tls_secret_names(&refs),
            vec!["cert".to_owned()],
            "a secret referenced twice is mounted once"
        );
    }

    #[test]
    fn test_collect_tls_secret_names_ignores_http_listeners() {
        let listeners = [listener("http", 80, "HTTP")];
        let refs: Vec<_> = listeners.iter().collect();

        assert!(
            collect_tls_secret_names(&refs).is_empty(),
            "plain HTTP listeners have no certificates"
        );
    }

    #[test]
    fn test_merge_listeners_by_port_groups_section_names() {
        let listeners = [listener("http", 80, "HTTP"), listener("http-alt", 80, "HTTP")];
        let refs: Vec<&GatewayListeners> = listeners.iter().collect();
        let merged = merge_listeners_by_port(&refs);

        assert_eq!(merged.len(), 1, "listeners on the same port merge into one");
        assert_eq!(
            merged[0].merged_section_names,
            vec!["http".to_owned(), "http-alt".to_owned()],
            "the merged listener must remember every section it serves"
        );
    }

    #[test]
    fn test_merge_listeners_by_port_keeps_distinct_ports_separate() {
        let listeners = [listener("http", 80, "HTTP"), listener("alt", 8080, "HTTP")];
        let refs: Vec<&GatewayListeners> = listeners.iter().collect();

        assert_eq!(
            merge_listeners_by_port(&refs).len(),
            2,
            "listeners on distinct ports stay separate"
        );
    }

    #[test]
    fn test_merge_tls_certs_combines_certificates() {
        let first = https_listener("a", 443, "cert-a");
        let second = https_listener("b", 443, "cert-b");
        let refs: Vec<&GatewayListeners> = vec![&first, &second];
        let mut merged = convert_listener(&first, "a-chain");

        merge_tls_certs(&mut merged, &refs);

        assert_eq!(
            merged.tls.map(|t| t.certificates.len()),
            Some(2),
            "both listeners' certificates should serve the shared port"
        );
    }

    #[test]
    fn test_build_service_ports_maps_names_and_targets() {
        let ports = build_service_ports(&[("http".to_owned(), 80)]);

        assert_eq!(ports.len(), 1, "one listener port yields one Service port");
        assert_eq!(ports[0].name, Some("http".to_owned()), "the listener name is reused");
        assert_eq!(ports[0].port, 80, "the listener port is exposed");
        assert_eq!(
            ports[0].target_port,
            Some(IntOrString::Int(80)),
            "the data plane listens on the same port"
        );
        assert_eq!(ports[0].protocol, Some("TCP".to_owned()), "HTTP listeners are TCP");
    }

    #[test]
    fn test_count_attached_routes_matches_hostname() {
        let listener = https_listener("https", 443, "cert");
        let route = route_with_hostnames(&["a.example.com"]);
        let attached = vec![(&route, vec![None])];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            0,
            "a route whose hostname misses the listener must not be counted"
        );
    }

    #[test]
    fn test_count_attached_routes_counts_unconstrained_routes() {
        let listener = listener("http", 80, "HTTP");
        let route = route_with_hostnames(&[]);
        let attached = vec![(&route, vec![None])];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            1,
            "a route without hostnames attaches to any listener"
        );
    }

    #[test]
    fn test_count_attached_routes_respects_section_name() {
        let listener = listener("http", 80, "HTTP");
        let route = route_with_hostnames(&[]);
        let attached = vec![(&route, vec![Some("https".to_owned())])];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            0,
            "a route bound to another section is not attached here"
        );
    }

    // -----------------------------------------------------------------------------
    // Listener Validation
    // -----------------------------------------------------------------------------

    #[test]
    fn test_validate_route_kinds_defaults_to_httproute() {
        let (supported, invalid) = validate_route_kinds(&listener("http", 80, "HTTP"));

        assert_eq!(supported.len(), 1, "HTTPRoute is supported by default");
        assert!(!invalid, "an unspecified kind list is never invalid");
    }

    #[test]
    fn test_validate_route_kinds_flags_unsupported_kinds() {
        let mut l = listener("http", 80, "HTTP");
        l.allowed_routes = Some(GatewayListenersAllowedRoutes {
            kinds: Some(vec![GatewayListenersAllowedRoutesKinds {
                group: None,
                kind: "TCPRoute".to_owned(),
            }]),
            ..Default::default()
        });

        let (supported, invalid) = validate_route_kinds(&l);

        assert!(supported.is_empty(), "an unsupported-only list supports nothing");
        assert!(invalid, "TCPRoute is not implemented and must be reported");
    }

    #[test]
    fn test_is_secret_cert_ref_accepts_core_secret() {
        assert!(
            is_secret_cert_ref(&GatewayListenersTlsCertificateRefs {
                name: "cert".to_owned(),
                ..Default::default()
            }),
            "an unqualified certificateRef defaults to a core Secret"
        );
    }

    #[test]
    fn test_is_secret_cert_ref_rejects_other_kinds() {
        assert!(
            !is_secret_cert_ref(&GatewayListenersTlsCertificateRefs {
                name: "cert".to_owned(),
                kind: Some("ConfigMap".to_owned()),
                ..Default::default()
            }),
            "only Secrets can carry TLS material"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_accepts_pem() {
        let data = secret_data("-----BEGIN CERTIFICATE-----", "-----BEGIN PRIVATE KEY-----");

        assert!(
            validate_tls_secret_data(Some(&data), 1).is_none(),
            "a well-formed TLS secret produces no failure condition"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_rejects_missing_keys() {
        let condition = validate_tls_secret_data(None, 1);

        assert_eq!(
            condition.map(|c| c.message),
            Some("malformed secret".to_owned()),
            "a secret without tls.crt and tls.key is malformed"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_rejects_non_pem() {
        let data = secret_data("not a certificate", "not a key");
        let condition = validate_tls_secret_data(Some(&data), 1);

        assert_eq!(
            condition.map(|c| c.message),
            Some("invalid PEM data".to_owned()),
            "non-PEM contents must be reported"
        );
    }

    #[test]
    fn test_is_pem_entry() {
        let data = secret_data("-----BEGIN CERTIFICATE-----", "garbage");

        assert!(is_pem_entry(&data, "tls.crt"), "a PEM header should be recognised");
        assert!(!is_pem_entry(&data, "tls.key"), "non-PEM data should be rejected");
        assert!(!is_pem_entry(&data, "missing"), "an absent key is not PEM");
    }

    // -----------------------------------------------------------------------------
    // Deployment Readiness
    // -----------------------------------------------------------------------------

    #[test]
    fn test_is_new_rs_available_true() {
        let status = deployment_status("Progressing", "True", "NewReplicaSetAvailable");

        assert!(
            is_new_rs_available(&status),
            "NewReplicaSetAvailable marks a finished rollout"
        );
    }

    #[test]
    fn test_is_new_rs_available_rejects_in_progress_rollout() {
        let status = deployment_status("Progressing", "True", "ReplicaSetUpdated");

        assert!(
            !is_new_rs_available(&status),
            "an updating ReplicaSet is not a finished rollout"
        );
    }

    #[test]
    fn test_is_new_rs_available_without_conditions() {
        assert!(
            !is_new_rs_available(&DeploymentStatus::default()),
            "a Deployment with no conditions has not rolled out"
        );
    }

    // -----------------------------------------------------------------------------
    // Namespace Filtering
    // -----------------------------------------------------------------------------

    #[test]
    fn test_is_namespace_allowed_defaults_to_same() {
        let l = listener("http", 80, "HTTP");

        assert!(
            is_namespace_allowed(&l, "infra", "infra", None),
            "the default policy allows only the Gateway's own namespace"
        );
        assert!(
            !is_namespace_allowed(&l, "apps", "infra", None),
            "the default policy rejects other namespaces"
        );
    }

    #[test]
    fn test_is_namespace_allowed_all() {
        let l = listener_with_namespace_policy(GatewayListenersAllowedRoutesNamespacesFrom::All);

        assert!(
            is_namespace_allowed(&l, "apps", "infra", None),
            "the All policy accepts every namespace"
        );
    }

    #[test]
    fn test_is_namespace_allowed_selector_without_namespaces_is_denied() {
        let l = listener_with_namespace_policy(GatewayListenersAllowedRoutesNamespacesFrom::Selector);

        assert!(
            !is_namespace_allowed(&l, "apps", "infra", None),
            "a selector policy cannot be evaluated without the namespace list"
        );
    }

    #[test]
    fn test_matches_label_selector_match_labels() {
        let ns = namespace("apps", &[("team", "core")]);
        let selector = GatewayListenersAllowedRoutesNamespacesSelector {
            match_labels: Some([("team".to_owned(), "core".to_owned())].into_iter().collect()),
            match_expressions: None,
        };

        assert!(
            matches_label_selector(&ns, &selector),
            "matching labels should satisfy the selector"
        );
    }

    #[test]
    fn test_matches_label_selector_rejects_missing_label() {
        let ns = namespace("apps", &[]);
        let selector = GatewayListenersAllowedRoutesNamespacesSelector {
            match_labels: Some([("team".to_owned(), "core".to_owned())].into_iter().collect()),
            match_expressions: None,
        };

        assert!(
            !matches_label_selector(&ns, &selector),
            "an unlabelled namespace cannot satisfy matchLabels"
        );
    }

    #[test]
    fn test_evaluate_match_expression_operators() {
        let labels: BTreeMap<String, String> = [("team".to_owned(), "core".to_owned())].into_iter().collect();

        assert!(
            evaluate_match_expression(&expression("team", "In", &["core", "infra"]), &labels),
            "In should match a listed value"
        );
        assert!(
            !evaluate_match_expression(&expression("team", "In", &["infra"]), &labels),
            "In should reject an unlisted value"
        );
        assert!(
            evaluate_match_expression(&expression("team", "NotIn", &["infra"]), &labels),
            "NotIn should accept an unlisted value"
        );
        assert!(
            evaluate_match_expression(&expression("team", "Exists", &[]), &labels),
            "Exists should match a present key"
        );
        assert!(
            evaluate_match_expression(&expression("tier", "DoesNotExist", &[]), &labels),
            "DoesNotExist should match an absent key"
        );
        assert!(
            !evaluate_match_expression(&expression("team", "Bogus", &[]), &labels),
            "an unknown operator must not match"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds endpoint address strings from string slices.
    fn endpoints(addrs: &[&str]) -> Vec<String> {
        addrs.iter().map(|a| (*a).to_owned()).collect()
    }

    /// Builds a Gateway listener with the given name, port, and protocol.
    fn listener(name: &str, port: i32, protocol: &str) -> GatewayListeners {
        GatewayListeners {
            name: name.to_owned(),
            port,
            protocol: protocol.to_owned(),
            ..Default::default()
        }
    }

    /// Builds an HTTPS listener referencing a TLS secret, scoped by hostname.
    fn https_listener(name: &str, port: i32, secret: &str) -> GatewayListeners {
        GatewayListeners {
            hostname: Some(format!("{name}.example.com")),
            tls: Some(GatewayListenersTls {
                certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                    name: secret.to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..listener(name, port, "HTTPS")
        }
    }

    /// Builds a listener carrying an explicit `allowedRoutes.namespaces.from`.
    fn listener_with_namespace_policy(from: GatewayListenersAllowedRoutesNamespacesFrom) -> GatewayListeners {
        GatewayListeners {
            allowed_routes: Some(GatewayListenersAllowedRoutes {
                namespaces: Some(GatewayListenersAllowedRoutesNamespaces {
                    from: Some(from),
                    selector: None,
                }),
                ..Default::default()
            }),
            ..listener("http", 80, "HTTP")
        }
    }

    /// Builds an `HTTPRoute` carrying the given hostnames.
    fn route_with_hostnames(hostnames: &[&str]) -> HTTPRoute {
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

    /// Builds Secret data with the given `tls.crt` and `tls.key` contents.
    fn secret_data(cert: &str, key: &str) -> BTreeMap<String, ByteString> {
        [
            ("tls.crt".to_owned(), ByteString(cert.as_bytes().to_vec())),
            ("tls.key".to_owned(), ByteString(key.as_bytes().to_vec())),
        ]
        .into_iter()
        .collect()
    }

    /// Builds a `DeploymentStatus` carrying a single condition.
    fn deployment_status(type_: &str, status: &str, reason: &str) -> DeploymentStatus {
        DeploymentStatus {
            conditions: Some(vec![DeploymentCondition {
                last_transition_time: Some(Time(Timestamp::UNIX_EPOCH)),
                last_update_time: Some(Time(Timestamp::UNIX_EPOCH)),
                message: None,
                reason: Some(reason.to_owned()),
                status: status.to_owned(),
                type_: type_.to_owned(),
            }]),
            ..Default::default()
        }
    }

    /// Builds a `Namespace` with the given labels.
    fn namespace(name: &str, labels: &[(&str, &str)]) -> Namespace {
        Namespace {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                labels: Some(labels.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Builds a label-selector match expression.
    fn expression(
        key: &str,
        operator: &str,
        values: &[&str],
    ) -> GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions {
        GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions {
            key: key.to_owned(),
            operator: operator.to_owned(),
            values: Some(values.iter().map(|v| (*v).to_owned()).collect()),
        }
    }

    /// Builds a route with `rules` rules, the last of which uses an
    /// unsupported `RegularExpression` path match.
    fn regex_route(rules: usize) -> HTTPRoute {
        use gateway_api::httproutes::{
            HttpRouteRules, HttpRouteRulesMatches, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType,
            HttpRouteSpec,
        };

        let built = (0..rules)
            .map(|index| {
                let kind = if index + 1 == rules {
                    HttpRouteRulesMatchesPathType::RegularExpression
                } else {
                    HttpRouteRulesMatchesPathType::Exact
                };
                HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(kind),
                            value: Some("/x".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }
            })
            .collect();

        HTTPRoute {
            metadata: Default::default(),
            spec: HttpRouteSpec {
                rules: Some(built),
                ..Default::default()
            },
            status: None,
        }
    }
}
