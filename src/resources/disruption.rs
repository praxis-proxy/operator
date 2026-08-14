// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `PodDisruptionBudget` builder for the Praxis data plane.

use gateway_api::gateways::Gateway;
use k8s_openapi::{
    api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec},
    apimachinery::pkg::{
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use kube::ResourceExt as _;

use super::labels::{owner_reference, standard_labels};

// -----------------------------------------------------------------------------
// PodDisruptionBudget Builder
// -----------------------------------------------------------------------------

/// Builds a `PodDisruptionBudget` keeping one data-plane pod serving.
///
/// Guards against voluntary disruption only — a node drain or cluster
/// upgrade — which is exactly when an unprotected single-replica proxy
/// disappears and takes every attached route with it.
///
/// Expressed as `minAvailable: 1` rather than a percentage so the
/// meaning does not change when a Gateway is resized.
///
/// # Errors
///
/// Returns an error if the Gateway has no UID.
pub fn build_pod_disruption_budget(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
) -> crate::error::Result<PodDisruptionBudget> {
    let instance = gateway.name_any();
    let labels = standard_labels(&instance);

    Ok(PodDisruptionBudget {
        metadata: ObjectMeta {
            labels: Some(labels.clone()),
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            ..Default::default()
        },
        spec: Some(PodDisruptionBudgetSpec {
            min_available: Some(IntOrString::Int(1)),
            selector: Some(LabelSelector {
                match_labels: Some(labels),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: None,
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

    #[test]
    fn test_budget_keeps_one_pod_available() {
        let budget = build_pod_disruption_budget("praxis-gw", "default", &gateway()).unwrap();
        let spec = budget.spec.expect("spec should be set");

        assert_eq!(
            spec.min_available,
            Some(IntOrString::Int(1)),
            "a drain must never take the last data-plane pod"
        );
        assert!(
            spec.max_unavailable.is_none(),
            "minAvailable and maxUnavailable are mutually exclusive"
        );
    }

    #[test]
    fn test_budget_selects_the_gateway_pods() {
        let budget = build_pod_disruption_budget("praxis-gw", "default", &gateway()).unwrap();
        let selector = budget
            .spec
            .and_then(|spec| spec.selector)
            .and_then(|selector| selector.match_labels)
            .expect("selector should be set");

        assert_eq!(
            selector.get("app.kubernetes.io/instance"),
            Some(&"test-gateway".to_owned()),
            "the budget must select only this Gateway's pods"
        );
    }

    #[test]
    fn test_budget_is_owned_by_the_gateway() {
        let budget = build_pod_disruption_budget("praxis-gw", "default", &gateway()).unwrap();
        let owners = budget.metadata.owner_references.expect("owner refs should be set");

        assert_eq!(
            owners[0].kind, "Gateway",
            "the budget is garbage collected with its Gateway"
        );
        assert_eq!(owners[0].uid, "test-uid", "the owner uid should match");
    }

    #[test]
    fn test_budget_requires_a_gateway_uid() {
        let mut gw = gateway();
        gw.metadata.uid = None;

        assert!(
            build_pod_disruption_budget("praxis-gw", "default", &gw).is_err(),
            "without a uid the budget could not be garbage collected and must not be created"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a Gateway suitable for owning child resources.
    fn gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        }
    }
}
