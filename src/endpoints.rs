// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Kubernetes Service endpoint resolution.

use k8s_openapi::{
    api::{
        core::v1::{Endpoints, Service},
        discovery::v1::EndpointSlice,
    },
    apimachinery::pkg::util::intstr::IntOrString,
};
use kube::{Api, Client, api::ListParams};
use tracing::debug;

use crate::error::Result;

// -----------------------------------------------------------------------------
// TargetPort
// -----------------------------------------------------------------------------

/// Criteria for selecting the endpoint port backing a `Service` port.
///
/// Endpoint ports are matched by name first, because that is the only
/// way to resolve a named `targetPort`, then by number, then by
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetPort {
    /// `ServicePort` name, matched against endpoint port names.
    name: Option<String>,

    /// Numeric `targetPort`, when the `Service` declares one.
    number: Option<i32>,

    /// Service port, used when nothing else resolves.
    service_port: i32,
}

impl TargetPort {
    /// Returns the port number to use when no endpoint port matches.
    fn fallback(&self) -> i32 {
        self.number.unwrap_or(self.service_port)
    }

    /// Returns `true` when an endpoint port name identifies this port.
    fn matches_name(&self, candidate: Option<&str>) -> bool {
        match (self.name.as_deref(), candidate) {
            (Some(want), Some(got)) => want == got,
            _ => false,
        }
    }
}

// -----------------------------------------------------------------------------
// Endpoint Resolution
// -----------------------------------------------------------------------------

/// Resolves ready endpoint addresses for a Kubernetes Service.
///
/// Tries `EndpointSlice` first (supports headless and manual endpoints),
/// falling back to classic Endpoints for backwards compatibility.
///
/// # Errors
///
/// Returns an error if listing `EndpointSlices` or reading the
/// `Service` fails for any reason other than the object being absent.
/// A missing `Service` yields an empty address list, not an error, so
/// the caller can report `BackendNotFound` on the route instead.
pub async fn resolve_endpoints(
    client: &Client,
    namespace: &str,
    service_name: &str,
    service_port: i32,
) -> Result<Vec<String>> {
    let Some(svc) = get_or_none(client, namespace, service_name, "service").await? else {
        return Ok(Vec::new());
    };
    let target = resolve_target_port(&svc, service_port);

    match resolve_via_endpoint_slices(client, namespace, service_name, &target).await {
        Ok(eps) if !eps.is_empty() => return Ok(eps),
        Ok(_) => {},
        Err(err) => debug!("EndpointSlice lookup failed, falling back to Endpoints: {err}"),
    }

    let Some(ep) = get_or_none::<Endpoints>(client, namespace, service_name, "endpoints").await? else {
        return Ok(Vec::new());
    };

    Ok(collect_endpoint_addresses(ep, &target))
}

/// Resolves endpoints via `EndpointSlice` resources.
async fn resolve_via_endpoint_slices(
    client: &Client,
    namespace: &str,
    service_name: &str,
    target: &TargetPort,
) -> Result<Vec<String>> {
    let api = Api::<EndpointSlice>::namespaced(client.clone(), namespace);
    let label = format!("kubernetes.io/service-name={service_name}");
    let list = api.list(&ListParams::default().labels(&label)).await?;

    let mut addrs = Vec::new();
    for slice in list.items {
        collect_slice_addresses(&slice, target, &mut addrs);
    }
    Ok(addrs)
}

/// Collects ready addresses from a single `EndpointSlice`.
fn collect_slice_addresses(slice: &EndpointSlice, target: &TargetPort, out: &mut Vec<String>) {
    let port = resolve_slice_port(slice, target);

    for ep in &slice.endpoints {
        if !is_endpoint_ready(ep) {
            continue;
        }
        for addr in &ep.addresses {
            out.push(format!("{addr}:{port}"));
        }
    }
}

