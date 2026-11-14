// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Shane Utt

//! Optional Prometheus monitoring resources for managed data planes.

use std::collections::BTreeMap;

use gateway_api::gateways::Gateway;
use k8s_openapi::{
    api::networking::v1::{NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort},
    apimachinery::pkg::{
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use kube::{
    ResourceExt as _,
    api::{ApiResource, DynamicObject},
    core::GroupVersionKind,
};
use serde_json::json;

use super::labels::{owner_reference, standard_labels};
use crate::context::{ADMIN_PORT, ADMIN_PORT_NAME};

/// Environment variable enabling monitoring resources.
const MONITORING_ENABLED_ENV: &str = "PRAXIS_MONITORING_ENABLED";

/// Environment variable selecting the monitoring namespace.
const MONITORING_NAMESPACE_ENV: &str = "PRAXIS_MONITORING_NAMESPACE";

/// Environment variable selecting Prometheus pods.
const MONITORING_SELECTOR_ENV: &str = "PRAXIS_MONITORING_POD_SELECTOR";

/// Optional monitoring resource configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitoringConfig {
    /// Namespace containing the Prometheus pods.
    pub namespace: String,
    /// Labels selecting the Prometheus pods.
    pub pod_selector: BTreeMap<String, String>,
}

/// Reads optional monitoring configuration from the operator environment.
///
/// # Errors
///
/// Rejects malformed selectors instead of broadening metrics access.
pub fn monitoring_config() -> crate::error::Result<Option<MonitoringConfig>> {
    let enabled = std::env::var(MONITORING_ENABLED_ENV)
        .ok()
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    monitoring_config_from_values(
        enabled,
        std::env::var(MONITORING_NAMESPACE_ENV).ok().as_deref(),
        std::env::var(MONITORING_SELECTOR_ENV).ok().as_deref(),
    )
}

/// Builds a namespaced `PodMonitor` as a dynamic resource so the operator can
/// run without the Prometheus Operator CRD installed when monitoring is off.
///
/// # Errors
///
/// Returns an error when the Gateway has no UID for its owner reference.
pub fn build_pod_monitor(name: &str, namespace: &str, gateway: &Gateway) -> crate::error::Result<DynamicObject> {
    let resource = pod_monitor_api_resource();
    let mut monitor = DynamicObject::new(name, &resource).within(namespace);
    monitor.metadata.labels = Some(standard_labels(&gateway.name_any()));
    monitor.metadata.owner_references = Some(vec![owner_reference(gateway)?]);
    Ok(monitor.data(json!({
        "spec": {
            "selector": {"matchLabels": standard_labels(&gateway.name_any())},
            "namespaceSelector": {"matchNames": [namespace]},
            "podMetricsEndpoints": [{
                "port": ADMIN_PORT_NAME,
                "path": "/metrics",
                "interval": "30s"
            }]
        }
    })))
}

/// Returns the API resource descriptor for the Prometheus Operator CRD.
#[must_use]
pub fn pod_monitor_api_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk("monitoring.coreos.com", "v1", "PodMonitor"))
}

/// Builds the optional metrics ingress policy for one managed data plane.
///
/// # Errors
///
/// Returns an error when the Gateway has no UID for its owner reference.
pub fn build_metrics_network_policy(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    config: &MonitoringConfig,
    listener_ports: &[(String, i32)],
) -> crate::error::Result<NetworkPolicy> {
    let labels = standard_labels(&gateway.name_any());
    let source = NetworkPolicyPeer {
        namespace_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "kubernetes.io/metadata.name".to_owned(),
                config.namespace.clone(),
            )])),
            ..Default::default()
        }),
        pod_selector: Some(LabelSelector {
            match_labels: Some(config.pod_selector.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    Ok(NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            ..Default::default()
        },
        spec: Some(build_network_policy_spec(labels, source, listener_ports)),
    })
}

/// Builds the policy specification separately to keep the resource builder
/// within the project's function-size limit.
fn build_network_policy_spec(
    labels: BTreeMap<String, String>,
    source: NetworkPolicyPeer,
    listener_ports: &[(String, i32)],
) -> k8s_openapi::api::networking::v1::NetworkPolicySpec {
    let mut rules = vec![NetworkPolicyIngressRule {
        from: Some(vec![source]),
        ports: Some(vec![policy_port(ADMIN_PORT)]),
    }];
    let data_ports: Vec<_> = listener_ports
        .iter()
        .filter(|(_, port)| *port != ADMIN_PORT)
        .map(|(_, port)| policy_port(*port))
        .collect();
    if !data_ports.is_empty() {
        rules.insert(
            0,
            NetworkPolicyIngressRule {
                from: None,
                ports: Some(data_ports),
            },
        );
    }
    k8s_openapi::api::networking::v1::NetworkPolicySpec {
        pod_selector: Some(LabelSelector {
            match_labels: Some(labels),
            ..Default::default()
        }),
        policy_types: Some(vec!["Ingress".to_owned()]),
        ingress: Some(rules),
        ..Default::default()
    }
}

/// Builds a TCP policy port without a source restriction.
fn policy_port(port: i32) -> NetworkPolicyPort {
    NetworkPolicyPort {
        port: Some(IntOrString::Int(port)),
        protocol: Some("TCP".to_owned()),
        ..Default::default()
    }
}

/// Resolves monitoring settings from explicit values for deterministic tests.
fn monitoring_config_from_values(
    enabled: bool,
    namespace: Option<&str>,
    selector: Option<&str>,
) -> crate::error::Result<Option<MonitoringConfig>> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(MonitoringConfig {
        namespace: namespace.unwrap_or("monitoring").to_owned(),
        pod_selector: selector.map_or_else(|| Ok(default_selector()), parse_selector)?,
    }))
}

