// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway spec to Praxis YAML.
//!
//! Turns the listeners and attached routes of one Gateway into the
//! configuration document the data plane reads, then applies the
//! `ConfigMap`, `Deployment`, `Service`, and `PodDisruptionBudget` that carry
//! it. The config hash computed here is what tells the Gateway
//! controller whether a rollout is still in flight.

use std::collections::{BTreeMap, HashMap, HashSet};

use futures::future::try_join_all;
use gateway_api::{
    gateways::{Gateway, GatewayListeners},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{api::core::v1::ServicePort, apimachinery::pkg::util::intstr::IntOrString};
use kube::ResourceExt as _;
use tracing::debug;

use crate::{
    config::{
        cluster::{PraxisCluster, build_cluster},
        filter_conversion::{RouteFilters, ServedRule, convert_filters},
        generate::assemble_config,
        listener::{PraxisCertificate, PraxisListener, PraxisTls, convert_listener},
        routing::{BackendRef, PraxisRoute, convert_routes, rule_has_authorized_backend},
        weights::{ResolvedBackend, distribute_service_weights, sort_service_endpoints},
    },
    endpoints,
    error::Result,
    gateway_api::{attachment::AttachedRoute, listener_conflict, protocol::ListenerProtocol},
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

/// Backend `Service` lookups issued concurrently while resolving clusters.
const MAX_CONCURRENT_BACKEND_LOOKUPS: usize = 16;

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
    attached: &[AttachedRoute<'_>],
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
    let route_filters = collect_filters(attached, grants);
    let clusters = resolve_clusters(client, &backend_refs).await?;
    let config = assemble_config(
        praxis_listeners,
        &praxis_routes,
        &clusters,
        &route_filters,
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
    attached: &[AttachedRoute<'_>],
    listener_hostnames: &HashMap<String, Option<String>>,
    grants: &[ReferenceGrant],
) -> (Vec<PraxisRoute>, Vec<BackendRef>) {
    convert_routes(attached, listener_hostnames, grants)
}

/// Extracts and converts filters from all attached route rules.
///
/// Each rule is paired with whether any of its backends survived the
/// reference checks, which is what decides between a rule the router
/// will serve and one that has to answer 500 on its own.
fn collect_filters(attached: &[AttachedRoute<'_>], grants: &[ReferenceGrant]) -> RouteFilters {
    let all_rules: Vec<_> = attached
        .iter()
        .flat_map(|attached| {
            let namespace = attached.route.namespace().unwrap_or_default();
            attached
                .route
                .spec
                .rules
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(move |rule| ServedRule {
                    rule,
                    resolvable: rule_has_authorized_backend(rule, &namespace, grants),
                })
        })
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
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::{
        controller::fixtures::{https_listener, listener},
        testing,
    };

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

    // -----------------------------------------------------------------------
    // Config Generation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_gateway_with_one_route_generates_a_routed_config() {
        let (client, _) = testing::fake_client(vec![backend_service_response(), endpoint_slice_response()]);
        let route = route();
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        let output = build_praxis_config(&client, &[http_listener()], &attached, &[])
            .await
            .expect("the backend resolves");

        assert!(
            output.config_yaml.contains("10.0.0.1:8080"),
            "the resolved endpoint has to reach the data plane config: {}",
            output.config_yaml
        );
        assert_eq!(
            output.listener_ports,
            vec![("http".to_owned(), 80)],
            "the container and Service need a port per distinct listener port"
        );
        assert!(
            output.tls_secret_names.is_empty(),
            "an HTTP listener mounts no certificates"
        );
    }

    #[tokio::test]
    async fn test_an_unsupported_listener_contributes_nothing() {
        let (client, _) = testing::fake_client(vec![]);
        let mut listener = http_listener();
        listener.protocol = "TCP".to_owned();

        let output = build_praxis_config(&client, &[listener], &[], &[])
            .await
            .expect("a config with no listeners is still a config");

        assert!(
            output.listener_ports.is_empty(),
            "binding a port for a protocol the data plane cannot serve would answer requests with \
             nothing"
        );
    }

    #[tokio::test]
    async fn test_a_backend_lookup_failure_fails_the_config() {
        let (client, _) = testing::failing_client();
        let route = route();
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        let Err(error) = build_praxis_config(&client, &[http_listener()], &attached, &[]).await else {
            panic!("a 500 from the endpoints API is not an empty backend");
        };

        assert!(
            matches!(error, crate::error::OperatorError::Kube(_)),
            "generating a config with no endpoints because the API server was down would blackhole \
             live traffic: {error}"
        );
    }

    #[tokio::test]
    async fn test_applying_children_writes_all_four_and_returns_the_hash() {
        let (client, journal) = testing::fake_client(vec![
            testing::Canned::ok("/configmaps", serde_json::json!({ "kind": "ConfigMap" })),
            testing::Canned::ok("/deployments", serde_json::json!({ "kind": "Deployment" })),
            testing::Canned::ok("/services", serde_json::json!({ "kind": "Service" })),
            testing::Canned::ok(
                "/poddisruptionbudgets",
                serde_json::json!({ "kind": "PodDisruptionBudget" }),
            ),
        ]);
        let output = PraxisConfigOutput {
            config_yaml: "listeners: []\n".to_owned(),
            listener_ports: vec![("http".to_owned(), 80)],
            tls_secret_names: vec![],
        };

        let hash = Box::pin(apply_child_resources(&client, &gateway(), &output))
            .await
            .expect("every apply is answered");

        assert_eq!(
            hash,
            sha256_hex(&output.config_yaml),
            "the returned hash is what the pod template is annotated with, so it has to be the \
             hash of the config that was actually applied"
        );
        for kind in ["/configmaps", "/deployments", "/services", "/poddisruptionbudgets"] {
            assert_eq!(
                journal.matching(kind).len(),
                1,
                "every child resource has to be applied, or the data plane is half-built: {kind}"
            );
        }
    }

    #[tokio::test]
    async fn test_a_refused_apply_stops_the_rest() {
        let (client, journal) = testing::failing_client();
        let output = PraxisConfigOutput {
            config_yaml: "listeners: []\n".to_owned(),
            listener_ports: vec![],
            tls_secret_names: vec![],
        };

        Box::pin(apply_child_resources(&client, &gateway(), &output))
            .await
            .expect_err("a refused ConfigMap is not success");

        assert_eq!(
            journal.requests().len(),
            1,
            "applying a Deployment that points at a ConfigMap the API server refused would start \
             pods with no config"
        );
    }

    #[test]
    fn test_service_ports_target_the_listener_port() {
        let ports = build_service_ports(&[("http".to_owned(), 80), ("https".to_owned(), 443)]);

        assert_eq!(ports.len(), 2, "one Service port per listener port");
        assert_eq!(
            (ports[0].port, ports[0].target_port.clone()),
            (80, Some(IntOrString::Int(80))),
            "the data plane listens on the same port the Service publishes, so the two match"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a plain HTTP listener on port 80.
    fn http_listener() -> GatewayListeners {
        GatewayListeners {
            name: "http".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            ..Default::default()
        }
    }

    /// Builds a route with one backend in the same namespace.
    fn route() -> gateway_api::httproutes::HTTPRoute {
        use gateway_api::httproutes::{HTTPRoute, HttpRouteRules, HttpRouteRulesBackendRefs, HttpRouteSpec};

        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "svc".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds the Gateway the child resources belong to.
    fn gateway() -> Gateway {
        use gateway_api::gateways::GatewaySpec;

        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("infra".to_owned()),
                uid: Some("uid".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners: vec![http_listener()],
                ..Default::default()
            },
            status: None,
        }
    }

    /// The backend Service the route names.
    fn backend_service_response() -> testing::Canned {
        testing::Canned::ok(
            "/services/svc",
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": { "name": "svc", "namespace": "infra" },
                "spec": { "ports": [{ "port": 8080, "targetPort": 8080 }] },
            }),
        )
    }

    /// One ready endpoint for the route's backend Service.
    fn endpoint_slice_response() -> testing::Canned {
        testing::Canned::ok(
            "/endpointslices",
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSliceList",
                "metadata": {},
                "items": [{
                    "metadata": { "name": "svc-abc", "namespace": "infra" },
                    "addressType": "IPv4",
                    "endpoints": [{
                        "addresses": ["10.0.0.1"],
                        "conditions": { "ready": true },
                    }],
                    "ports": [{ "port": 8080 }],
                }],
            }),
        )
    }
}
