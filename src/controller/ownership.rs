// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Ownership resolution for a Gateway reconcile.
//!
//! Answers the two questions the reconciler asks before doing any work:
//! does this operator own the Gateway, and which routes does that
//! Gateway own. Both are cheap checks that short-circuit the expensive
//! path, so they run first and live together.

use std::sync::Arc;

use gateway_api::{gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute};
use kube::{Api, ResourceExt as _};
use tracing::debug;

use super::namespace_filter;
use crate::{
    context::CONTROLLER_NAME,
    error::{OperatorError, Result},
    gateway_api::attachment::{self, AttachedRoute},
    stores::Stores,
};

// -----------------------------------------------------------------------------
// GatewayClass Validation
// -----------------------------------------------------------------------------

/// Validates that the Gateway's `GatewayClass` exists and belongs to this
/// controller.
///
/// Returns `Ok(true)` when the class is ours, `Ok(false)` when it belongs
/// to another controller (caller should skip), and `Err` on lookup failure
/// or missing class.
pub(super) async fn validate_gateway_class(client: &kube::Client, gw: &Gateway) -> Result<bool> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let gc_name = &gw.spec.gateway_class_name;

    let gc = fetch_gateway_class(client, gc_name).await?;

    if gc.spec.controller_name != CONTROLLER_NAME {
        debug!("ignoring Gateway {ns}/{name}: GatewayClass {gc_name} not ours");
        return Ok(false);
    }

    Ok(true)
}

/// Fetches a `GatewayClass` by name, mapping API errors.
async fn fetch_gateway_class(client: &kube::Client, gc_name: &str) -> Result<GatewayClass> {
    let api = Api::<GatewayClass>::all(client.clone());
    api.get(gc_name).await.map_err(|e| map_gc_error(e, gc_name))
}

/// Maps a `GatewayClass` lookup error to an operator error.
fn map_gc_error(e: kube::Error, gc_name: &str) -> OperatorError {
    if is_api_not_found(&e) {
        debug!("GatewayClass {gc_name} not found");
        return OperatorError::GatewayClassNotFound(gc_name.to_owned());
    }

    debug!(%e, "GatewayClass lookup failed");
    OperatorError::Kube(e)
}

/// Returns `true` when the error is a 404 API response.
fn is_api_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(resp) if resp.code == 404)
}

// -----------------------------------------------------------------------------
// Route Collection
// -----------------------------------------------------------------------------

/// Collects `HTTPRoute` resources attached to the Gateway, filtered by
/// namespace policies.
pub(super) fn collect_routes<'a>(
    gw: &Gateway,
    all_routes: &'a [Arc<HTTPRoute>],
    stores: &Stores,
) -> Vec<AttachedRoute<'a>> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let attached = attachment::attached_routes(&name, &ns, &gw.spec.listeners, all_routes);
    namespace_filter::filter_routes_by_allowed_namespaces(&attached, &gw.spec.listeners, &ns, stores)
}

// -----------------------------------------------------------------------------

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------
