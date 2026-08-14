// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Routing configuration generation for Praxis proxy.

use std::collections::{BTreeMap, HashMap, HashSet};

use gateway_api::{
    httproutes::{HTTPRoute, HttpRouteRulesBackendRefs, HttpRouteRulesMatches, HttpRouteRulesMatchesPathType},
    referencegrants::ReferenceGrant,
};
use serde::Serialize;
use tracing::warn;

use crate::gateway_api::{hostname::intersect_hostnames, reference_grant::is_reference_allowed};

// -----------------------------------------------------------------------------
// PraxisRoute
// -----------------------------------------------------------------------------

/// Praxis route configuration.
///
/// Represents a routing rule in the Praxis proxy config. Uses either `path`
/// (exact match) or `path_prefix` (prefix match). When `path` is set it
/// takes precedence over `path_prefix`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct PraxisRoute {
    /// Target cluster name.
    pub(crate) cluster: String,

    /// Request headers to match (exact match only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) headers: Option<BTreeMap<String, String>>,

    /// Hostname match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<String>,

    /// Listener names this route targets. `None` means all listeners.
    ///
    /// Used for per-listener route partitioning; not serialized.
    #[serde(skip)]
    pub(crate) listener_names: Vec<Option<String>>,

    /// Exact path match. Takes precedence over `path_prefix`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,

    /// Path prefix match. Must end with '/'.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) path_prefix: String,
}

// -----------------------------------------------------------------------------
// PraxisFilterEntry
// -----------------------------------------------------------------------------

/// Praxis filter entry.
///
/// Represents a filter in a filter chain with its configuration.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct PraxisFilterEntry {
    /// Filter name.
    pub(crate) filter: String,

    /// Filter configuration (flattened into parent).
    #[serde(flatten)]
    pub(crate) config: serde_yaml::Value,
}

// -----------------------------------------------------------------------------
// BackendRef
// -----------------------------------------------------------------------------

/// Resolved backend reference from an `HTTPRoute` rule.
///
/// Carries enough metadata for the gateway controller to resolve Kubernetes
/// `Service` endpoints into cluster `IP:port` pairs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackendRef {
    /// Cluster name (format: `namespace~service~port`).
    pub(crate) cluster_name: String,

    /// Kubernetes namespace of the backend Service.
    pub(crate) namespace: String,

    /// Backend Service port number.
    pub(crate) port: i32,

    /// Backend Service name.
    pub(crate) service: String,

    /// Traffic weight for weighted routing (Gateway API `backendRef.weight`).
    pub(crate) weight: Option<i32>,
}

// -----------------------------------------------------------------------------
// Route Conversion
// -----------------------------------------------------------------------------

/// Converts `HTTPRoutes` to Praxis route configurations.
///
/// Returns `(routes, backend_refs)`. Routes carry either an exact `path` or
/// a `path_prefix` (defaulting to `"/"`). `Backend` refs carry structured
/// metadata for endpoint resolution.
///
/// `listener_hostnames` maps listener section names to their hostname
/// constraints. Route hostnames are intersected with listener hostnames
/// per the Gateway API spec: only hostnames matching at least one
/// targeted listener are retained.
///
/// [`intersect_hostnames`]: crate::gateway_api::hostname::intersect_hostnames
pub(crate) fn convert_routes(
    routes: &[(&HTTPRoute, Vec<Option<String>>)],
    listener_hostnames: &HashMap<String, Option<String>>,
    grants: &[ReferenceGrant],
) -> (Vec<PraxisRoute>, Vec<BackendRef>) {
    let mut praxis_routes = Vec::new();
    let mut seen_clusters = HashSet::new();
    let mut backend_refs = Vec::new();

    for (route, section_names) in routes {
        let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
        let raw_hostnames = route.spec.hostnames.as_deref().unwrap_or(&[]);
        let effective = effective_hostnames(raw_hostnames, section_names, listener_hostnames);

        if !raw_hostnames.is_empty() && effective.is_empty() {
            continue;
        }

        if let Some(rules) = &route.spec.rules {
            for rule in rules {
                process_rule(
                    rule,
                    route_ns,
                    &effective,
                    section_names,
                    grants,
                    &mut seen_clusters,
                    &mut praxis_routes,
                    &mut backend_refs,
                );
            }
        }
    }

    (praxis_routes, backend_refs)
}

