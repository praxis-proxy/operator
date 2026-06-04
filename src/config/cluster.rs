// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Cluster configuration generation for Praxis proxy.

use serde::Serialize;

// -----------------------------------------------------------------------------
// PraxisCluster
// -----------------------------------------------------------------------------

/// Praxis cluster configuration.
///
/// Represents a backend cluster with endpoints and load balancing strategy.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct PraxisCluster {
    /// Cluster name.
    pub(crate) name: String,

    /// Cluster endpoints.
    pub(crate) endpoints: Vec<PraxisEndpoint>,

    /// Load balancing strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) load_balancer_strategy: Option<String>,
}

/// Praxis endpoint configuration.
///
/// Can be a simple address string or a weighted address.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum PraxisEndpoint {
    /// Simple endpoint address.
    Simple(String),
    /// Weighted endpoint with address and weight.
    Weighted {
        /// Endpoint address.
        address: String,
        /// Endpoint weight.
        weight: i32,
    },
}

// -----------------------------------------------------------------------------
// Cluster Building
// -----------------------------------------------------------------------------

/// Generates a cluster name from namespace, service, and port.
///
/// Returns a cluster name in the format `{namespace}~{service}~{port}`.
///
/// Uses `~` as separator because it cannot appear in Kubernetes namespace
/// or service names (DNS subdomain charset), preventing ambiguity.
#[allow(dead_code, reason = "utility function used in tests and future integration")]
pub(crate) fn cluster_name(namespace: &str, service: &str, port: i32) -> String {
    format!("{namespace}~{service}~{port}")
}

/// Builds a Praxis cluster configuration.
///
/// If weights are provided, creates weighted endpoints. Otherwise, uses simple
/// endpoint addresses.
pub(crate) fn build_cluster(name: &str, endpoints: Vec<String>, weights: Option<Vec<i32>>) -> PraxisCluster {
    let endpoints = if let Some(ws) = weights {
        endpoints
            .into_iter()
            .zip(ws)
            .map(|(address, weight)| PraxisEndpoint::Weighted { address, weight })
            .collect()
    } else {
        endpoints.into_iter().map(PraxisEndpoint::Simple).collect()
    };

    PraxisCluster {
        name: name.to_owned(),
        endpoints,
        load_balancer_strategy: None,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
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

    #[test]
    fn test_cluster_name() {
        let name = cluster_name("default", "my-svc", 8080);
        assert_eq!(name, "default~my-svc~8080", "cluster name format should match");
    }

    #[test]
    fn test_cluster_name_different_namespace() {
        let name = cluster_name("kube-system", "dns", 53);
        assert_eq!(name, "kube-system~dns~53", "cluster name should include namespace");
    }

    #[test]
    fn test_build_cluster_simple_endpoints() {
        let endpoints = vec!["10.0.0.1:8080".to_owned(), "10.0.0.2:8080".to_owned()];
        let cluster = build_cluster("test-cluster", endpoints, None);

        assert_eq!(cluster.name, "test-cluster", "cluster name should match");
        assert_eq!(cluster.endpoints.len(), 2, "should have two endpoints");

        match &cluster.endpoints[0] {
            PraxisEndpoint::Simple(addr) => {
                assert_eq!(addr, "10.0.0.1:8080", "first endpoint should match");
            },
            _ => panic!("expected Simple endpoint"),
        }

        match &cluster.endpoints[1] {
            PraxisEndpoint::Simple(addr) => {
                assert_eq!(addr, "10.0.0.2:8080", "second endpoint should match");
            },
            _ => panic!("expected Simple endpoint"),
        }

        assert_eq!(
            cluster.load_balancer_strategy, None,
            "load balancer strategy should be None"
        );
    }

    #[test]
    fn test_build_cluster_weighted_endpoints() {
        let endpoints = vec!["10.0.0.1:8080".to_owned(), "10.0.0.2:8080".to_owned()];
        let weights = vec![100, 200];
        let cluster = build_cluster("test-cluster", endpoints, Some(weights));

        assert_eq!(cluster.name, "test-cluster", "cluster name should match");
        assert_eq!(cluster.endpoints.len(), 2, "should have two endpoints");

        match &cluster.endpoints[0] {
            PraxisEndpoint::Weighted { address, weight } => {
                assert_eq!(address, "10.0.0.1:8080", "first endpoint address should match");
                assert_eq!(*weight, 100, "first endpoint weight should match");
            },
            _ => panic!("expected Weighted endpoint"),
        }

        match &cluster.endpoints[1] {
            PraxisEndpoint::Weighted { address, weight } => {
                assert_eq!(address, "10.0.0.2:8080", "second endpoint address should match");
                assert_eq!(*weight, 200, "second endpoint weight should match");
            },
            _ => panic!("expected Weighted endpoint"),
        }
    }

    #[test]
    fn test_build_cluster_empty_endpoints() {
        let cluster = build_cluster("empty-cluster", vec![], None);

        assert_eq!(cluster.name, "empty-cluster", "cluster name should match");
        assert!(cluster.endpoints.is_empty(), "endpoints should be empty");
    }

    #[test]
    fn test_build_cluster_single_endpoint() {
        let endpoints = vec!["10.0.0.1:8080".to_owned()];
        let cluster = build_cluster("single-cluster", endpoints, None);

        assert_eq!(cluster.endpoints.len(), 1, "should have one endpoint");
    }
}