/// Returns the default Prometheus pod selector.
fn default_selector() -> BTreeMap<String, String> {
    BTreeMap::from([("app.kubernetes.io/name".to_owned(), "prometheus".to_owned())])
}

/// Parses every equality requirement, rejecting incomplete or duplicate entries.
fn parse_selector(selector: &str) -> crate::error::Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for pair in selector.split(',') {
        let (key, value) = pair
            .split_once('=')
            .ok_or(crate::error::OperatorError::InvalidMonitoringSelector)?;
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty()
            || value.is_empty()
            || value.contains('=')
            || parsed.insert(key.to_owned(), value.to_owned()).is_some()
        {
            return Err(crate::error::OperatorError::InvalidMonitoringSelector);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use gateway_api::gateways::GatewaySpec;

    use super::*;

    /// Builds the Gateway used by the monitoring tests.
    fn gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec::default(),
            status: None,
        }
    }

    /// The `PodMonitor` scrapes the reserved metrics port and is Gateway-owned.
    #[test]
    fn pod_monitor_uses_metrics_port_and_owner() {
        let monitor = build_pod_monitor("praxis-test", "default", &gateway()).unwrap();
        assert_eq!(
            monitor.types.as_ref().map(|types| types.kind.as_str()),
            Some("PodMonitor"),
            "use the monitoring CRD"
        );
        assert_eq!(
            monitor.metadata.namespace.as_deref(),
            Some("default"),
            "monitor lives with the gateway"
        );
        assert_eq!(
            monitor.metadata.owner_references.as_ref().unwrap()[0].uid,
            "test-uid",
            "gateway owns monitor"
        );
        assert_eq!(
            monitor.data["spec"]["podMetricsEndpoints"][0]["port"], ADMIN_PORT_NAME,
            "scrape the named admin port"
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "explicit monitoring fixtures cover policy and reconciliation contracts"
    )]
    /// Data ports stay open to every peer; metrics stay closed to all but Prometheus.
    fn policy_preserves_data_ingress_and_restricts_metrics_to_selected_peer() {
        let config = monitoring_config_from_values(true, Some("observability"), Some("team=platform,instance=main"))
            .unwrap()
            .unwrap();
        let ports = [
            ("http".to_owned(), 8080),
            ("https".to_owned(), 8443),
            ("reserved".to_owned(), ADMIN_PORT),
        ];
        let policy = build_metrics_network_policy("praxis-test", "default", &gateway(), &config, &ports).unwrap();
        let spec = policy.spec.unwrap();
        assert_eq!(
            spec.policy_types,
            Some(vec!["Ingress".to_owned()]),
            "egress remains unchanged"
        );
        let rules = spec.ingress.unwrap();
        assert_eq!(rules.len(), 2, "data and metrics have separate rules");
        assert!(rules[0].from.is_none(), "clients from any source can reach data ports");
        assert_eq!(
            rules[0].ports,
            Some(vec![policy_port(8080), policy_port(8443)]),
            "metrics must never appear in unrestricted data ports"
        );
        assert_eq!(
            rules[1].ports,
            Some(vec![policy_port(ADMIN_PORT)]),
            "only metrics uses restricted peer"
        );
        let peers = rules[1].from.as_ref().unwrap();
        assert_eq!(peers.len(), 1, "namespace and pod selectors must be ANDed");
        assert_eq!(
            peers[0]
                .namespace_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()["kubernetes.io/metadata.name"],
            "observability",
            "restrict namespace"
        );
        assert_eq!(
            peers[0].pod_selector.as_ref().unwrap().match_labels.as_ref(),
            Some(&config.pod_selector),
            "preserve all selector requirements"
        );
    }

    /// Monitoring is opt-in and the selector comes from the environment contract.
    #[test]
    fn monitoring_is_opt_in_and_selector_is_configurable() {
        assert_eq!(
            monitoring_config_from_values(false, None, Some("invalid")).unwrap(),
            None,
            "disabled monitoring needs no selector"
        );
        let enabled = monitoring_config_from_values(true, Some("observability"), Some("team=platform"))
            .unwrap()
            .unwrap();
        assert_eq!(enabled.namespace, "observability", "use configured namespace");
        assert_eq!(
            enabled.pod_selector.get("team"),
            Some(&"platform".to_owned()),
            "use configured selector"
        );
        assert_eq!(
            monitoring_config_from_values(true, None, None)
                .unwrap()
                .unwrap()
                .pod_selector,
            default_selector(),
            "defaults apply only when unset"
        );
    }

    /// A data plane with no listeners cannot open metrics to every peer.
    #[test]
    fn empty_data_ports_cannot_allow_unrestricted_metrics() {
        let config = monitoring_config_from_values(true, None, None).unwrap().unwrap();
        for ports in [vec![], vec![("reserved".to_owned(), ADMIN_PORT)]] {
            let policy = build_metrics_network_policy("praxis-test", "default", &gateway(), &config, &ports).unwrap();
            let rules = policy.spec.unwrap().ingress.unwrap();
            assert_eq!(rules.len(), 1, "empty ports must not produce an allow-all rule");
            assert!(rules[0].from.is_some(), "metrics always requires the Prometheus peer");
        }
    }

    /// Malformed selectors fail closed instead of broadening access.
    #[test]
    fn malformed_selectors_never_produce_partial_or_default_access() {
        for selector in [
            "team=monitoring,instance",
            "",
            "team=",
            "=value",
            "team=a,",
            "team=a,team=b",
            "team=a=b",
        ] {
            assert!(
                matches!(
                    parse_selector(selector),
                    Err(crate::error::OperatorError::InvalidMonitoringSelector)
                ),
                "reject the entire selector: {selector}"
            );
        }
    }
}
