// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Full Praxis YAML configuration assembly.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    cluster::PraxisCluster,
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

    /// Insecure options for container deployments.
    pub insecure_options: PraxisInsecureOptions,

    /// Listeners (proxy entry points).
    pub listeners: Vec<PraxisListener>,
}

/// Admin endpoint configuration.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisAdmin {
    /// Admin bind address.
    pub address: String,
}

/// Named filter chain.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisFilterChain {
    /// Filter chain name.
    pub name: String,

    /// Ordered filters in the chain.
    pub filters: Vec<PraxisFilterEntry>,
}

/// Insecure options (for container deployments).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisInsecureOptions {
    /// Allow admin endpoint on public interface.
    pub allow_public_admin: bool,
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
    extra_filters: &[PraxisFilterEntry],
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> yaml_serde::Result<PraxisConfig> {
    let filter_chains: Vec<_> = listeners
        .iter()
        .map(|l| build_filter_chain(l, routes, clusters, extra_filters, listener_hostnames))
        .collect::<yaml_serde::Result<Vec<_>>>()?;

    Ok(PraxisConfig {
        admin: PraxisAdmin {
            address: "0.0.0.0:9901".to_owned(),
        },
        filter_chains,
        insecure_options: PraxisInsecureOptions {
            allow_public_admin: true,
        },
        listeners,
    })
}

/// Builds a single filter chain for a listener.
///
/// The chain contains `request_id`, `router` (with matching routes),
/// any extra filters, and `load_balancer` (with embedded clusters).
///
/// # Errors
///
/// Returns an error if filter config serialization fails.
fn build_filter_chain(
    listener: &PraxisListener,
    routes: &[PraxisRoute],
    clusters: &[PraxisCluster],
    extra_filters: &[PraxisFilterEntry],
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> yaml_serde::Result<PraxisFilterChain> {
    let name = &listener.name;

    let section_names = &listener.merged_section_names;
    let filtered: Vec<_> = routes
        .iter()
        .filter(|r| {
            r.listener_names.is_empty()
                || r.listener_names
                    .iter()
                    .any(|ln| ln.is_none() || ln.as_deref().is_some_and(|n| section_names.contains(&n.to_owned())))
        })
        .cloned()
        .collect();
    let mut scoped = inject_listener_hostnames(&filtered, listener_hostnames);
    sort_routes_by_specificity_owned(&mut scoped);
    let scoped_refs: Vec<_> = scoped.iter().collect();

    let mut filters = vec![PraxisFilterEntry {
        filter: "request_id".to_owned(),
        config: yaml_serde::Value::Null,
    }];
    filters.extend_from_slice(extra_filters);
    filters.push(build_router_filter(&scoped_refs)?);
    filters.push(build_lb_filter(clusters)?);

    Ok(PraxisFilterChain {
        name: format!("{name}-chain"),
        filters,
    })
}

/// Injects listener hostnames into routes that lack one.
///
/// Each route's `listener_names` determines which Gateway listener it
/// targets. If that listener has a hostname constraint, routes without
/// an HTTPRoute-level hostname inherit it.
fn inject_listener_hostnames(
    routes: &[PraxisRoute],
    listener_hostnames: &std::collections::HashMap<String, Option<String>>,
) -> Vec<PraxisRoute> {
    routes
        .iter()
        .flat_map(|r| expand_route_hostnames(r, listener_hostnames))
        .collect()
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
        .map(|h| {
            let mut scoped = route.clone();
            scoped.host = Some(h);
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
/// Serializes the routes into a YAML mapping under the `routes` key.
///
/// # Errors
///
/// Returns an error if route serialization fails.
fn build_router_filter(routes: &[&PraxisRoute]) -> yaml_serde::Result<PraxisFilterEntry> {
    let config = yaml_serde::to_value(yaml_serde::Mapping::from_iter([(
        yaml_serde::Value::String("routes".to_owned()),
        yaml_serde::to_value(routes)?,
    )]))?;

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
            cluster: "default~my-svc~8080".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default~my-svc~8080", vec!["10.0.0.1:8080".to_owned()], None);

        let config = assemble_config(vec![listener], &[route], &[cluster], &[], &Default::default()).unwrap();

        assert_eq!(config.admin.address, "0.0.0.0:9901", "admin address should be set");
        assert_eq!(config.listeners.len(), 1, "should have one listener");
        assert!(
            config.insecure_options.allow_public_admin,
            "allow_public_admin should be true"
        );

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
            cluster: "default~svc~80".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default~svc~80", vec!["10.0.0.1:80".to_owned()], None);

        let config = assemble_config(vec![listener], &[route], &[cluster], &[], &Default::default()).unwrap();

        let yaml = yaml_serde::to_string(&config).expect("config should serialize to YAML");

        assert!(yaml.contains("admin:"), "YAML should contain admin section");
        assert!(yaml.contains("0.0.0.0:9901"), "YAML should contain admin address");
        assert!(yaml.contains("listeners:"), "YAML should contain listeners section");
        assert!(
            yaml.contains("filter_chains:"),
            "YAML should contain filter_chains section"
        );
        assert!(
            yaml.contains("insecure_options:"),
            "YAML should contain insecure_options section"
        );
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
            cluster: "default~svc~80".to_owned(),
            listener_names: vec![],
        };

        let cluster = build_cluster("default~svc~80", vec!["10.0.0.1:80".to_owned()], None);

        let config = assemble_config(
            vec![http_listener, https_listener],
            &[route],
            &[cluster],
            &[],
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
            &[],
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

        let config = assemble_config(vec![listener], &[], &[], &[], &Default::default()).unwrap();

        let chain = &config.filter_chains[0];
        let lb_config = &chain.filters[2].config;
        let clusters_value = lb_config.get("clusters").expect("lb config should have clusters key");
        let clusters_seq = clusters_value.as_sequence().expect("clusters should be a sequence");
        assert!(clusters_seq.is_empty(), "clusters should be empty");
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
            &[],
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
    fn test_extra_filters_placed_before_router() {
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

        let config = assemble_config(vec![listener], &[], &[], &[redirect], &Default::default()).unwrap();

        let chain = &config.filter_chains[0];
        let names: Vec<_> = chain.filters.iter().map(|f| f.filter.as_str()).collect();

        let redirect_pos = names.iter().position(|n| *n == "redirect");
        let router_pos = names.iter().position(|n| *n == "router");

        assert!(
            redirect_pos.is_some() && router_pos.is_some(),
            "both redirect and router should be in the chain: {names:?}"
        );
        assert!(
            redirect_pos.expect("redirect present") < router_pos.expect("router present"),
            "redirect must come before router so conditional redirects fire before routing: {names:?}"
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
        let config = assemble_config(vec![listener], &[route], &[cluster], &[], &hostnames).unwrap();

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
