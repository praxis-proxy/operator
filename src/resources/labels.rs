// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Standard labels, selectors, and owner references for managed resources.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;

// -----------------------------------------------------------------------------
// Labels and Naming
// -----------------------------------------------------------------------------

/// Returns standard Praxis labels for a given instance.
///
/// Includes `app.kubernetes.io/name`, `app.kubernetes.io/instance`, and
/// `app.kubernetes.io/managed-by`.
pub(crate) fn standard_labels(instance: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("app.kubernetes.io/name".to_owned(), "praxis".to_owned());
    labels.insert("app.kubernetes.io/instance".to_owned(), instance.to_owned());
    labels.insert("app.kubernetes.io/managed-by".to_owned(), "praxis-operator".to_owned());
    labels
}

/// Returns the child resource name for a given Gateway name.
///
/// Prefixes the gateway name with `praxis-` to form the deployment and service
/// names.
pub(crate) fn child_name(gateway_name: &str) -> String {
    format!("praxis-{gateway_name}")
}

/// Returns an `OwnerReference` for a `Gateway` resource.
///
/// Sets `controller: true` and `block_owner_deletion: true` so the child
/// resource lifecycle is bound to the Gateway.
///
/// # Errors
///
/// Returns [`Error::MissingObjectKey`] when the Gateway has no UID.
///
/// [`Error::MissingObjectKey`]: crate::error::Error::MissingObjectKey
pub(crate) fn owner_reference(gateway: &gateway_api::gateways::Gateway) -> crate::error::Result<OwnerReference> {
    Ok(OwnerReference {
        api_version: "gateway.networking.k8s.io/v1".to_owned(),
        block_owner_deletion: Some(true),
        controller: Some(true),
        kind: "Gateway".to_owned(),
        name: gateway.name_any(),
        uid: gateway
            .uid()
            .ok_or(crate::error::Error::MissingObjectKey(".metadata.uid"))?,
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
    use super::*;

    #[test]
    fn test_standard_labels() {
        let labels = standard_labels("test-instance");
        assert_eq!(labels.get("app.kubernetes.io/name"), Some(&"praxis".to_owned()));
        assert_eq!(
            labels.get("app.kubernetes.io/instance"),
            Some(&"test-instance".to_owned())
        );
        assert_eq!(
            labels.get("app.kubernetes.io/managed-by"),
            Some(&"praxis-operator".to_owned())
        );
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn test_child_name() {
        assert_eq!(child_name("my-gateway"), "praxis-my-gateway");
        assert_eq!(child_name(""), "praxis-");
    }

    #[test]
    fn test_owner_reference() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

        let gateway = gateway_api::gateways::Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                uid: Some("test-uid-123".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };

        let owner_ref = owner_reference(&gateway).unwrap();
        assert_eq!(owner_ref.api_version, "gateway.networking.k8s.io/v1");
        assert_eq!(owner_ref.kind, "Gateway");
        assert_eq!(owner_ref.name, "test-gateway");
        assert_eq!(owner_ref.uid, "test-uid-123");
        assert_eq!(owner_ref.controller, Some(true));
        assert_eq!(owner_ref.block_owner_deletion, Some(true));
    }
}
