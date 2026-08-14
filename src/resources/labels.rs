// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Standard labels, selectors, and owner references for managed resources.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt as _;

// -----------------------------------------------------------------------------
// Labels and Naming
// -----------------------------------------------------------------------------

/// Returns standard Praxis labels for a given instance.
///
/// Includes `app.kubernetes.io/name`, `app.kubernetes.io/instance`, and
/// `app.kubernetes.io/managed-by`.
pub fn standard_labels(instance: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("app.kubernetes.io/name".to_owned(), "praxis".to_owned());
    labels.insert("app.kubernetes.io/instance".to_owned(), instance.to_owned());
    labels.insert("app.kubernetes.io/managed-by".to_owned(), "praxis-operator".to_owned());
    labels
}

/// Label naming the Gateway a generated resource was created for.
///
/// Standardised by the Gateway API so that tooling can find an
/// implementation's generated objects without knowing its naming
/// scheme. Conformance uses it as a list selector.
pub const GATEWAY_NAME_LABEL: &str = "gateway.networking.k8s.io/gateway-name";

/// Returns the labels every generated resource carries, beyond the
/// selector.
///
/// Deliberately separate from [`standard_labels`]: that set is the
/// `Deployment` and `Service` selector, which Kubernetes will not let
/// the operator change once created. Anything that can vary with the
/// Gateway spec — the operator-declared labels below — has to stay out
/// of it, or the first Gateway to edit `spec.infrastructure` would
/// leave the operator unable to apply its own `Deployment`.
pub fn descriptive_labels(gateway: &gateway_api::gateways::Gateway) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([(GATEWAY_NAME_LABEL.to_owned(), gateway.name_any())]);
    labels.extend(infrastructure_labels(gateway));
    labels
}

/// Returns the labels the Gateway asks generated resources to carry.
pub fn infrastructure_labels(gateway: &gateway_api::gateways::Gateway) -> BTreeMap<String, String> {
    gateway
        .spec
        .infrastructure
        .as_ref()
        .and_then(|infra| infra.labels.clone())
        .unwrap_or_default()
}

/// Returns the annotations the Gateway asks generated resources to
/// carry.
pub fn infrastructure_annotations(gateway: &gateway_api::gateways::Gateway) -> BTreeMap<String, String> {
    gateway
        .spec
        .infrastructure
        .as_ref()
        .and_then(|infra| infra.annotations.clone())
        .unwrap_or_default()
}

/// Longest name Kubernetes accepts for the objects this operator
/// creates.
///
/// `ConfigMap`, `Deployment`, `Service` and `PodDisruptionBudget` names
/// are DNS labels, capped at 63 characters. A Gateway name may be up to
/// 253, and the `praxis-` prefix spends seven of the 63, so anything
/// past 56 characters overflows.
const MAX_CHILD_NAME: usize = 63;

/// Characters of the digest appended to a truncated name.
const DIGEST_LEN: usize = 8;

/// Returns the child resource name for a given Gateway name.
///
/// Prefixes the gateway name with `praxis-` to form the `ConfigMap`,
/// `Deployment`, `Service` and `PodDisruptionBudget` names.
///
/// A name that would exceed the 63-character limit is truncated and
/// given a digest of the full Gateway name. Without that, every child
/// apply for such a Gateway is rejected as invalid, the reconcile
/// fails before it writes a status, and the Gateway sits forever on
/// the `Accepted: Unknown` the CRD defaults to — with nothing to say
/// why. The digest is what keeps two long Gateways sharing a prefix
/// from sharing a Deployment.
///
/// ```
/// use praxis_operator::resources::labels::child_name;
///
/// assert_eq!(child_name("my-gateway"), "praxis-my-gateway");
///
/// // 57 characters, one past what the prefix leaves room for.
/// let long = "gateway-with-one-not-matching-port-and-section-name-route";
/// assert!(child_name(long).len() <= 63);
/// assert_ne!(child_name(long), child_name(&format!("{long}-two")));
/// ```
pub fn child_name(gateway_name: &str) -> String {
    let full = format!("praxis-{gateway_name}");
    if full.len() <= MAX_CHILD_NAME {
        return full;
    }

    let digest = short_digest(gateway_name);
    let keep = MAX_CHILD_NAME - DIGEST_LEN - 1;
    let head: String = full.chars().take(keep).collect();
    format!("{}-{digest}", head.trim_end_matches('-'))
}

/// Returns the first [`DIGEST_LEN`] hex characters of the SHA-256 of
/// `value`.
fn short_digest(value: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(value.as_bytes());
    format!("{digest:x}").chars().take(DIGEST_LEN).collect()
}

/// Returns an `OwnerReference` for a `Gateway` resource.
///
/// Sets `controller: true` and `block_owner_deletion: true` so the child
/// resource lifecycle is bound to the Gateway.
///
/// # Errors
///
/// Returns [`OperatorError::MissingObjectKey`] when the Gateway has no UID.
///
/// [`OperatorError::MissingObjectKey`]: crate::error::OperatorError::MissingObjectKey
pub fn owner_reference(gateway: &gateway_api::gateways::Gateway) -> crate::error::Result<OwnerReference> {
    Ok(OwnerReference {
        api_version: "gateway.networking.k8s.io/v1".to_owned(),
        block_owner_deletion: Some(true),
        controller: Some(true),
        kind: "Gateway".to_owned(),
        name: gateway.name_any(),
        uid: gateway
            .uid()
            .ok_or(crate::error::OperatorError::MissingObjectKey(".metadata.uid"))?,
    })
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::default_trait_access, reason = "tests")]
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
    fn test_a_name_at_the_limit_is_left_alone() {
        let name = "g".repeat(MAX_CHILD_NAME - "praxis-".len());

        assert_eq!(
            child_name(&name),
            format!("praxis-{name}"),
            "truncating a name that already fits would rename the children of every Gateway near \
             the limit, orphaning what it had already created"
        );
    }

    #[test]
    fn test_an_overlong_name_is_cut_to_the_limit() {
        let name = "g".repeat(200);

        let child = child_name(&name);

        assert_eq!(
            child.len(),
            MAX_CHILD_NAME,
            "Kubernetes rejects a longer object name outright, and every child apply fails with it"
        );
    }

    #[test]
    fn test_trimming_a_trailing_dash_may_come_in_under_the_limit() {
        let name = "gateway-with-one-not-matching-port-and-section-name-route";

        let child = child_name(name);

        assert!(
            child.len() <= MAX_CHILD_NAME,
            "the cap is a ceiling, not a target: {child}"
        );
        assert!(
            !child.contains("--"),
            "trimming the dash the cut landed on must not leave a doubled one: {child}"
        );
    }

    #[test]
    fn test_two_overlong_names_sharing_a_prefix_stay_distinct() {
        let base = "g".repeat(200);

        assert_ne!(
            child_name(&format!("{base}-one")),
            child_name(&format!("{base}-two")),
            "truncation alone would give two Gateways the same Deployment, and each reconcile \
             would overwrite the other's config"
        );
    }

    #[test]
    fn test_a_truncated_name_is_a_valid_dns_label() {
        let name = format!("{}-", "g".repeat(60));

        let child = child_name(&name);

        assert!(
            child
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "a name outside the DNS label charset is rejected as surely as a long one: {child}"
        );
        assert!(
            !child.starts_with('-') && !child.ends_with('-'),
            "a leading or trailing dash is not a DNS label: {child}"
        );
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