/// Checks whether an endpoint should receive traffic.
///
/// Follows the `EndpointSlice` condition contract: an endpoint is usable
/// when `ready` is true, or when `ready` is unset and `serving` is true.
/// A terminating endpoint is never usable, whatever the other conditions
/// say, so pods being drained stop receiving traffic.
fn is_endpoint_ready(ep: &k8s_openapi::api::discovery::v1::Endpoint) -> bool {
    let Some(conditions) = ep.conditions.as_ref() else {
        return true;
    };

    if conditions.terminating == Some(true) {
        return false;
    }

    match conditions.ready {
        Some(ready) => ready,
        None => conditions.serving.unwrap_or(true),
    }
}

/// Resolves the port from an `EndpointSlice`.
///
/// Prefers the port whose name matches the `ServicePort` name, then a
/// numeric match, then the first declared port.
fn resolve_slice_port(slice: &EndpointSlice, target: &TargetPort) -> i32 {
    slice
        .ports
        .as_ref()
        .and_then(|ports| {
            ports
                .iter()
                .find(|port_entry| target.matches_name(port_entry.name.as_deref()))
                .or_else(|| {
                    target
                        .number
                        .and_then(|num| ports.iter().find(|port_entry| port_entry.port == Some(num)))
                })
                .or_else(|| ports.first())
        })
        .and_then(|port_entry| port_entry.port)
        .unwrap_or_else(|| target.fallback())
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
        Ok(resource) => Ok(Some(resource)),
        Err(err) => not_found_to_none(err, kind_label, namespace, name),
    }
}

