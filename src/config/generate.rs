// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Shane Utt

//! Full Praxis YAML configuration assembly.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    cluster::PraxisCluster,
    filter_conversion::RouteFilters,
    listener::PraxisListener,
    routing::{PraxisFilterEntry, PraxisRoute},
};

// -----------------------------------------------------------------------------
// PraxisConfig
// -----------------------------------------------------------------------------

/// Top-level Praxis YAML configuration.
///
/// Clusters are embedded inside the `load_balancer` filter config in each
/// filter chain, matching the Praxis `deny_unknown_fields` schema.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisConfig {
    /// Admin endpoint configuration.
    pub admin: PraxisAdmin,

    /// Filter chains with routing and processing filters.
    pub filter_chains: Vec<PraxisFilterChain>,

    /// Listeners (proxy entry points).
    pub listeners: Vec<PraxisListener>,

    /// Runtime configuration.
    pub runtime: PraxisRuntime,
}

/// Admin endpoint configuration.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisAdmin {
    /// Health and metrics bind address.
    pub metrics_address: String,
}

/// Named filter chain.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisFilterChain {
    /// Filter chain name.
    pub name: String,

    /// Ordered filters in the chain.
    pub filters: Vec<PraxisFilterEntry>,
}

/// Runtime configuration.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisRuntime {
    /// Worker threads per service.
    pub threads: usize,
}

// -----------------------------------------------------------------------------
// Config Assembly
// -----------------------------------------------------------------------------

