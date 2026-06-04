// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `GatewayClass` reconciler.

use std::{sync::Arc, time::Duration};

use gateway_api::gatewayclasses::GatewayClass;
use kube::{
    Api, ResourceExt,
    api::{Patch, PatchParams},
    runtime::controller::Action,
};
use tracing::{debug, error, info};

use crate::{
    context::{CONTROLLER_NAME, Context},
    error::{Error, Result},
    gateway_api::conditions,
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
async fn accept_gateway_class(gc: &GatewayClass, name: &str, ctx: &Context) -> Result<()> {
    let generation = gc.metadata.generation.unwrap_or(0);
    let status = build_accepted_status(name, generation);

    let api = Api::<GatewayClass>::all(ctx.client.clone());
    api.patch_status(
        name,
        &PatchParams::apply("praxis-operator").force(),
        &Patch::Apply(&status),
    )
    .await?;

    info!("GatewayClass {name} accepted");
    Ok(())
}

// -----------------------------------------------------------------------------
// Status Builders
// -----------------------------------------------------------------------------

/// Builds the accepted status patch for a [`GatewayClass`].
///
/// Sets the `Accepted` condition to `True` and declares supported features.
fn build_accepted_status(name: &str, generation: i64) -> serde_json::Value {
    let condition = conditions::accepted(generation, "GatewayClass accepted");

    serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "GatewayClass",
        "metadata": { "name": name },
        "status": {
            "conditions": [condition],
            "supportedFeatures": [
                { "name": "Gateway" },
                { "name": "HTTPRoute" },
            ],
        },
    })
}

/// Error policy for `GatewayClass` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub(crate) fn error_policy(_gc: Arc<GatewayClass>, error: &Error, _ctx: Arc<Context>) -> Action {
    error!(%error, "GatewayClass reconciliation failed");
    Action::requeue(Duration::from_secs(30))
}