/// Computes the effective hostnames for a route after listener intersection.
///
/// Collects listener hostnames for each section name the route targets,
/// then intersects route hostnames with that set. Routes without
/// hostnames pass through unchanged (empty vec). When a route targets
/// all listeners (section name is `None`), all listener hostnames are
/// used for intersection.
fn effective_hostnames(
    route_hostnames: &[String],
    section_names: &[Option<String>],
    listener_hostnames: &HashMap<String, Option<String>>,
) -> Vec<String> {
    if route_hostnames.is_empty() {
        return Vec::new();
    }

    let listener_hosts: Vec<Option<String>> = collect_listener_hosts(section_names, listener_hostnames);
    intersect_hostnames(route_hostnames, &listener_hosts)
}

/// Gathers listener hostnames for the targeted sections.
///
/// Named sections resolve to their specific listener hostname.
/// `None` sections (no `sectionName`) expand to all listener hostnames.
fn collect_listener_hosts(
    section_names: &[Option<String>],
    listener_hostnames: &HashMap<String, Option<String>>,
) -> Vec<Option<String>> {
    let targets_all = section_names.iter().any(Option::is_none);
    if targets_all {
        return listener_hostnames.values().cloned().collect();
    }

    section_names
        .iter()
        .filter_map(|sn| sn.as_ref().and_then(|name| listener_hostnames.get(name)).cloned())
        .collect()
}

/// Processes a single rule: creates routes and collects backend refs.
#[expect(
    clippy::too_many_arguments,
    reason = "rule processing needs access to shared accumulators"
)]
fn process_rule(
    rule: &gateway_api::httproutes::HttpRouteRules,
    route_ns: &str,
    route_hostnames: &[String],
    section_names: &[Option<String>],
    grants: &[ReferenceGrant],
    seen_clusters: &mut HashSet<String>,
    praxis_routes: &mut Vec<PraxisRoute>,
    backend_refs: &mut Vec<BackendRef>,
) {
    let matches = rule.matches.as_deref().unwrap_or(&[]);
    let raw_backends = rule.backend_refs.as_deref().unwrap_or(&[]);

    if raw_backends.is_empty() {
        return;
    }

    let all_refs: Vec<_> = raw_backends.iter().collect();
    let cluster_name = merged_cluster_name_from_refs(&all_refs, route_ns);
    create_routes_for_backend(&cluster_name, matches, route_hostnames, section_names, praxis_routes);
    backend_refs.extend(
        raw_backends
            .iter()
            .filter(|b| check_backend_authorized(b, route_ns, grants))
            .filter_map(|b| process_backend_ref(b, route_ns, &cluster_name, seen_clusters)),
    );
}

/// Returns `true` if authorized; logs and returns `false` otherwise.
fn check_backend_authorized(backend: &HttpRouteRulesBackendRefs, route_ns: &str, grants: &[ReferenceGrant]) -> bool {
    if is_backend_authorized(backend, route_ns, grants) {
        return true;
    }
    tracing::debug!(backend = backend.name, "skipping unauthorized cross-namespace backend");
    false
}

/// Checks if a backend ref is authorized (same namespace or [`ReferenceGrant`]).
fn is_backend_authorized(backend: &HttpRouteRulesBackendRefs, route_ns: &str, grants: &[ReferenceGrant]) -> bool {
    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    is_reference_allowed(
        route_ns,
        "gateway.networking.k8s.io",
        "HTTPRoute",
        backend_ns,
        group,
        kind,
        Some(&backend.name),
        grants,
    )
}

/// Builds a merged cluster name from a pre-filtered list of backend refs.
fn merged_cluster_name_from_refs(backends: &[&HttpRouteRulesBackendRefs], route_ns: &str) -> String {
    if let [only] = backends {
        return single_cluster_name(only, route_ns);
    }
    backends
        .iter()
        .map(|b| single_cluster_name(b, route_ns))
        .collect::<Vec<_>>()
        .join("+")
}

/// Builds a cluster name for a single backend ref.
fn single_cluster_name(backend: &HttpRouteRulesBackendRefs, route_ns: &str) -> String {
    let namespace = backend.namespace.as_deref().unwrap_or(route_ns);
    let service = backend.name.as_str();
    let port = backend.port.unwrap_or(80);
    format!("{namespace}~{service}~{port}")
}

