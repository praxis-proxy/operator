// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Benchmarks for Praxis configuration generation.
//!
//! Config generation runs on every Gateway reconcile, and the route set
//! is the input that grows without bound in a real cluster. These
//! measure how conversion scales with it so a regression shows up as a
//! number rather than as a slow conformance run.
//!
//! The benchmark drives the operator's own converters, not a stand-in.
//! Everything the reconciler does between "here are the routes" and
//! "here is the YAML" is measured: parent-ref attachment, hostname
//! intersection, rule conversion, cluster assembly, and serialization.
//! Endpoint resolution is the one step left out — it is an API round
//! trip, so its cost is latency rather than CPU, and including it would
//! measure a fake client instead of the operator.

#![expect(
    missing_docs,
    reason = "criterion_group and criterion_main generate undocumented items"
)]

use std::{collections::HashMap, hint::black_box};

use criterion::{Criterion, criterion_group, criterion_main};
use fixtures::{GATEWAY_NAME, GATEWAY_NAMESPACE, listener_manifests, route_manifests};
use gateway_api::gateways::GatewayListeners;
use praxis_operator::{
    config::{
        cluster::{PraxisCluster, build_cluster},
        generate::assemble_config,
        listener::convert_listener,
        routing::{BackendRef, convert_routes},
    },
    gateway_api::attachment::attached_routes,
};

// -----------------------------------------------------------------------------
// Route Set Sizes
// -----------------------------------------------------------------------------

/// Route counts the conversion benchmark sweeps.
///
/// Spans a small cluster through one large enough that quadratic
/// behaviour would be obvious.
const ROUTE_COUNTS: [usize; 4] = [1, 10, 100, 500];

/// Endpoints synthesized per backend `Service`.
///
/// Stands in for what endpoint resolution would have returned, so
/// cluster assembly and serialization see a realistic amount of data.
const ENDPOINTS_PER_SERVICE: usize = 3;

// -----------------------------------------------------------------------------
// Benchmarks
// -----------------------------------------------------------------------------

/// Measures the full route-to-YAML pipeline across growing route sets.
fn bench_config_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("config_generation");
    let listeners = listener_manifests();

    for count in ROUTE_COUNTS {
        group.bench_function(format!("{count}_routes"), |b| {
            let routes = route_manifests(count);
            b.iter(|| black_box(generate_config(&listeners, &routes)));
        });
    }

    group.finish();
}

/// Measures attachment alone, which every reconcile pays per Gateway.
///
/// Split out because it scales with the cluster-wide route count rather
/// than with the routes that actually attach: a Gateway with no routes
/// of its own still walks the whole list.
fn bench_attachment(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_attachment");

    for count in ROUTE_COUNTS {
        group.bench_function(format!("{count}_routes"), |b| {
            let routes = route_manifests(count);
            b.iter(|| black_box(attached_routes(GATEWAY_NAME, GATEWAY_NAMESPACE, &routes).len()));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_config_generation, bench_attachment);
criterion_main!(benches);

// -----------------------------------------------------------------------------
// Pipeline
// -----------------------------------------------------------------------------

/// Runs the synchronous half of `build_praxis_config` and returns the
/// serialized length, which keeps the optimizer from eliding the work.
fn generate_config(listeners: &[GatewayListeners], routes: &[gateway_api::httproutes::HTTPRoute]) -> usize {
    let attached = attached_routes(GATEWAY_NAME, GATEWAY_NAMESPACE, routes);
    let listener_hostnames: HashMap<String, Option<String>> =
        listeners.iter().map(|l| (l.name.clone(), l.hostname.clone())).collect();

    let praxis_listeners: Vec<_> = listeners
        .iter()
        .map(|l| convert_listener(l, &format!("{}-chain", l.name)))
        .collect();

    let (praxis_routes, backend_refs) = convert_routes(&attached, &listener_hostnames, &[]);
    let clusters = synthesize_clusters(&backend_refs);

    assemble_config(praxis_listeners, &praxis_routes, &clusters, &[], &listener_hostnames)
        .ok()
        .and_then(|config| serde_norway::to_string(&config).ok())
        .map_or(0, |yaml| yaml.len())
}

/// Builds clusters with fixed endpoints, standing in for the API reads
/// that resolution would otherwise perform.
fn synthesize_clusters(backend_refs: &[BackendRef]) -> Vec<PraxisCluster> {
    backend_refs
        .iter()
        .map(|backend| {
            let endpoints = (0..ENDPOINTS_PER_SERVICE)
                .map(|i| format!("10.0.{i}.1:{}", backend.port))
                .collect();
            build_cluster(&backend.cluster_name, endpoints, None)
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------

/// Gateway API manifests the benchmark converts.
mod fixtures {
    use gateway_api::{
        gateways::GatewayListeners,
        httproutes::{
            HTTPRoute, HttpRouteParentRefs, HttpRouteRules, HttpRouteRulesBackendRefs, HttpRouteRulesMatches,
            HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType, HttpRouteSpec,
        },
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    /// Name of the Gateway every generated route attaches to.
    pub(super) const GATEWAY_NAME: &str = "bench-gateway";

    /// Namespace holding the Gateway and every generated route.
    pub(super) const GATEWAY_NAMESPACE: &str = "default";

    /// Builds the listener set routes attach to.
    ///
    /// One cleartext listener with no hostname constraint, so hostname
    /// intersection runs on every route without discarding any.
    pub(super) fn listener_manifests() -> Vec<GatewayListeners> {
        vec![GatewayListeners {
            name: "http".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            ..Default::default()
        }]
    }

    /// Builds `count` routes, each with one match and one backend.
    pub(super) fn route_manifests(count: usize) -> Vec<HTTPRoute> {
        (0..count).map(build_route).collect()
    }

    /// Builds one route with a distinct path, hostname, and backend.
    fn build_route(index: usize) -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some(format!("route-{index}")),
                namespace: Some(GATEWAY_NAMESPACE.to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec![format!("host-{index}.example.com")]),
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: GATEWAY_NAME.to_owned(),
                    ..Default::default()
                }]),
                rules: Some(vec![build_rule(index)]),
            },
            status: None,
        }
    }

    /// Builds one rule matching a distinct prefix onto a distinct backend.
    fn build_rule(index: usize) -> HttpRouteRules {
        HttpRouteRules {
            backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                name: format!("svc-{index}"),
                port: Some(8080),
                ..Default::default()
            }]),
            matches: Some(vec![HttpRouteRulesMatches {
                path: Some(HttpRouteRulesMatchesPath {
                    r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                    value: Some(format!("/api/{index}")),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }
}
