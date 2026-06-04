// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `Service` builder for Praxis `LoadBalancer`.

use gateway_api::gateways::Gateway;
use k8s_openapi::{
    api::core::v1::{Service, ServicePort, ServiceSpec},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use kube::ResourceExt;

use super::labels::{owner_reference, standard_labels};

// -----------------------------------------------------------------------------
// Service Builder
// -----------------------------------------------------------------------------

/// Builds a `LoadBalancer` `Service` for Praxis.
///
/// Creates a `Service` with type `LoadBalancer`, standard labels, and a selector
/// matching the Praxis deployment pods. Owned by the Gateway resource.
/// # Errors
///
/// Returns an error if the Gateway has no UID.
pub(crate) fn build_service(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    ports: Vec<ServicePort>,
) -> crate::error::Result<Service> {
    let instance = gateway.name_any();

    Ok(Service {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            labels: Some(standard_labels(&instance)),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("LoadBalancer".to_owned()),
            selector: Some(standard_labels(&instance)),
            ports: Some(ports),
            ..Default::default()
        }),
        ..Default::default()
    })
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
    use k8s_openapi::apimachinery::pkg::{apis::meta::v1::ObjectMeta, util::intstr::IntOrString};

    use super::*;

    #[test]
    fn test_build_service_metadata() {
        let gateway = Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };

        let ports = vec![ServicePort {
            name: Some("http".to_owned()),
            port: 80,
            target_port: Some(IntOrString::Int(8080)),
            protocol: Some("TCP".to_owned()),
            ..Default::default()
        }];

        let service = build_service("praxis-svc", "default", &gateway, ports).unwrap();

        assert_eq!(
            service.metadata.name,
            Some("praxis-svc".to_owned()),
            "name should match"
        );
        assert_eq!(
            service.metadata.namespace,
            Some("default".to_owned()),
            "namespace should match"
        );

        let labels = service.metadata.labels.expect("labels should be set");
        assert_eq!(
            labels.get("app.kubernetes.io/name"),
            Some(&"praxis".to_owned()),
            "app name label should be set"
        );
        assert_eq!(
            labels.get("app.kubernetes.io/instance"),
            Some(&"test-gateway".to_owned()),
            "instance label should match gateway name"
        );

        let owner_refs = service
            .metadata
            .owner_references
            .expect("owner references should be set");
        assert_eq!(owner_refs.len(), 1, "should have one owner reference");
        assert_eq!(owner_refs[0].kind, "Gateway", "owner kind should be Gateway");
        assert_eq!(owner_refs[0].name, "test-gateway", "owner name should match");
    }

    #[test]
    fn test_build_service_spec() {
        let gateway = Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };

        let ports = vec![
            ServicePort {
                name: Some("http".to_owned()),
                port: 80,
                target_port: Some(IntOrString::Int(8080)),
                protocol: Some("TCP".to_owned()),
                ..Default::default()
            },
            ServicePort {
                name: Some("https".to_owned()),
                port: 443,
                target_port: Some(IntOrString::Int(8443)),
                protocol: Some("TCP".to_owned()),
                ..Default::default()
            },
        ];

        let service = build_service("praxis-svc", "default", &gateway, ports.clone()).unwrap();

        let spec = service.spec.expect("spec should be set");
        assert_eq!(
            spec.type_,
            Some("LoadBalancer".to_owned()),
            "type should be LoadBalancer"
        );

        let selector = spec.selector.expect("selector should be set");
        assert_eq!(
            selector.get("app.kubernetes.io/name"),
            Some(&"praxis".to_owned()),
            "selector should match labels"
        );
        assert_eq!(
            selector.get("app.kubernetes.io/instance"),
            Some(&"test-gateway".to_owned()),
            "selector instance should match"
        );

        let service_ports = spec.ports.expect("ports should be set");
        assert_eq!(service_ports.len(), 2, "should have two ports");
        assert_eq!(
            service_ports[0].name,
            Some("http".to_owned()),
            "first port name should match"
        );
        assert_eq!(service_ports[0].port, 80, "first port should be 80");
        assert_eq!(
            service_ports[1].name,
            Some("https".to_owned()),
            "second port name should match"
        );
        assert_eq!(service_ports[1].port, 443, "second port should be 443");
    }

    #[test]
    fn test_build_service_selector_matches_labels() {
        let gateway = Gateway {
            metadata: ObjectMeta {
                name: Some("my-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };

        let service = build_service("praxis-svc", "default", &gateway, vec![]).unwrap();

        let labels = service.metadata.labels.expect("labels should be set");
        let spec = service.spec.expect("spec should be set");
        let selector = spec.selector.expect("selector should be set");

        assert_eq!(labels, selector, "labels and selector should match");
    }
}
