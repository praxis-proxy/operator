// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `ConfigMap` builder for Praxis configuration.

use std::collections::BTreeMap;

use gateway_api::gateways::Gateway;
use k8s_openapi::{api::core::v1::ConfigMap, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use kube::ResourceExt;

use super::labels::{owner_reference, standard_labels};

// -----------------------------------------------------------------------------
// ConfigMap Builder
// -----------------------------------------------------------------------------

/// Builds a `ConfigMap` containing Praxis configuration YAML.
///
/// The `ConfigMap` is owned by the `Gateway` resource and labeled with standard
/// Praxis labels. Contains a single data key `config.yaml` with the provided
/// YAML content.
/// # Errors
///
/// Returns an error if the Gateway has no UID.
pub(crate) fn build_configmap(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    config_yaml: &str,
) -> crate::error::Result<ConfigMap> {
    let mut data = BTreeMap::new();
    data.insert("config.yaml".to_owned(), config_yaml.to_owned());

    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            labels: Some(standard_labels(&gateway.name_any())),
            ..Default::default()
        },
        data: Some(data),
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
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_build_configmap_metadata() {
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

        let config = build_configmap(
            "praxis-config",
            "default",
            &gateway,
            "admin:\n  address: 0.0.0.0:9901\n",
        )
        .unwrap();

        assert_eq!(
            config.metadata.name,
            Some("praxis-config".to_owned()),
            "name should match"
        );
        assert_eq!(
            config.metadata.namespace,
            Some("default".to_owned()),
            "namespace should match"
        );

        let labels = config.metadata.labels.expect("labels should be set");
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
        assert_eq!(
            labels.get("app.kubernetes.io/managed-by"),
            Some(&"praxis-operator".to_owned()),
            "managed-by label should be set"
        );

        let owner_refs = config
            .metadata
            .owner_references
            .expect("owner references should be set");
        assert_eq!(owner_refs.len(), 1, "should have one owner reference");
        assert_eq!(owner_refs[0].kind, "Gateway", "owner kind should be Gateway");
        assert_eq!(owner_refs[0].name, "test-gateway", "owner name should match");
        assert_eq!(owner_refs[0].uid, "test-uid", "owner uid should match");
        assert_eq!(owner_refs[0].controller, Some(true), "controller should be true");
    }

    #[test]
    fn test_build_configmap_data() {
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

        let yaml_content = "admin:\n  address: 0.0.0.0:9901\nlisteners: []\n";
        let config = build_configmap("praxis-config", "default", &gateway, yaml_content).unwrap();

        let data = config.data.expect("data should be set");
        assert_eq!(data.len(), 1, "should have one data key");
        assert_eq!(
            data.get("config.yaml"),
            Some(&yaml_content.to_owned()),
            "config.yaml should contain the YAML content"
        );
    }

    #[test]
    fn test_build_configmap_different_namespace() {
        let gateway = Gateway {
            metadata: ObjectMeta {
                name: Some("prod-gateway".to_owned()),
                namespace: Some("production".to_owned()),
                uid: Some("prod-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };

        let config = build_configmap("praxis-prod-config", "production", &gateway, "test: data\n").unwrap();

        assert_eq!(
            config.metadata.name,
            Some("praxis-prod-config".to_owned()),
            "name should match"
        );
        assert_eq!(
            config.metadata.namespace,
            Some("production".to_owned()),
            "namespace should match"
        );
    }
}
