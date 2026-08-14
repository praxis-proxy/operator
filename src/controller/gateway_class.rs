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
    context::{CONTROLLER_NAME, Context, FIELD_MANAGER},
    error::{OperatorError, Result},
    gateway_api::{
        conditions, status,
        status_types::{self, GatewayClassStatus, SupportedFeature},
    },
    observability::metrics,
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Gateway API features this operator implements.
///
/// Advertised through `GatewayClass.status.supportedFeatures` so that
/// conformance tooling runs only the suites this implementation claims.
/// Every entry must correspond to behaviour that is actually reachable:
/// an over-claim turns a passing run into a false negative elsewhere.
/// Kept sorted, which `test_supported_features_are_sorted_and_unique`
/// enforces.
/// Deliberately absent: `HTTPRouteRequestMirror` and
/// `HTTPRouteRequestMultipleMirrors` (the `RequestMirror` filter),
/// `HTTPRouteMethodMatching` and `HTTPRouteQueryParamMatching`. Every
/// one is rejected by [`validate_route`] — the filter because Praxis
/// registers none that mirrors a request, the two match kinds because
/// `praxis_core::config::Route` has no field to carry them.
/// Advertising any of them would direct conformance tooling at suites
/// that cannot pass.
///
/// `HTTPRouteResponseHeaderModification` was claimed here and withdrawn.
/// Claiming it is what made conformance run
/// `HTTPRouteResponseHeaderModifier` at all — the suites are gated on
/// advertised features, which is why the test was skipped before.
/// Running it showed praxis 0.3.1 implemented the filter only in part:
/// `set` and `remove` behaved, but `add` replaced the existing header
/// rather than appending, so conformance asked for
/// `append-val-1,header-val-2` and got `header-val-2`.
///
/// Praxis 0.5.2, now the pinned data plane, appends: `request_add`
/// reads the existing values and combines them. Both header-modification
/// features are claimed again on that basis, and the conformance suites
/// they gate are no longer skipped.
///
/// The two rewrite features rest on the same release. Praxis registers
/// a `path_rewrite` filter whose `strip_prefix` and regex `replace`
/// operations cover both Gateway API path modifiers, and it forwards
/// the `Host` header untouched, so setting that header is the hostname
/// rewrite.
///
/// The two timeout features are claimed with a caveat worth stating.
/// Praxis's `timeout` filter compares elapsed time in the response
/// phase, so it converts a late response into a 504 but does not abort
/// a request the upstream never answers. That is the whole of what the
/// conformance suite exercises, and it is the behaviour a client sees
/// from any backend that eventually replies; a hung backend still
/// hangs. Withdraw both if that gap matters more than the coverage.
///
/// The redirect features need no code of their own: the `redirect`
/// filter already carries scheme and status through, and Praxis
/// accepts 301, 302, 307 and 308. `HTTPRoute303RedirectStatusCode` is
/// absent because it accepts no 303, and `HTTPRoutePathRedirect`
/// because its `location` template offers `${path}` whole and no way
/// to substitute part of it, which `ReplacePrefixMatch` needs.
/// `HTTPRoutePortRedirect` is absent for a different reason: the suite
/// gating on it requires omitting a default port for the listener's
/// own scheme, and filter entries are built once per Gateway rather
/// than once per listener, so that scheme is not known where the
/// location is assembled.
///
/// [`validate_route`]: crate::gateway_api::route_validation::validate_route
const SUPPORTED_FEATURES: &[&str] = &[
    "Gateway",
    "GatewayInfrastructurePropagation",
    "GatewayPort8080",
    "HTTPRoute",
    "HTTPRoute307RedirectStatusCode",
    "HTTPRoute308RedirectStatusCode",
    "HTTPRouteBackendTimeout",
    "HTTPRouteHostRewrite",
    "HTTPRoutePathRewrite",
    "HTTPRouteRequestHeaderModification",
    "HTTPRouteRequestTimeout",
    "HTTPRouteResponseHeaderModification",
    "HTTPRouteSchemeRedirect",
    "ReferenceGrant",
];

// -----------------------------------------------------------------------------
// Reconciler
// -----------------------------------------------------------------------------

/// Reconciles a [`GatewayClass`] by setting its `Accepted` condition.
///
/// Only processes `GatewayClasses` whose `controller_name` matches this
/// operator. Unrelated `GatewayClasses` are ignored via [`Action::await_change`].
///
/// # Errors
///
/// Returns an error if patching the `GatewayClass` status fails. The
/// error reaches [`error_policy`], which requeues with backoff.
pub async fn reconcile(gc: Arc<GatewayClass>, ctx: Arc<Context>) -> Result<Action> {
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

    let mut desired = build_accepted_status(generation)?;
    status::preserve_condition_times(&mut desired, &observed);

    if status::is_status_unchanged(&desired, &observed) {
        metrics::global().record_status_skipped();
        debug!("GatewayClass {name} status unchanged, skipping patch");
        return Ok(());
    }
    metrics::global().record_status_written();

    let payload = status_types::status_patch("GatewayClass", name, None, desired);

    let api = Api::<GatewayClass>::all(ctx.client.clone());
    api.patch_status(
        name,
        &PatchParams::apply(FIELD_MANAGER).force(),
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
fn build_accepted_status(generation: i64) -> serde_json::Result<serde_json::Value> {
    let features = SUPPORTED_FEATURES
        .iter()
        .map(|name| SupportedFeature {
            name: (*name).to_owned(),
        })
        .collect();

    serde_json::to_value(GatewayClassStatus {
        conditions: vec![conditions::accepted(generation, "GatewayClass accepted")],
        supported_features: features,
    })
}

/// Error policy for `GatewayClass` reconciliation failures.
///
/// Logs the error and requeues after 30 seconds.
pub fn error_policy(_gc: Arc<GatewayClass>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    error!(%error, "GatewayClass reconciliation failed");
    Action::requeue(Duration::from_secs(30))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
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
        let status = build_accepted_status(3).expect("a class status is strings and conditions");

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
        let status = build_accepted_status(1).expect("a class status is strings and conditions");

        assert_eq!(
            status["supportedFeatures"].as_array().map(Vec::len),
            Some(SUPPORTED_FEATURES.len()),
            "every advertised feature should reach the status"
        );
    }

    #[test]
    fn test_supported_features_are_sorted_and_unique() {
        let mut sorted = SUPPORTED_FEATURES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(
            sorted,
            SUPPORTED_FEATURES.to_vec(),
            "the feature list must stay sorted and free of duplicates so diffs stay readable"
        );
    }

    #[test]
    fn test_supported_features_include_the_core_kinds() {
        assert!(
            SUPPORTED_FEATURES.contains(&"Gateway") && SUPPORTED_FEATURES.contains(&"HTTPRoute"),
            "the two core kinds this operator reconciles must be advertised"
        );
    }

    #[test]
    fn test_build_accepted_status_carries_no_metadata() {
        let status = build_accepted_status(1).expect("a class status is strings and conditions");

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