/// Resolves a single backend ref into a [`BackendRef`].
///
/// Returns `None` for unsupported backend kinds (non-empty group or
/// non-Service kind). Deduplicates by the combination of
/// `cluster_name` and service identity using `seen`.
fn process_backend_ref(
    backend: &HttpRouteRulesBackendRefs,
    route_ns: &str,
    cluster_name: &str,
    seen: &mut HashSet<String>,
) -> Option<BackendRef> {
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    if !group.is_empty() || kind != "Service" {
        tracing::debug!(group, kind, "skipping unsupported backend ref kind");
        return None;
    }

    let namespace = backend.namespace.as_deref().unwrap_or(route_ns);
    let service = backend.name.as_str();
    let port = backend.port.unwrap_or(80);
    let dedup_key = format!("{cluster_name}:{namespace}~{service}~{port}");

    if !seen.insert(dedup_key) {
        return None;
    }

    Some(BackendRef {
        cluster_name: cluster_name.to_owned(),
        namespace: namespace.to_owned(),
        service: service.to_owned(),
        port,
        weight: backend.weight,
    })
}

/// Creates [`PraxisRoute`] entries for a single backend cluster.
///
/// When `matches` is empty a catch-all route (`/`) is created. Each match
/// is expanded across all `hostnames`; if no hostnames are present a single
/// host-less route is emitted.
fn create_routes_for_backend(
    cluster: &str,
    matches: &[HttpRouteRulesMatches],
    hostnames: &[String],
    section_names: &[Option<String>],
    out: &mut Vec<PraxisRoute>,
) {
    if matches.is_empty() {
        emit_catchall_routes(cluster, hostnames, section_names, out);
    } else {
        for m in matches {
            emit_match_routes(cluster, m, hostnames, section_names, out);
        }
    }
}

/// Emits catch-all routes (prefix `/`) for a backend cluster.
fn emit_catchall_routes(
    cluster: &str,
    hostnames: &[String],
    section_names: &[Option<String>],
    out: &mut Vec<PraxisRoute>,
) {
    let path_prefix = "/".to_owned();
    if hostnames.is_empty() {
        out.push(PraxisRoute {
            path: None,
            path_prefix,
            host: None,
            headers: None,
            cluster: cluster.to_owned(),
            listener_names: section_names.to_vec(),
        });
    } else {
        for hostname in hostnames {
            out.push(PraxisRoute {
                path: None,
                path_prefix: path_prefix.clone(),
                host: Some(hostname.clone()),
                headers: None,
                cluster: cluster.to_owned(),
                listener_names: section_names.to_vec(),
            });
        }
    }
}

