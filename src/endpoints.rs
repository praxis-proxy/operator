// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Kubernetes Service endpoint resolution.

use k8s_openapi::api::{
    core::v1::{Endpoints, Service},
    discovery::v1::EndpointSlice,
};
use kube::{Api, Client, api::ListParams};
use tracing::debug;

use crate::error::Result;

// -----------------------------------------------------------------------------
// Endpoint Resolution
// -----------------------------------------------------------------------------

/// Resolves ready endpoint addresses for a Kubernetes Service.
///
/// Tries `EndpointSlice` first (supports headless and manual endpoints),
/// falling back to classic Endpoints for backwards compatibility.
pub(crate) async fn resolve_endpoints(
    client: &Client,
    namespace: &str,
    service_name: &str,
    service_port: i32,
) -> Result<Vec<String>> {
    let Some(svc) = get_or_none(client, namespace, service_name, "service").await? else {
        return Ok(Vec::new());
    };
    let target_port = resolve_target_port(&svc, service_port);

    match resolve_via_endpoint_slices(client, namespace, service_name, target_port).await {
        Ok(eps) if !eps.is_empty() => return Ok(eps),
        Ok(_) => {},
        Err(e) => debug!("EndpointSlice lookup failed, falling back to Endpoints: {e}"),
    }

    let Some(ep) = get_or_none::<Endpoints>(client, namespace, service_name, "endpoints").await? else {
        return Ok(Vec::new());
    };

    Ok(collect_endpoint_addresses(ep, target_port))
}

/// Resolves endpoints via `EndpointSlice` resources.
async fn resolve_via_endpoint_slices(
    client: &Client,
    namespace: &str,
    service_name: &str,
    target_port: i32,
) -> Result<Vec<String>> {
    let api = Api::<EndpointSlice>::namespaced(client.clone(), namespace);
    let label = format!("kubernetes.io/service-name={service_name}");
    let list = api.list(&ListParams::default().labels(&label)).await?;

    let mut addrs = Vec::new();
    for slice in list.items {
        collect_slice_addresses(&slice, target_port, &mut addrs);
    }
    Ok(addrs)
}

/// Collects ready addresses from a single `EndpointSlice`.
fn collect_slice_addresses(slice: &EndpointSlice, target_port: i32, out: &mut Vec<String>) {
    let port = resolve_slice_port(slice, target_port);

    for ep in &slice.endpoints {
        if !is_endpoint_ready(ep) {
            continue;
        }
        for addr in &ep.addresses {
            out.push(format!("{addr}:{port}"));
        }
    }
}

/// Checks if an endpoint is ready (or serving).
fn is_endpoint_ready(ep: &k8s_openapi::api::discovery::v1::Endpoint) -> bool {
    ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true)
}

/// Resolves the port from an `EndpointSlice`, falling back to `target_port`.
fn resolve_slice_port(slice: &EndpointSlice, target_port: i32) -> i32 {
    slice
        .ports
        .as_ref()
        .and_then(|ports| {
            ports
                .iter()
                .find(|p| p.port == Some(target_port))
                .or_else(|| ports.first())
        })
        .and_then(|p| p.port)
        .unwrap_or(target_port)
}

/// Fetches a namespaced resource, returning `None` on 404.
async fn get_or_none<K>(client: &Client, namespace: &str, name: &str, kind_label: &str) -> Result<Option<K>>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + serde::de::DeserializeOwned
        + Clone
        + std::fmt::Debug
        + Send
        + Sync,
    K::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), namespace);
    match api.get(name).await {
        Ok(r) => Ok(Some(r)),
        Err(e) => not_found_to_none(e, kind_label, namespace, name),
    }
}