/// Assembles a complete Praxis configuration from components.
///
/// `listeners` are deduplicated (one per port). Each gets a single
/// filter chain with routes from all Gateway listeners that share
/// that port.
///
/// # Errors
///
/// Returns an error if filter config serialization fails.
pub fn assemble_config(
    listeners: Vec<PraxisListener>,
    routes: &[PraxisRoute],
    clusters: &[PraxisCluster],
    route_filters: &RouteFilters,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> yaml_serde::Result<PraxisConfig> {
    let by_listener: BTreeMap<String, RouteFilters> = listeners
        .iter()
        .map(|listener| (listener.name.clone(), route_filters.clone()))
        .collect();
    assemble_config_with_listener_filters(listeners, routes, clusters, &by_listener, listener_hostnames)
}

/// Assembles a Praxis configuration with filters scoped to each listener.
///
/// Route filters belong to the listener whose routes contributed them; a
/// listener with no routes must not inherit filters from another listener.
///
/// # Errors
///
/// Returns an error if filter config serialization fails.
pub fn assemble_config_with_listener_filters(
    listeners: Vec<PraxisListener>,
    routes: &[PraxisRoute],
    clusters: &[PraxisCluster],
    route_filters: &BTreeMap<String, RouteFilters>,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> yaml_serde::Result<PraxisConfig> {
    let empty = RouteFilters::default();
    let filter_chains: Vec<_> = listeners
        .iter()
        .map(|listener| {
            let filters = route_filters.get(&listener.name).unwrap_or(&empty);
            build_filter_chain(listener, routes, clusters, filters, listener_hostnames)
        })
        .collect::<yaml_serde::Result<Vec<_>>>()?;

    Ok(PraxisConfig {
        admin: PraxisAdmin {
            metrics_address: "0.0.0.0:9902".to_owned(),
        },
        filter_chains,
        listeners,
        runtime: PraxisRuntime { threads: 1 },
    })
}

/// Builds a single filter chain for a listener.
///
/// The chain runs `request_id`, the route filters that answer a request
/// without an upstream, `router` (with matching routes), the route
/// filters that reshape a routed request, and finally `load_balancer`
/// (with embedded clusters). Routeless listeners terminate with a
/// 404 response because Praxis rejects an empty router route list. [`RouteFilters`] explains why the router splits the
/// two groups.
///
/// # Errors
///
/// Returns an error if filter config serialization fails.
fn build_filter_chain(
    listener: &PraxisListener,
    routes: &[PraxisRoute],
    clusters: &[PraxisCluster],
    route_filters: &RouteFilters,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> yaml_serde::Result<PraxisFilterChain> {
    let name = &listener.name;
    let scoped = scoped_routes(listener, routes, listener_hostnames);
    let scoped_refs: Vec<_> = scoped.iter().collect();

    let mut filters = vec![PraxisFilterEntry {
        filter: "request_id".to_owned(),
        config: yaml_serde::Value::Null,
    }];
    if scoped_refs.is_empty() {
        // A listener with no routes still serves its terminating filters
        // (for example a redirect-only rule); the 404 answers everything else.
        filters.extend_from_slice(&route_filters.terminating);
        filters.push(PraxisFilterEntry {
            filter: "static_response".to_owned(),
            config: yaml_serde::to_value(serde_json::json!({"status": 404}))?,
        });
        return Ok(PraxisFilterChain {
            name: format!("{name}-chain"),
            filters,
        });
    }
    filters.extend_from_slice(&route_filters.terminating);
    filters.push(build_router_filter(&scoped_refs)?);
    filters.extend_from_slice(&route_filters.transforming);
    filters.push(build_lb_filter(clusters)?);

    Ok(PraxisFilterChain {
        name: format!("{name}-chain"),
        filters,
    })
}

/// Returns the routes scoped to one listener, ordered by specificity.
fn scoped_routes(
    listener: &PraxisListener,
    routes: &[PraxisRoute],
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> Vec<PraxisRoute> {
    let section_names = &listener.merged_section_names;
    let mut scoped: Vec<_> = routes
        .iter()
        .filter(|route| {
            route.listener_names.is_empty()
                || route.listener_names.iter().any(|name| {
                    name.as_deref()
                        .is_none_or(|section| section_names.iter().any(|candidate| candidate == section))
                })
        })
        .flat_map(|route| expand_route_hostnames(route, listener_hostnames))
        .collect();
    sort_routes_by_specificity_owned(&mut scoped);
    scoped
}

/// Expands a route into one copy per targeted listener hostname.
///
/// Routes with an existing hostname pass through unchanged. Routes
/// without a hostname are duplicated for each distinct listener
/// hostname they target (deduped). Routes targeting listeners
/// without hostname constraints pass through unchanged.
fn expand_route_hostnames(
    route: &PraxisRoute,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> Vec<PraxisRoute> {
    if route.host.is_some() {
        return vec![route.clone()];
    }

    let hostnames = collect_route_listener_hostnames(route, listener_hostnames);
    if hostnames.is_empty() {
        return vec![route.clone()];
    }

    hostnames
        .into_iter()
        .map(|host| {
            let mut scoped = route.clone();
            scoped.host = Some(host);
            scoped
        })
        .collect()
}

/// Collects distinct listener hostnames for a route's target listeners.
fn collect_route_listener_hostnames(
    route: &PraxisRoute,
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut hostnames = Vec::new();
    for ln in &route.listener_names {
        let Some(section) = ln.as_ref() else { continue };
        if let Some(Some(hostname)) = listener_hostnames.get(section)
            && seen.insert(hostname.clone())
        {
            hostnames.push(hostname.clone());
        }
    }
    hostnames
}

/// Sorts owned routes by specificity (same logic as reference version).
fn sort_routes_by_specificity_owned(routes: &mut [PraxisRoute]) {
    routes.sort_by_key(route_sort_key);
}

/// Returns a sort key `(priority, reverse_len, host_absent,
/// reverse_constraint_count)`.
///
/// Lower values sort first. Exact matches get priority 0, prefix
/// matches get priority 1. Within each tier, longer paths sort first
/// via [`std::cmp::Reverse`]. Host-constrained routes precede
/// unconstrained, and among the rest the route carrying the most
/// header, query and method constraints wins.
fn route_sort_key(route: &PraxisRoute) -> (u8, std::cmp::Reverse<usize>, bool, std::cmp::Reverse<usize>) {
    let (tier, length) = if route.path.is_some() {
        (0, route.path.as_ref().map_or(0, String::len))
    } else {
        (1, route.path_prefix.len())
    };

    (
        tier,
        std::cmp::Reverse(length),
        route.host.is_none(),
        std::cmp::Reverse(extra_constraints(route)),
    )
}

/// Counts the non-path constraints a route carries.
///
/// Two routes matching the same path are ordered by how specific they
/// are, so a header-constrained rule is evaluated before an otherwise
/// identical unconstrained one.
fn extra_constraints(route: &PraxisRoute) -> usize {
    route.headers.as_ref().map_or(0, BTreeMap::len)
}

/// Builds the `router` filter entry from matched routes.
///
/// Serializes the routes into a YAML mapping under the `routes` key,
/// and turns on multi-level subdomain matching.
///
/// Praxis defaults that off, so `*.example.com` matches
/// `foo.example.com` and nothing deeper. The Gateway API defines the
/// wildcard as a suffix match at any depth — `foo.bar.example.com` is
/// as much a match as `foo.example.com` — and a Gateway serving
/// `*.example.com` that 404s the deeper name is refusing traffic it
/// advertised.
///
/// # Errors
///
/// Returns an error if route serialization fails.
fn build_router_filter(routes: &[&PraxisRoute]) -> yaml_serde::Result<PraxisFilterEntry> {
    let config = yaml_serde::to_value(yaml_serde::Mapping::from_iter([
        (
            yaml_serde::Value::String("multi_level_subdomain_matching".to_owned()),
            yaml_serde::Value::Bool(true),
        ),
        (
            yaml_serde::Value::String("routes".to_owned()),
            yaml_serde::to_value(routes)?,
        ),
    ]))?;

    Ok(PraxisFilterEntry {
        filter: "router".to_owned(),
        config,
    })
}

/// Builds the `load_balancer` filter entry with embedded clusters.
///
/// Serializes clusters into a YAML mapping under the `clusters` key.
///
/// # Errors
///
/// Returns an error if cluster serialization fails.
fn build_lb_filter(clusters: &[PraxisCluster]) -> yaml_serde::Result<PraxisFilterEntry> {
    let config = yaml_serde::to_value(yaml_serde::Mapping::from_iter([(
        yaml_serde::Value::String("clusters".to_owned()),
        yaml_serde::to_value(clusters)?,
    )]))?;

    Ok(PraxisFilterEntry {
        filter: "load_balancer".to_owned(),
        config,
    })
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::too_many_lines, clippy::default_trait_access, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::cluster::{PraxisEndpoint, build_cluster};

    #[test]
    fn test_assemble_config_single_http_listener() {
        let listener = PraxisListener {
            name: "http".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["http-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let route = PraxisRoute {
            path: None,
            path_prefix: "/api/".to_owned(),
            host: None,
            headers: None,
            cluster: "default-my-svc-8080".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default-my-svc-8080", vec!["10.0.0.1:8080".to_owned()], None);

        let config = assemble_config(
            vec![listener],
            &[route],
            &[cluster],
            &RouteFilters::default(),
            &Default::default(),
        )
        .unwrap();

        assert_eq!(
            config.admin.metrics_address, "0.0.0.0:9902",
            "health and metrics should bind to the dedicated metrics address"
        );
        assert_eq!(config.listeners.len(), 1, "should have one listener");
        assert_eq!(config.runtime.threads, 1, "runtime should use one worker thread");

        assert_eq!(config.filter_chains.len(), 1, "should have one filter chain");
        let chain = &config.filter_chains[0];
        assert_eq!(chain.name, "http-chain", "filter chain name should match listener");
        assert_eq!(chain.filters.len(), 3, "should have three filters");
        assert_eq!(
            chain.filters[0].filter, "request_id",
            "first filter should be request_id"
        );
        assert_eq!(chain.filters[1].filter, "router", "second filter should be router");
        assert_eq!(
            chain.filters[2].filter, "load_balancer",
            "third filter should be load_balancer"
        );

        let lb_config = &chain.filters[2].config;
        assert!(lb_config.is_mapping(), "load_balancer config should be a mapping");
        let clusters_value = lb_config.get("clusters").expect("lb config should have clusters");
        let clusters_seq = clusters_value.as_sequence().expect("clusters should be a sequence");
        assert_eq!(clusters_seq.len(), 1, "should have one cluster in lb config");
    }

    #[test]
    fn test_assemble_config_yaml_serialization() {
        let listener = PraxisListener {
            name: "http".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["http-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: None,
            headers: None,
            cluster: "default-svc-80".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default-svc-80", vec!["10.0.0.1:80".to_owned()], None);

        let config = assemble_config(
            vec![listener],
            &[route],
            &[cluster],
            &RouteFilters::default(),
            &Default::default(),
        )
        .unwrap();

        let yaml = yaml_serde::to_string(&config).expect("config should serialize to YAML");

        assert!(yaml.contains("admin:"), "YAML should contain admin section");
        assert!(
            yaml.contains("metrics_address: 0.0.0.0:9902"),
            "YAML should contain metrics address"
        );
        assert!(yaml.contains("listeners:"), "YAML should contain listeners section");
        assert!(
            yaml.contains("filter_chains:"),
            "YAML should contain filter_chains section"
        );
        assert!(
            !yaml.contains("insecure_options"),
            "public metrics must not require an insecure option"
        );
        assert!(yaml.contains("threads: 1"), "YAML should contain runtime thread count");
        assert!(yaml.contains("request_id"), "YAML should contain request_id filter");
        assert!(yaml.contains("router"), "YAML should contain router filter");
        assert!(
            yaml.contains("load_balancer"),
            "YAML should contain load_balancer filter"
        );
        assert!(
            yaml.contains("clusters:"),
            "YAML should contain clusters inside load_balancer filter"
        );
    }

    #[test]
    fn test_assemble_config_multiple_listeners() {
        let http_listener = PraxisListener {
            name: "http".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["http-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let https_listener = PraxisListener {
            name: "https".to_owned(),
            address: "0.0.0.0:443".to_owned(),
            protocol: Some("http".to_owned()),
            filter_chains: vec!["https-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: None,
            headers: None,
            cluster: "default-svc-80".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default-svc-80", vec!["10.0.0.1:80".to_owned()], None);

        let config = assemble_config(
            vec![http_listener, https_listener],
            &[route],
            &[cluster],
            &RouteFilters::default(),
            &Default::default(),
        )
        .unwrap();

        assert_eq!(config.listeners.len(), 2, "should have two listeners");
        assert_eq!(config.filter_chains.len(), 2, "should have two filter chains");
        assert_eq!(
            config.filter_chains[0].name, "http-chain",
            "first chain name should match"
        );
        assert_eq!(
            config.filter_chains[1].name, "https-chain",
            "second chain name should match"
        );
    }

    #[test]
    fn test_assemble_config_filter_chain_structure() {
        let listener = PraxisListener {
            name: "test".to_owned(),
            address: "0.0.0.0:8080".to_owned(),
            protocol: None,
            filter_chains: vec!["test-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let route = PraxisRoute {
            path: None,
            path_prefix: "/api/".to_owned(),
            host: None,
            headers: None,
            cluster: "test-cluster".to_owned(),
            listener_names: vec![],
        };

        let cluster = PraxisCluster {
            name: "test-cluster".to_owned(),
            endpoints: vec![PraxisEndpoint::Simple("127.0.0.1:8080".to_owned())],
            load_balancer_strategy: None,
        };

        let config = assemble_config(
            vec![listener],
            std::slice::from_ref(&route),
            &[cluster],
            &RouteFilters::default(),
            &Default::default(),
        )
        .unwrap();

        let chain = &config.filter_chains[0];
        assert_eq!(
            chain.filters[0].filter, "request_id",
            "first filter should be request_id"
        );

        assert_eq!(chain.filters[1].filter, "router", "second filter should be router");
        let router_config = &chain.filters[1].config;
        assert!(router_config.is_mapping(), "router config should be a mapping");

        let routes_value = router_config.get("routes").expect("router config should have routes");
        let routes_array = routes_value.as_sequence().expect("routes should be a sequence");
        assert_eq!(routes_array.len(), 1, "should have one route in router config");

        assert_eq!(
            chain.filters[2].filter, "load_balancer",
            "third filter should be load_balancer"
        );
        let lb_config = &chain.filters[2].config;
        assert!(lb_config.is_mapping(), "load_balancer config should be a mapping");
        let clusters_value = lb_config.get("clusters").expect("lb config should have clusters");
        let clusters_seq = clusters_value.as_sequence().expect("clusters should be a sequence");
        assert_eq!(clusters_seq.len(), 1, "should have one cluster in lb config");
    }

    #[test]
    fn test_assemble_config_empty_clusters() {
        let listener = PraxisListener {
            name: "http".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["http-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let config = assemble_config(vec![listener], &[], &[], &RouteFilters::default(), &Default::default()).unwrap();

        let chain = &config.filter_chains[0];
        assert_eq!(
            chain.filters.len(),
            2,
            "a routeless listener should omit the router and load balancer"
        );
        assert_eq!(
            chain.filters[0].filter, "request_id",
            "the routeless chain should retain request IDs"
        );
        assert_eq!(
            chain.filters[1].filter, "static_response",
            "unmatched traffic must terminate"
        );
        assert_eq!(
            chain.filters[1].config["status"].as_u64(),
            Some(404),
            "routeless listeners return 404"
        );
    }

    #[test]
    fn test_merged_listener_includes_routes_from_all_sections() {
        let listener = PraxisListener {
            name: "listener-1".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["listener-1-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![
                "listener-1".to_owned(),
                "listener-2".to_owned(),
                "listener-3".to_owned(),
            ],
            tls: None,
        };

        let route_l1 = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: Some("bar.com".to_owned()),
            headers: None,
            cluster: "v1".to_owned(),
            listener_names: vec![Some("listener-1".to_owned())],
        };

        let route_l2 = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: Some("foo.bar.com".to_owned()),
            headers: None,
            cluster: "v2".to_owned(),
            listener_names: vec![Some("listener-2".to_owned())],
        };

        let route_l3 = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: Some("*.bar.com".to_owned()),
            headers: None,
            cluster: "v3".to_owned(),
            listener_names: vec![Some("listener-3".to_owned())],
        };

        let cluster_v1 = build_cluster("v1", vec!["10.0.0.1:80".to_owned()], None);
        let cluster_v2 = build_cluster("v2", vec!["10.0.0.2:80".to_owned()], None);
        let cluster_v3 = build_cluster("v3", vec!["10.0.0.3:80".to_owned()], None);

        let config = assemble_config(
            vec![listener],
            &[route_l1, route_l2, route_l3],
            &[cluster_v1, cluster_v2, cluster_v3],
            &RouteFilters::default(),
            &Default::default(),
        )
        .unwrap();

        let chain = &config.filter_chains[0];
        let router_config = &chain.filters[1].config;
        let routes_val = router_config.get("routes").expect("router should have routes");
        let routes_seq = routes_val.as_sequence().expect("routes should be a sequence");

        assert_eq!(
            routes_seq.len(),
            3,
            "merged listener should include routes from all sections (listener-1, listener-2, listener-3)"
        );
    }

    #[test]
    fn test_router_matches_wildcards_at_any_depth() {
        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: Some("*.example.com".to_owned()),
            headers: None,
            cluster: "cluster".to_owned(),
            listener_names: vec![],
        };
        let router = build_router_filter(&[&route]).unwrap();

        assert_eq!(
            router.config.get("multi_level_subdomain_matching"),
            Some(&yaml_serde::Value::Bool(true)),
            "Praxis defaults this off, which would 404 foo.bar.example.com on a *.example.com \
             listener — traffic the Gateway API says the wildcard covers"
        );
    }

    #[test]
    fn test_terminating_filters_run_before_the_router() {
        let listener = PraxisListener {
            name: "http".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["http-chain".to_owned()],
            hostname: None,
            merged_section_names: vec![],
            tls: None,
        };

        let redirect = PraxisFilterEntry {
            filter: "redirect".to_owned(),
            config: yaml_serde::Value::Null,
        };

        let headers = PraxisFilterEntry {
            filter: "headers".to_owned(),
            config: yaml_serde::Value::Null,
        };

        let route_filters = RouteFilters {
            terminating: vec![redirect],
            transforming: vec![headers],
        };
        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: None,
            headers: None,
            cluster: "cluster".to_owned(),
            listener_names: vec![],
        };
        let cluster = build_cluster("cluster", vec!["10.0.0.1:80".to_owned()], None);

        let config = assemble_config(
            vec![listener],
            std::slice::from_ref(&route),
            std::slice::from_ref(&cluster),
            &route_filters,
            &Default::default(),
        )
        .unwrap();

        let chain = &config.filter_chains[0];
        let names: Vec<_> = chain.filters.iter().map(|f| f.filter.as_str()).collect();

        assert_eq!(
            names,
            vec!["request_id", "redirect", "router", "headers", "load_balancer"],
            "a redirect answers the request its rule has no backend for, so it has to precede the \
             router's 404; a header modifier changes what the router reads and so has to follow it"
        );
    }

    #[test]
    fn routeless_listener_keeps_its_own_terminating_filters() {
        let listener = PraxisListener {
            name: "routeless".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            filter_chains: vec!["routeless-chain".to_owned()],
            protocol: None,
            hostname: None,
            merged_section_names: vec!["routeless".to_owned()],
            tls: None,
        };
        let mut by_listener = BTreeMap::new();
        by_listener.insert(
            "routeless".to_owned(),
            RouteFilters {
                terminating: vec![PraxisFilterEntry {
                    filter: "redirect".to_owned(),
                    config: yaml_serde::Value::Null,
                }],
                transforming: Vec::new(),
            },
        );
        let config = assemble_config_with_listener_filters(
            vec![listener],
            &[],
            &[],
            &by_listener,
            &Default::default(),
        )
        .unwrap();
        let names: Vec<_> = config.filter_chains[0]
            .filters
            .iter()
            .map(|filter| filter.filter.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["request_id", "redirect", "static_response"],
            "a routeless listener must keep its own terminating filters before the 404"
        );
    }

    #[test]
    fn routeless_listeners_do_not_inherit_terminating_filters() {
        let routed = PraxisListener {
            name: "routed".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["routed-chain".to_owned()],
            hostname: None,
            merged_section_names: vec!["routed".to_owned()],
            tls: None,
        };
        let routeless = PraxisListener {
            name: "routeless".to_owned(),
            address: "0.0.0.0:8081".to_owned(),
            protocol: None,
            filter_chains: vec!["routeless-chain".to_owned()],
            hostname: None,
            merged_section_names: vec!["routeless".to_owned()],
            tls: None,
        };
        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: None,
            headers: None,
            cluster: "cluster".to_owned(),
            listener_names: vec![Some("routed".to_owned())],
        };
        let cluster = build_cluster("cluster", vec!["10.0.0.1:80".to_owned()], None);
        let route_filters = RouteFilters {
            terminating: vec![PraxisFilterEntry {
                filter: "redirect".to_owned(),
                config: yaml_serde::Value::Null,
            }],
            transforming: Vec::new(),
        };

        let mut by_listener = BTreeMap::new();
        by_listener.insert("routed".to_owned(), route_filters);
        let config = assemble_config_with_listener_filters(
            vec![routed, routeless],
            std::slice::from_ref(&route),
            std::slice::from_ref(&cluster),
            &by_listener,
            &Default::default(),
        )
        .unwrap();
        let routeless = config
            .filter_chains
            .iter()
            .find(|chain| chain.name == "routeless-chain")
            .expect("routeless chain");
        let names: Vec<_> = routeless.filters.iter().map(|filter| filter.filter.as_str()).collect();

        assert_eq!(
            names,
            vec!["request_id", "static_response"],
            "terminating filters from another listener's route must not answer routeless traffic"
        );
    }

    #[test]
    fn test_route_expanded_for_multiple_listener_hostnames() {
        let mut hostnames = std::collections::HashMap::new();
        hostnames.insert("l3".to_owned(), Some("*.bar.com".to_owned()));
        hostnames.insert("l4".to_owned(), Some("*.foo.com".to_owned()));

        let listener = PraxisListener {
            name: "l1".to_owned(),
            address: "0.0.0.0:80".to_owned(),
            protocol: None,
            filter_chains: vec!["l1-chain".to_owned()],
            hostname: None,
            merged_section_names: vec!["l3".to_owned(), "l4".to_owned()],
            tls: None,
        };

        let route = PraxisRoute {
            path: None,
            path_prefix: "/".to_owned(),
            host: None,
            headers: None,
            cluster: "v3".to_owned(),
            listener_names: vec![Some("l3".to_owned()), Some("l4".to_owned())],
        };

        let cluster = build_cluster("v3", vec!["10.0.0.3:80".to_owned()], None);
        let config = assemble_config(
            vec![listener],
            &[route],
            &[cluster],
            &RouteFilters::default(),
            &hostnames,
        )
        .unwrap();

        let chain = &config.filter_chains[0];
        let router_config = &chain.filters[1].config;
        let routes_val = router_config.get("routes").expect("should have routes");
        let routes_seq = routes_val.as_sequence().expect("routes should be a sequence");

        assert_eq!(
            routes_seq.len(),
            2,
            "route targeting 2 listeners with different hostnames should expand to 2 routes"
        );

        let hosts: Vec<_> = routes_seq
            .iter()
            .filter_map(|r| r.get("host"))
            .filter_map(|h| h.as_str())
            .collect();
        assert!(hosts.contains(&"*.bar.com"), "should have *.bar.com route");
        assert!(hosts.contains(&"*.foo.com"), "should have *.foo.com route");
    }
}