/// Converts a 404 API error to `Ok(None)`, propagating all other errors.
fn not_found_to_none<T>(err: kube::Error, kind: &str, ns: &str, name: &str) -> Result<Option<T>> {
    if let kube::Error::Api(resp) = &err
        && resp.code == 404
    {
        debug!("{kind} {ns}/{name} not found");
        return Ok(None);
    }
    Err(err.into())
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Collects `ip:port` addresses from an [`Endpoints`] resource.
///
/// For each subset, resolves the port by name, then number, then
/// position, and pairs it with each ready address.
fn collect_endpoint_addresses(ep: Endpoints, target: &TargetPort) -> Vec<String> {
    ep.subsets
        .unwrap_or_default()
        .into_iter()
        .flat_map(|subset| {
            let resolved_port = subset
                .ports
                .as_ref()
                .and_then(|ports| {
                    ports
                        .iter()
                        .find(|port_entry| target.matches_name(port_entry.name.as_deref()))
                        .or_else(|| {
                            target
                                .number
                                .and_then(|num| ports.iter().find(|port_entry| port_entry.port == num))
                        })
                        .or_else(|| ports.first())
                })
                .map_or_else(|| target.fallback(), |port_entry| port_entry.port);

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

/// Resolves a `Service` port into the criteria for picking an endpoint
/// port.
///
/// A `targetPort` given as a name cannot be resolved from the `Service`
/// alone; it is resolved later against the endpoint port names, which
/// Kubernetes derives from `Service.ports[].name`.
fn resolve_target_port(svc: &Service, service_port: i32) -> TargetPort {
    let matching = svc
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .and_then(|ports| ports.iter().find(|port_entry| port_entry.port == service_port));

    let Some(sp) = matching else {
        return TargetPort {
            name: None,
            number: None,
            service_port,
        };
    };

    let number = match sp.target_port.as_ref() {
        Some(IntOrString::Int(num)) => Some(*num),
        Some(IntOrString::String(_)) | None => None,
    };

    TargetPort {
        name: sp.name.clone().filter(|nm| !nm.is_empty()),
        number,
        service_port,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::default_trait_access, reason = "tests")]
mod tests {
    use k8s_openapi::api::{
        core::v1::{EndpointAddress, EndpointPort, EndpointSubset, ServicePort, ServiceSpec},
        discovery::v1::{Endpoint, EndpointConditions},
    };
    use kube::core::Status;

    use super::*;

    #[test]
    fn test_resolve_target_port_numeric_target() {
        let svc = service(&[(80, Some(IntOrString::Int(8080)))]);

        assert_eq!(
            resolve_target_port(&svc, 80).number,
            Some(8080),
            "a numeric targetPort should be carried through"
        );
    }

    #[test]
    fn test_resolve_target_port_without_target_falls_back() {
        let svc = service(&[(80, None)]);

        assert_eq!(
            resolve_target_port(&svc, 80).fallback(),
            80,
            "an unset targetPort defaults to the service port"
        );
    }

    #[test]
    fn test_resolve_target_port_named_target_defers_to_the_port_name() {
        let mut svc = service(&[(80, Some(IntOrString::String("http".to_owned())))]);
        set_port_name(&mut svc, 80, "web");
        let target = resolve_target_port(&svc, 80);

        assert_eq!(target.number, None, "a named targetPort has no number to match on");
        assert_eq!(
            target.name,
            Some("web".to_owned()),
            "the ServicePort name is what endpoint ports are keyed by"
        );
    }

    #[test]
    fn test_resolve_target_port_selects_the_matching_service_port() {
        let svc = service(&[(80, Some(IntOrString::Int(8080))), (443, Some(IntOrString::Int(8443)))]);

        assert_eq!(
            resolve_target_port(&svc, 443).number,
            Some(8443),
            "the ServicePort matching the requested port should win"
        );
    }

    #[test]
    fn test_resolve_target_port_unknown_port_falls_back() {
        let svc = service(&[(80, Some(IntOrString::Int(8080)))]);

        assert_eq!(
            resolve_target_port(&svc, 9000).fallback(),
            9000,
            "an unmatched service port falls back to itself"
        );
    }

    #[test]
    fn test_resolve_target_port_without_spec() {
        let svc = Service::default();

        assert_eq!(
            resolve_target_port(&svc, 80).fallback(),
            80,
            "a Service without a spec falls back to the requested port"
        );
    }

    #[test]
    fn test_resolve_slice_port_prefers_the_named_port() {
        let mut svc = service(&[(80, Some(IntOrString::String("http".to_owned()))), (443, None)]);
        set_port_name(&mut svc, 80, "web");
        let target = resolve_target_port(&svc, 80);
        let slice = named_endpoint_slice(&[(Some("admin"), 9901), (Some("web"), 8080)]);

        assert_eq!(
            resolve_slice_port(&slice, &target),
            8080,
            "a named targetPort must resolve through the matching endpoint port name, not position"
        );
    }

    #[test]
    fn test_resolve_slice_port_name_beats_number() {
        let mut svc = service(&[(80, Some(IntOrString::Int(9999)))]);
        set_port_name(&mut svc, 80, "web");
        let target = resolve_target_port(&svc, 80);
        let slice = named_endpoint_slice(&[(Some("other"), 9999), (Some("web"), 8080)]);

        assert_eq!(
            resolve_slice_port(&slice, &target),
            8080,
            "the port name is more specific than a numeric match"
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
    fn test_is_endpoint_ready_excludes_terminating() {
        assert!(
            !is_endpoint_ready(&endpoint_with_conditions(Some(true), Some(true), Some(true))),
            "a terminating endpoint must be excluded even while still ready"
        );
    }

    #[test]
    fn test_is_endpoint_ready_falls_back_to_serving() {
        assert!(
            is_endpoint_ready(&endpoint_with_conditions(None, Some(true), Some(false))),
            "an endpoint without ready should fall back to serving"
        );
        assert!(
            !is_endpoint_ready(&endpoint_with_conditions(None, Some(false), None)),
            "an endpoint that is neither ready nor serving must be excluded"
        );
    }

    #[test]
    fn test_is_endpoint_ready_ignores_serving_when_ready_is_set() {
        assert!(
            !is_endpoint_ready(&endpoint_with_conditions(Some(false), Some(true), None)),
            "an explicit ready=false must win over serving=true"
        );
    }

    #[test]
    fn test_resolve_slice_port_prefers_the_matching_port() {
        let slice = endpoint_slice(&[8080, 9090], &[]);

        assert_eq!(
            resolve_slice_port(&slice, &numeric_target(9090)),
            9090,
            "the slice port equal to the target port should be chosen"
        );
    }

    #[test]
    fn test_resolve_slice_port_falls_back_to_the_first_port() {
        let slice = endpoint_slice(&[8080, 9090], &[]);

        assert_eq!(
            resolve_slice_port(&slice, &numeric_target(1234)),
            8080,
            "an unmatched target port falls back to the first slice port"
        );
    }

    #[test]
    fn test_resolve_slice_port_without_ports() {
        let mut slice = endpoint_slice(&[], &[]);
        slice.ports = None;

        assert_eq!(
            resolve_slice_port(&slice, &numeric_target(8080)),
            8080,
            "a slice without ports falls back to the target port"
        );
    }

    #[test]
    fn test_collect_slice_addresses_skips_unready_endpoints() {
        let slice = endpoint_slice(&[8080], &[("10.0.0.1", Some(true)), ("10.0.0.2", Some(false))]);
        let mut out = Vec::new();

        collect_slice_addresses(&slice, &numeric_target(8080), &mut out);

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
            collect_endpoint_addresses(ep, &numeric_target(9090)),
            vec!["10.0.0.1:9090".to_owned(), "10.0.0.2:9090".to_owned()],
            "every address should be paired with the matching subset port"
        );
    }

    #[test]
    fn test_collect_endpoint_addresses_falls_back_to_the_first_port() {
        let ep = endpoints_resource(&[8080, 9090], &["10.0.0.1"]);

        assert_eq!(
            collect_endpoint_addresses(ep, &numeric_target(1234)),
            vec!["10.0.0.1:8080".to_owned()],
            "an unmatched target port falls back to the first subset port"
        );
    }

    #[test]
    fn test_collect_endpoint_addresses_without_subsets() {
        assert!(
            collect_endpoint_addresses(Endpoints::default(), &numeric_target(80)).is_empty(),
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

    /// Builds a [`TargetPort`] that matches purely on a port number.
    fn numeric_target(port: i32) -> TargetPort {
        TargetPort {
            name: None,
            number: Some(port),
            service_port: port,
        }
    }

    /// Sets the `name` of the `ServicePort` with the given port number.
    fn set_port_name(svc: &mut Service, port: i32, name: &str) {
        let ports = svc
            .spec
            .as_mut()
            .and_then(|spec| spec.ports.as_mut())
            .expect("service should declare ports");
        let sp = ports
            .iter_mut()
            .find(|p| p.port == port)
            .expect("service should declare the requested port");
        sp.name = Some(name.to_owned());
    }

    /// Builds an `EndpointSlice` with explicitly named ports.
    fn named_endpoint_slice(ports: &[(Option<&str>, i32)]) -> EndpointSlice {
        EndpointSlice {
            address_type: "IPv4".to_owned(),
            endpoints: Vec::new(),
            metadata: Default::default(),
            ports: Some(
                ports
                    .iter()
                    .map(|(name, port)| k8s_openapi::api::discovery::v1::EndpointPort {
                        name: name.map(str::to_owned),
                        port: Some(*port),
                        ..Default::default()
                    })
                    .collect(),
            ),
        }
    }

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

    /// Builds an endpoint with explicit `ready`, `serving` and
    /// `terminating` conditions.
    fn endpoint_with_conditions(ready: Option<bool>, serving: Option<bool>, terminating: Option<bool>) -> Endpoint {
        Endpoint {
            addresses: vec!["10.0.0.1".to_owned()],
            conditions: Some(EndpointConditions {
                ready,
                serving,
                terminating,
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