/// Converts a 404 API error to `Ok(None)`, propagating all other errors.
fn not_found_to_none<T>(e: kube::Error, kind: &str, ns: &str, name: &str) -> Result<Option<T>> {
    if let kube::Error::Api(resp) = &e
        && resp.code == 404
    {
        debug!("{kind} {ns}/{name} not found");
        return Ok(None);
    }
    Err(e.into())
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Collects `ip:port` addresses from an [`Endpoints`] resource.
///
/// For each subset, resolves the matching port (or falls back to
/// `target_port`) and pairs it with each ready address.
fn collect_endpoint_addresses(ep: Endpoints, target_port: i32) -> Vec<String> {
    ep.subsets
        .unwrap_or_default()
        .into_iter()
        .flat_map(|subset| {
            let resolved_port = subset
                .ports
                .as_ref()
                .and_then(|ports| ports.iter().find(|p| p.port == target_port).or_else(|| ports.first()))
                .map_or(target_port, |p| p.port);

            subset
                .addresses
                .unwrap_or_default()
                .into_iter()
                .map(move |addr| {
                    let ip = &addr.ip;
                    format!("{ip}:{resolved_port}")
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Resolves a `Service` port number to its target port.
///
/// Finds the `ServicePort` matching `service_port` and returns its
/// `targetPort` (falling back to the service port if unset).
fn resolve_target_port(svc: &Service, service_port: i32) -> i32 {
    svc.spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .and_then(|ports| ports.iter().find(|p| p.port == service_port))
        .and_then(|sp| {
            sp.target_port.as_ref().map(|tp| match tp {
                k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(n) => *n,
                k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(_) => service_port,
            })
        })
        .unwrap_or(service_port)
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
    use k8s_openapi::{
        api::{
            core::v1::{EndpointAddress, EndpointPort, EndpointSubset, ServicePort, ServiceSpec},
            discovery::v1::{Endpoint, EndpointConditions},
        },
        apimachinery::pkg::util::intstr::IntOrString,
    };
    use kube::core::Status;

    use super::*;

    #[test]
    fn test_resolve_target_port_numeric_target() {
        let svc = service(&[(80, Some(IntOrString::Int(8080)))]);

        assert_eq!(
            resolve_target_port(&svc, 80),
            8080,
            "a numeric targetPort should be used verbatim"
        );
    }

    #[test]
    fn test_resolve_target_port_without_target_falls_back() {
        let svc = service(&[(80, None)]);

        assert_eq!(
            resolve_target_port(&svc, 80),
            80,
            "an unset targetPort defaults to the service port"
        );
    }

    #[test]
    fn test_resolve_target_port_named_target_falls_back() {
        let svc = service(&[(80, Some(IntOrString::String("http".to_owned())))]);

        assert_eq!(
            resolve_target_port(&svc, 80),
            80,
            "a named targetPort cannot be resolved here and falls back to the service port"
        );
    }

    #[test]
    fn test_resolve_target_port_selects_the_matching_service_port() {
        let svc = service(&[(80, Some(IntOrString::Int(8080))), (443, Some(IntOrString::Int(8443)))]);

        assert_eq!(
            resolve_target_port(&svc, 443),
            8443,
            "the ServicePort matching the requested port should win"
        );
    }

    #[test]
    fn test_resolve_target_port_unknown_port_falls_back() {
        let svc = service(&[(80, Some(IntOrString::Int(8080)))]);

        assert_eq!(
            resolve_target_port(&svc, 9000),
            9000,
            "an unmatched service port falls back to itself"
        );
    }

    #[test]
    fn test_resolve_target_port_without_spec() {
        let svc = Service::default();

        assert_eq!(
            resolve_target_port(&svc, 80),
            80,
            "a Service without a spec falls back to the requested port"
        );
    }

    #[test]
    fn test_is_endpoint_ready_defaults_to_true() {
        assert!(
            is_endpoint_ready(&endpoint("10.0.0.1", None)),
            "an endpoint without conditions is assumed ready"
        );
    }

    #[test]
    fn test_is_endpoint_ready_honours_conditions() {
        assert!(
            is_endpoint_ready(&endpoint("10.0.0.1", Some(true))),
            "a ready endpoint should be used"
        );
        assert!(
            !is_endpoint_ready(&endpoint("10.0.0.1", Some(false))),
            "an unready endpoint must be excluded"
        );
    }

    #[test]
    fn test_resolve_slice_port_prefers_the_matching_port() {
        let slice = endpoint_slice(&[8080, 9090], &[]);

        assert_eq!(
            resolve_slice_port(&slice, 9090),
            9090,
            "the slice port equal to the target port should be chosen"
        );
    }

    #[test]
    fn test_resolve_slice_port_falls_back_to_the_first_port() {
        let slice = endpoint_slice(&[8080, 9090], &[]);

        assert_eq!(
            resolve_slice_port(&slice, 1234),
            8080,
            "an unmatched target port falls back to the first slice port"
        );
    }

    #[test]
    fn test_resolve_slice_port_without_ports() {
        let mut slice = endpoint_slice(&[], &[]);
        slice.ports = None;

        assert_eq!(
            resolve_slice_port(&slice, 8080),
            8080,
            "a slice without ports falls back to the target port"
        );
    }

    #[test]
    fn test_collect_slice_addresses_skips_unready_endpoints() {
        let slice = endpoint_slice(&[8080], &[("10.0.0.1", Some(true)), ("10.0.0.2", Some(false))]);
        let mut out = Vec::new();

        collect_slice_addresses(&slice, 8080, &mut out);

        assert_eq!(
            out,
            vec!["10.0.0.1:8080".to_owned()],
            "only ready endpoints belong in the data-plane config"
        );
    }

    #[test]
    fn test_collect_endpoint_addresses_matches_port() {
        let ep = endpoints_resource(&[8080, 9090], &["10.0.0.1", "10.0.0.2"]);

        assert_eq!(
            collect_endpoint_addresses(ep, 9090),
            vec!["10.0.0.1:9090".to_owned(), "10.0.0.2:9090".to_owned()],
            "every address should be paired with the matching subset port"
        );
    }

    #[test]
    fn test_collect_endpoint_addresses_falls_back_to_the_first_port() {
        let ep = endpoints_resource(&[8080, 9090], &["10.0.0.1"]);

        assert_eq!(
            collect_endpoint_addresses(ep, 1234),
            vec!["10.0.0.1:8080".to_owned()],
            "an unmatched target port falls back to the first subset port"
        );
    }

    #[test]
    fn test_collect_endpoint_addresses_without_subsets() {
        assert!(
            collect_endpoint_addresses(Endpoints::default(), 80).is_empty(),
            "an Endpoints resource with no subsets yields no addresses"
        );
    }

    #[test]
    fn test_not_found_to_none_maps_404() {
        let result = not_found_to_none::<Service>(api_error(404), "service", "apps", "svc");

        assert!(
            matches!(result, Ok(None)),
            "a 404 should be reported as an absent resource, not an error"
        );
    }

    #[test]
    fn test_not_found_to_none_propagates_other_errors() {
        let result = not_found_to_none::<Service>(api_error(403), "service", "apps", "svc");

        assert!(result.is_err(), "a non-404 API error must propagate");
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds a Service exposing the given `(port, target_port)` pairs.
    fn service(ports: &[(i32, Option<IntOrString>)]) -> Service {
        Service {
            spec: Some(ServiceSpec {
                ports: Some(
                    ports
                        .iter()
                        .map(|(port, target)| ServicePort {
                            port: *port,
                            target_port: target.clone(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Builds an `EndpointSlice` endpoint with an optional ready condition.
    fn endpoint(address: &str, ready: Option<bool>) -> Endpoint {
        Endpoint {
            addresses: vec![address.to_owned()],
            conditions: ready.map(|r| EndpointConditions {
                ready: Some(r),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Builds an `EndpointSlice` with the given ports and endpoints.
    fn endpoint_slice(ports: &[i32], addresses: &[(&str, Option<bool>)]) -> EndpointSlice {
        EndpointSlice {
            address_type: "IPv4".to_owned(),
            endpoints: addresses.iter().map(|(a, r)| endpoint(a, *r)).collect(),
            metadata: Default::default(),
            ports: Some(
                ports
                    .iter()
                    .map(|p| k8s_openapi::api::discovery::v1::EndpointPort {
                        port: Some(*p),
                        ..Default::default()
                    })
                    .collect(),
            ),
        }
    }

    /// Builds a classic `Endpoints` resource with one subset.
    fn endpoints_resource(ports: &[i32], addresses: &[&str]) -> Endpoints {
        Endpoints {
            subsets: Some(vec![EndpointSubset {
                addresses: Some(
                    addresses
                        .iter()
                        .map(|a| EndpointAddress {
                            ip: (*a).to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ports: Some(
                    ports
                        .iter()
                        .map(|p| EndpointPort {
                            port: *p,
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// Builds a kube API error carrying the given HTTP status code.
    fn api_error(code: u16) -> kube::Error {
        kube::Error::Api(Box::new(Status {
            code,
            message: "boom".to_owned(),
            reason: "Boom".to_owned(),
            ..Default::default()
        }))
    }
}