/// Emits routes for a single match entry, expanding across hostnames.
///
/// A single match may produce multiple path entries (for element-wise
/// prefix matching), each expanded across all hostnames.
fn emit_match_routes(
    cluster: &str,
    m: &HttpRouteRulesMatches,
    hostnames: &[String],
    section_names: &[Option<String>],
    out: &mut Vec<PraxisRoute>,
) {
    let path_entries = extract_path_match(&m.path);
    let headers = extract_headers(&m.headers);

    for (path, path_prefix) in path_entries {
        if hostnames.is_empty() {
            out.push(PraxisRoute {
                path,
                path_prefix,
                host: None,
                headers: headers.clone(),
                cluster: cluster.to_owned(),
                listener_names: section_names.to_vec(),
            });
        } else {
            for hostname in hostnames {
                out.push(PraxisRoute {
                    path: path.clone(),
                    path_prefix: path_prefix.clone(),
                    host: Some(hostname.clone()),
                    headers: headers.clone(),
                    cluster: cluster.to_owned(),
                    listener_names: section_names.to_vec(),
                });
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Extracts path match entries from an `HTTPRoute` path match.
///
/// Returns a vec of `(path, path_prefix)` pairs:
/// - `PathPrefix` without trailing `/`: emits two entries for element-wise matching (exact `"/foo"` + prefix
///   `"/foo/"`), preventing `/foobar` from matching while allowing `/foo` and `/foo/anything`.
/// - `PathPrefix` with trailing `/` or `"/"`: single prefix entry.
/// - `Exact`: single entry with `path` set, empty `path_prefix`.
/// - Default / unsupported: single entry with prefix `"/"`.
fn extract_path_match(
    path: &Option<gateway_api::httproutes::HttpRouteRulesMatchesPath>,
) -> Vec<(Option<String>, String)> {
    let Some(p) = path.as_ref() else {
        return vec![(None, "/".to_owned())];
    };

    match &p.r#type {
        Some(HttpRouteRulesMatchesPathType::PathPrefix) => {
            let value = p.value.as_deref().unwrap_or("/");
            if value == "/" || value.ends_with('/') {
                vec![(None, value.to_owned())]
            } else {
                vec![(Some(value.to_owned()), String::new()), (None, format!("{value}/"))]
            }
        },
        Some(HttpRouteRulesMatchesPathType::Exact) => {
            let value = p.value.clone().unwrap_or_else(|| "/".to_owned());
            vec![(Some(value), String::new())]
        },
        Some(HttpRouteRulesMatchesPathType::RegularExpression) => {
            warn!("RegularExpression path match not supported, using catch-all prefix /");
            vec![(None, "/".to_owned())]
        },
        _ => vec![(None, "/".to_owned())],
    }
}

/// Extracts header match pairs from `HTTPRoute` header matches.
///
/// Only `Exact` match type is supported. `RegularExpression` headers are
/// skipped with a warning. Per Gateway API spec, first entry wins for
/// duplicate header names.
fn extract_headers(
    headers: &Option<Vec<gateway_api::httproutes::HttpRouteRulesMatchesHeaders>>,
) -> Option<BTreeMap<String, String>> {
    let hs = headers.as_ref().filter(|hs| !hs.is_empty())?;
    let map = collect_exact_headers(hs);
    if map.is_empty() { None } else { Some(map) }
}

/// Collects exact-match headers into a map, skipping regex matches.
fn collect_exact_headers(hs: &[gateway_api::httproutes::HttpRouteRulesMatchesHeaders]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for h in hs {
        if is_regex_header(h) {
            warn!(header = %h.name, "RegularExpression header match not supported, skipping");
            continue;
        }
        map.entry(h.name.clone()).or_insert_with(|| h.value.clone());
    }
    map
}

/// Returns `true` if the header match uses `RegularExpression` type.
fn is_regex_header(h: &gateway_api::httproutes::HttpRouteRulesMatchesHeaders) -> bool {
    h.r#type
        .as_ref()
        .is_some_and(|t| *t == gateway_api::httproutes::HttpRouteRulesMatchesHeadersType::RegularExpression)
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
    use gateway_api::httproutes::{
        HttpRouteRules, HttpRouteRulesBackendRefs, HttpRouteRulesMatches, HttpRouteRulesMatchesPath, HttpRouteSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_convert_routes_path_prefix() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/api".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "my-svc".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, backend_refs) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(
            praxis_routes.len(),
            2,
            "element-wise prefix should produce exact + prefix routes"
        );

        assert_eq!(
            praxis_routes[0].path,
            Some("/api".to_owned()),
            "first route should be exact match for bare path"
        );
        assert_eq!(praxis_routes[0].path_prefix, "", "exact route should have empty prefix");

        assert_eq!(praxis_routes[1].path, None, "second route should be prefix match");
        assert_eq!(
            praxis_routes[1].path_prefix, "/api/",
            "prefix route should have trailing slash"
        );

        for r in &praxis_routes {
            assert_eq!(
                r.cluster, "default~my-svc~8080",
                "cluster should match namespace-service-port"
            );
        }

        assert_eq!(backend_refs.len(), 1, "should have one backend ref");
        assert_eq!(
            backend_refs[0].cluster_name, "default~my-svc~8080",
            "backend ref cluster name should match"
        );
        assert_eq!(backend_refs[0].service, "my-svc", "backend ref service should match");
        assert_eq!(backend_refs[0].port, 8080, "backend ref port should match");
    }

    #[test]
    fn test_convert_routes_exact_path() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::Exact),
                            value: Some("/exact".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "my-svc".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, _) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(praxis_routes.len(), 1, "should produce one route");
        assert_eq!(
            praxis_routes[0].path,
            Some("/exact".to_owned()),
            "exact match should use path field"
        );
        assert_eq!(
            praxis_routes[0].path_prefix, "",
            "exact match should have empty path_prefix"
        );
    }

    #[test]
    fn test_convert_routes_multiple_rules() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![
                    HttpRouteRules {
                        matches: Some(vec![HttpRouteRulesMatches {
                            path: Some(HttpRouteRulesMatchesPath {
                                r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                                value: Some("/api".to_owned()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                            name: "api-svc".to_owned(),
                            port: Some(8080),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                    HttpRouteRules {
                        matches: Some(vec![HttpRouteRulesMatches {
                            path: Some(HttpRouteRulesMatchesPath {
                                r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                                value: Some("/web".to_owned()),
                            }),
                            ..Default::default()
                        }]),
                        backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                            name: "web-svc".to_owned(),
                            port: Some(80),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, backend_refs) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(
            praxis_routes.len(),
            4,
            "two prefix rules produce two element-wise pairs each"
        );

        assert!(
            backend_refs.iter().any(|b| b.cluster_name == "default~api-svc~8080"),
            "should have api-svc backend ref"
        );
        assert!(
            backend_refs.iter().any(|b| b.cluster_name == "default~web-svc~80"),
            "should have web-svc backend ref"
        );
    }

    #[test]
    fn test_convert_routes_with_hostname() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec!["example.com".to_owned()]),
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "my-svc".to_owned(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, _) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(praxis_routes.len(), 1, "should produce one route");
        assert_eq!(
            praxis_routes[0].host,
            Some("example.com".to_owned()),
            "host should match"
        );
    }

    #[test]
    fn test_convert_routes_cross_namespace() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("app-ns".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "svc".to_owned(),
                        namespace: Some("other-ns".to_owned()),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let grant = make_reference_grant("other-ns", "app-ns", "svc");
        let routes = vec![(&route, vec![None])];
        let (praxis_routes, backend_refs) = convert_routes(&routes, &HashMap::new(), &[grant]);

        assert_eq!(praxis_routes.len(), 1, "should produce one route");
        assert_eq!(
            praxis_routes[0].cluster, "other-ns~svc~80",
            "cluster should use backend namespace"
        );

        assert!(
            backend_refs.iter().any(|b| b.cluster_name == "other-ns~svc~80"),
            "backend refs should use backend namespace"
        );
        assert_eq!(
            backend_refs[0].namespace, "other-ns",
            "backend ref namespace should be other-ns"
        );
    }

    #[test]
    fn test_convert_routes_with_header_matches() {
        use gateway_api::httproutes::HttpRouteRulesMatchesHeaders;

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/api".to_owned()),
                        }),
                        headers: Some(vec![
                            HttpRouteRulesMatchesHeaders {
                                name: "x-version".to_owned(),
                                value: "v1".to_owned(),
                                ..Default::default()
                            },
                            HttpRouteRulesMatchesHeaders {
                                name: "x-region".to_owned(),
                                value: "us-east-1".to_owned(),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "api-svc".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, _) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(
            praxis_routes.len(),
            2,
            "element-wise prefix should produce exact + prefix routes"
        );

        for r in &praxis_routes {
            assert!(r.headers.is_some(), "both routes should have header matches");

            let headers = r.headers.as_ref().unwrap();
            assert_eq!(headers.len(), 2, "should have two header matches");
            assert_eq!(headers.get("x-version").unwrap(), "v1", "x-version header should match");
            assert_eq!(
                headers.get("x-region").unwrap(),
                "us-east-1",
                "x-region header should match"
            );
        }
    }

    #[test]
    fn test_convert_routes_missing_path_value() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: None,
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "svc".to_owned(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, _) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(praxis_routes.len(), 1, "should produce one route");
        assert_eq!(
            praxis_routes[0].path, None,
            "missing path value should have no exact path"
        );
        assert_eq!(
            praxis_routes[0].path_prefix, "/",
            "missing path value should default to /"
        );
    }

    #[test]
    fn test_convert_routes_regular_expression_type() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::RegularExpression),
                            value: Some("/api/.*".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "svc".to_owned(),
                        port: Some(80),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, _) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(praxis_routes.len(), 1, "should produce one route");
        assert_eq!(praxis_routes[0].path, None, "regex match should have no exact path");
        assert_eq!(
            praxis_routes[0].path_prefix, "/",
            "regex match should fall back to default /"
        );
    }

    #[test]
    fn test_convert_routes_hostname_intersection_filters_non_matching() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec!["non.matching.com".to_owned(), "very.specific.com".to_owned()]),
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/s1".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "backend".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut listener_map = HashMap::new();
        listener_map.insert("listener-1".to_owned(), Some("very.specific.com".to_owned()));

        let routes = vec![(&route, vec![Some("listener-1".to_owned())])];
        let (praxis_routes, _) = convert_routes(&routes, &listener_map, &[]);

        let hosts: Vec<_> = praxis_routes.iter().filter_map(|r| r.host.as_deref()).collect();
        assert!(
            hosts.iter().all(|h| *h == "very.specific.com"),
            "should only have matching hostname, got: {hosts:?}"
        );
        assert!(
            !hosts.contains(&"non.matching.com"),
            "non-matching hostname should be filtered out"
        );
    }

    #[test]
    fn test_convert_routes_hostname_intersection_wildcard_listener() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec![
                    "non.matching.com".to_owned(),
                    "foo.wildcard.io".to_owned(),
                    "bar.wildcard.io".to_owned(),
                ]),
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/s2".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "backend".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut listener_map = HashMap::new();
        listener_map.insert("listener-2".to_owned(), Some("*.wildcard.io".to_owned()));

        let routes = vec![(&route, vec![Some("listener-2".to_owned())])];
        let (praxis_routes, _) = convert_routes(&routes, &listener_map, &[]);

        let hosts: Vec<_> = praxis_routes.iter().filter_map(|r| r.host.as_deref()).collect();
        assert!(
            hosts.contains(&"foo.wildcard.io"),
            "foo.wildcard.io should match *.wildcard.io"
        );
        assert!(
            hosts.contains(&"bar.wildcard.io"),
            "bar.wildcard.io should match *.wildcard.io"
        );
        assert!(
            !hosts.contains(&"non.matching.com"),
            "non.matching.com should be filtered out"
        );
    }

    #[test]
    fn test_convert_routes_no_hostnames_unaffected() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "backend".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut listener_map = HashMap::new();
        listener_map.insert("listener-1".to_owned(), Some("specific.com".to_owned()));

        let routes = vec![(&route, vec![Some("listener-1".to_owned())])];
        let (praxis_routes, _) = convert_routes(&routes, &listener_map, &[]);

        assert_eq!(praxis_routes.len(), 1, "should produce one catch-all route");
        assert_eq!(
            praxis_routes[0].host, None,
            "route without hostnames should stay host-less"
        );
    }

    #[test]
    fn test_convert_routes_all_hostnames_filtered_produces_no_routes() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                hostnames: Some(vec!["wrong.com".to_owned(), "also.wrong.com".to_owned()]),
                rules: Some(vec![HttpRouteRules {
                    matches: Some(vec![HttpRouteRulesMatches {
                        path: Some(HttpRouteRulesMatchesPath {
                            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/s5".to_owned()),
                        }),
                        ..Default::default()
                    }]),
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "backend".to_owned(),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut listener_map = HashMap::new();
        listener_map.insert("listener-1".to_owned(), Some("very.specific.com".to_owned()));

        let routes = vec![(&route, vec![Some("listener-1".to_owned())])];
        let (praxis_routes, _) = convert_routes(&routes, &listener_map, &[]);

        assert!(
            praxis_routes.is_empty(),
            "routes with no intersecting hostnames should produce no PraxisRoutes"
        );
    }

    fn make_reference_grant(target_ns: &str, from_ns: &str, service_name: &str) -> ReferenceGrant {
        use gateway_api::referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo};

        ReferenceGrant {
            metadata: ObjectMeta {
                namespace: Some(target_ns.to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: from_ns.to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: Some(service_name.to_owned()),
                }],
            },
        }
    }

    #[test]
    fn test_convert_routes_cross_ns_backend_without_grant_excluded() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("xns-route".to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                rules: Some(vec![HttpRouteRules {
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: "web-backend".to_owned(),
                        namespace: Some("other-ns".to_owned()),
                        port: Some(8080),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let routes = vec![(&route, vec![None])];
        let (praxis_routes, backend_refs) = convert_routes(&routes, &HashMap::new(), &[]);

        assert_eq!(
            praxis_routes.len(),
            1,
            "route should still be created for unauthorized backend"
        );
        assert!(
            backend_refs.is_empty(),
            "unauthorized cross-ns backend should be excluded from cluster"
        );
    }
}
