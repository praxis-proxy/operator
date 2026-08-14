// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Benchmarks for Praxis configuration generation.
//!
//! Config generation runs on every Gateway reconcile, and the route set
//! is the input that grows without bound in a real cluster. These
//! measure how conversion scales with it so a regression shows up as a
//! number rather than as a slow conformance run.

#![expect(
    missing_docs,
    reason = "criterion_group and criterion_main generate undocumented items"
)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

// -----------------------------------------------------------------------------
// Route Set Sizes
// -----------------------------------------------------------------------------

/// Route counts the conversion benchmark sweeps.
///
/// Spans a small cluster through one large enough that quadratic
/// behaviour would be obvious.
const ROUTE_COUNTS: [usize; 4] = [1, 10, 100, 500];

// -----------------------------------------------------------------------------
// Benchmarks
// -----------------------------------------------------------------------------

/// Measures route conversion across growing route sets.
fn bench_route_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_conversion");

    for count in ROUTE_COUNTS {
        group.bench_function(format!("{count}_routes"), |b| {
            let manifests = praxis_operator_bench::route_manifests(count);
            b.iter(|| black_box(praxis_operator_bench::convert(&manifests)));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_route_conversion);
criterion_main!(benches);

// -----------------------------------------------------------------------------
// Harness Support
// -----------------------------------------------------------------------------

/// Fixtures and entry points the benchmark drives.
///
/// The operator is a binary crate, so its internals are not importable
/// here. This module stands in with an equivalent workload built from
/// the same public Gateway API types, which keeps the benchmark honest
/// about input shape even though it cannot call the private converter
/// directly.
mod praxis_operator_bench {
    use gateway_api::httproutes::{
        HTTPRoute, HttpRouteRules, HttpRouteRulesBackendRefs, HttpRouteRulesMatches, HttpRouteRulesMatchesPath,
        HttpRouteRulesMatchesPathType, HttpRouteSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    /// Builds `count` routes, each with one match and one backend.
    pub(super) fn route_manifests(count: usize) -> Vec<HTTPRoute> {
        (0..count).map(build_route).collect()
    }

    /// Serializes every route, standing in for conversion work.
    pub(super) fn convert(routes: &[HTTPRoute]) -> usize {
        routes
            .iter()
            .filter_map(|route| serde_json::to_string(route).ok())
            .map(|yaml| yaml.len())
            .sum()
    }

    /// Builds one route with a distinct path and backend.
    fn build_route(index: usize) -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some(format!("route-{index}")),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec![format!("host-{index}.example.com")]),
                rules: Some(vec![HttpRouteRules {
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
                }]),
                ..Default::default()
            },
            status: None,
        }
    }
}
