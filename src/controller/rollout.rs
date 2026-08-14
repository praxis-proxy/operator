// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Data-plane rollout inspection.
//!
//! The Gateway controller must not accept routes while the `Deployment`
//! is still rolling out the previous config, so it reads the hash the
//! running pods were created with and waits for the new `ReplicaSet` to
//! become available before reporting the route as accepted.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentStatus};
use kube::Api;

// -----------------------------------------------------------------------------
// Rollout State
// -----------------------------------------------------------------------------

/// Reads the current config hash from the Deployment's pod template.
///
/// Returns `None` if the Deployment doesn't exist or has no hash.
pub(super) async fn current_deployment_hash(client: &kube::Client, ns: &str, child: &str) -> Option<String> {
    Api::<Deployment>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|d| {
            d.spec?
                .template
                .metadata?
                .annotations?
                .get("praxis.sh/config-hash")
                .cloned()
        })
}

/// Returns `true` when the Deployment's rollout is complete.
///
/// Uses the `Progressing` condition reason `NewReplicaSetAvailable`,
/// which the deployment controller sets only after the new
/// `ReplicaSet` has all desired pods ready. This is immune to
/// stale-status races in back-to-back reconciliations.
pub(super) async fn is_deployment_rolled_out(client: &kube::Client, ns: &str, child: &str) -> bool {
    let Ok(d) = Api::<Deployment>::namespaced(client.clone(), ns).get(child).await else {
        return false;
    };
    let generation = d.metadata.generation.unwrap_or(0);
    let Some(status) = d.status.as_ref() else {
        return false;
    };
    if status.observed_generation.unwrap_or(0) < generation {
        return false;
    }
    is_new_rs_available(status)
}

/// Returns `true` when the `Progressing` condition has reason
/// `NewReplicaSetAvailable`.
fn is_new_rs_available(status: &DeploymentStatus) -> bool {
    status
        .conditions
        .as_ref()
        .and_then(|c| c.iter().find(|c| c.type_ == "Progressing"))
        .is_some_and(|c| c.status == "True" && c.reason.as_deref() == Some("NewReplicaSetAvailable"))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::fixtures::deployment_status;

    #[test]
    fn test_is_new_rs_available_true() {
        let status = deployment_status("Progressing", "True", "NewReplicaSetAvailable");

        assert!(
            is_new_rs_available(&status),
            "NewReplicaSetAvailable marks a finished rollout"
        );
    }

    #[test]
    fn test_is_new_rs_available_rejects_in_progress_rollout() {
        let status = deployment_status("Progressing", "True", "ReplicaSetUpdated");

        assert!(
            !is_new_rs_available(&status),
            "an updating ReplicaSet is not a finished rollout"
        );
    }

    #[test]
    fn test_is_new_rs_available_without_conditions() {
        assert!(
            !is_new_rs_available(&DeploymentStatus::default()),
            "a Deployment with no conditions has not rolled out"
        );
    }
}
