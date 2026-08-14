// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `GatewayClass` reconciler.

use std::{sync::Arc, time::Duration};

use gateway_api::gatewayclasses::GatewayClass;
use kube::{
    Api, ResourceExt as _,
    api::{Patch, PatchParams},
    runtime::controller::Action,
};
use tracing::{debug, error, info};

use crate::{
    context::{CONTROLLER_NAME, Context},
    error::{OperatorError, Result},
    gateway_api::{conditions, status},
};

// -----------------------------------------------------------------------------
// Reconciler
// -----------------------------------------------------------------------------

/// Reconciles a [`GatewayClass`] by setting its `Accepted` condition.
///
/// Only processes `GatewayClasses` whose `controller_name` matches this
/// operator. Unrelated `GatewayClasses` are ignored via [`Action::await_change`].
pub(crate) async fn reconcile(gc: Arc<GatewayClass>, ctx: Arc<Context>) -> Result<Action> {
    let name = gc.name_any();
    info!("reconciling GatewayClass {name}");

    if !is_our_controller(&gc, &name) {
        return Ok(Action::await_change());
    }

    accept_gateway_class(&gc, &name, &ctx).await?;
    Ok(Action::await_change())
}

/// Returns `true` when the `GatewayClass` belongs to this controller.
fn is_our_controller(gc: &GatewayClass, name: &str) -> bool {
    if gc.spec.controller_name != CONTROLLER_NAME {
        debug!(
            controller = gc.spec.controller_name,
            "ignoring GatewayClass {name}: not our controller"
        );
        return false;
    }
    true
}

/// Sets the `Accepted` condition on a `GatewayClass`.
///
/// Carries condition transition times forward and skips the patch when
/// the computed status already matches the live object, so an accepted
/// class does not re-trigger the controller's own watch.
async fn accept_gateway_class(gc: &GatewayClass, name: &str, ctx: &Context) -> Result<()> {
    let generation = gc.metadata.generation.unwrap_or(0);
    let observed = serde_json::to_value(&gc.status)?;

    let mut desired = build_accepted_status(generation);
    status::preserve_condition_times(&mut desired, &observed);

    if status::is_status_unchanged(&desired, &observed) {
        debug!("GatewayClass {name} status unchanged, skipping patch");
        return Ok(());
    }

    let payload = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "GatewayClass",
        "metadata": { "name": name },
        "status": desired,
    });

    let api = Api::<GatewayClass>::all(ctx.client.clone());
    api.patch_status(
        name,
        &PatchParams::apply("praxis-operator").force(),
        &Patch::Apply(&payload),
    )
    .await?;

    info!("GatewayClass {name} accepted");
    Ok(())
}

// -----------------------------------------------------------------------------
// Status Builders
// -----------------------------------------------------------------------------

/// Builds the `status` sub-object of the accepted patch.
///
/// Sets the `Accepted` condition to `True` and declares supported features.
fn build_accepted_status(generation: i64) -> serde_json::Value {
    let condition = conditions::accepted(generation, "GatewayClass accepted");

    serde_json::json!({
        "conditions": [condition],
        "supportedFeatures": [
            { "name": "Gateway" },
            { "name": "HTTPRoute" },
        ],
    })
}

/// Error policy for `GatewayClass` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub(crate) fn error_policy(_gc: Arc<GatewayClass>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    error!(%error, "GatewayClass reconciliation failed");
    Action::requeue(Duration::from_secs(30))
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
    use gateway_api::gatewayclasses::GatewayClassSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_is_our_controller_accepts_matching_name() {
        assert!(
            is_our_controller(&gateway_class(CONTROLLER_NAME), "praxis"),
            "a GatewayClass naming this controller belongs to us"
        );
    }

    #[test]
    fn test_is_our_controller_rejects_other_controllers() {
        assert!(
            !is_our_controller(&gateway_class("example.com/other"), "other"),
            "a GatewayClass naming another controller must be ignored"
        );
    }

    #[test]
    fn test_build_accepted_status_sets_accepted_true() {
        let status = build_accepted_status(3);

        assert_eq!(
            status["conditions"][0]["type"], "Accepted",
            "the Accepted condition should be written"
        );
        assert_eq!(status["conditions"][0]["status"], "True", "an owned class is accepted");
        assert_eq!(
            status["conditions"][0]["observedGeneration"], 3,
            "the observed generation should be carried"
        );
    }

    #[test]
    fn test_build_accepted_status_declares_supported_features() {
        let status = build_accepted_status(1);

        assert_eq!(
            status["supportedFeatures"].as_array().map(Vec::len),
            Some(2),
            "the advertised feature list should be present"
        );
    }

    #[test]
    fn test_build_accepted_status_carries_no_metadata() {
        let status = build_accepted_status(1);

        assert!(
            status.get("metadata").is_none(),
            "the builder returns only the status sub-object, so it can be diffed against the live status"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds a `GatewayClass` registered to the given controller name.
    fn gateway_class(controller_name: &str) -> GatewayClass {
        GatewayClass {
            metadata: ObjectMeta {
                name: Some("praxis".to_owned()),
                ..Default::default()
            },
            spec: GatewayClassSpec {
                controller_name: controller_name.to_owned(),
                ..Default::default()
            },
            status: None,
        }
    }
}
