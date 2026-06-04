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
    if let kube::Error::Api(ref resp) = e
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
