// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `Service` builder for Praxis `LoadBalancer`.

use gateway_api::gateways::Gateway;
use k8s_openapi::{
    api::core::v1::{Service, ServicePort, ServiceSpec},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use kube::ResourceExt as _;

use super::labels::{descriptive_labels, infrastructure_annotations, owner_reference, standard_labels};

// -----------------------------------------------------------------------------
// Service Builder
// -----------------------------------------------------------------------------

/// Returns the labels stamped on the Service itself.
fn service_labels(gateway: &Gateway, instance: &str) -> std::collections::BTreeMap<String, String> {
    let mut labels = standard_labels(instance);
    labels.extend(descriptive_labels(gateway));
    labels
}

/// Builds a `LoadBalancer` `Service` for Praxis.
///
/// Creates a `Service` with type `LoadBalancer`, standard labels, and a selector
/// matching the Praxis deployment pods. Owned by the Gateway resource.
/// # Errors
///
/// Returns an error if the Gateway has no UID.
pub fn build_service(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    ports: Vec<ServicePort>,
) -> crate::error::Result<Service> {
    let instance = gateway.name_any();

    Ok(Service {
        metadata: ObjectMeta {
            annotations: Some(infrastructure_annotations(gateway)),
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            labels: Some(service_labels(gateway, &instance)),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("LoadBalancer".to_owned()),
            // Deliberately the bare standard set: a Service selector is
            // as immutable as a Deployment's, so nothing that can change
            // with the Gateway spec may appear in it.
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
#[expect(clippy::too_many_lines, clippy::default_trait_access, reason = "tests")]
mod tests {
    use k8s_openapi::apimachinery::pkg::{apis::meta::v1::ObjectMeta, util::intstr::IntOrString};

    use super::{super::labels::GATEWAY_NAME_LABEL, *};

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
    fn test_build_service_selector_is_a_subset_of_its_labels() {
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

        assert!(
            selector.iter().all(|(key, value)| labels.get(key) == Some(value)),
            "the selector has to keep selecting the pods: {selector:?} vs {labels:?}"
        );
        assert!(
            !selector.contains_key(GATEWAY_NAME_LABEL) && labels.contains_key(GATEWAY_NAME_LABEL),
            "a Service selector is immutable once created, so only the fixed labels may appear in \
             it — the descriptive ones belong on the object alone"
        );
    }

    #[test]
    fn test_build_service_carries_gateway_infrastructure_metadata() {
        use gateway_api::gateways::{GatewayInfrastructure, GatewaySpec};


        let gateway = Gateway {
            metadata: ObjectMeta {
                name: Some("my-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                infrastructure: Some(GatewayInfrastructure {
                    annotations: Some(std::collections::BTreeMap::from([(
                        "key1".to_owned(),
                        "value1".to_owned(),
                    )])),
                    labels: Some(std::collections::BTreeMap::from([(
                        "key2".to_owned(),
                        "value2".to_owned(),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            },
            status: None,
        };

        let service = build_service("praxis-svc", "default", &gateway, vec![]).unwrap();

        assert_eq!(
            service.metadata.labels.as_ref().and_then(|l| l.get("key2")),
            Some(&"value2".to_owned()),
            "spec.infrastructure.labels asks for these on every generated resource"
        );
        assert_eq!(
            service.metadata.annotations.as_ref().and_then(|a| a.get("key1")),
            Some(&"value1".to_owned()),
            "spec.infrastructure.annotations likewise"
        );
        assert_eq!(
            service
                .spec
                .and_then(|s| s.selector)
                .and_then(|s| s.get("key2").cloned()),
            None,
            "an infrastructure label in the selector would make the Service unpatchable the first \
             time someone edited it"
        );
    }
}
