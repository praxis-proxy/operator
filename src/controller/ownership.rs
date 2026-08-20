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
    api.get(gc_name).await.map_err(|err| map_gc_error(err, gc_name))
}

/// Maps a `GatewayClass` lookup error to an operator error.
fn map_gc_error(err: kube::Error, gc_name: &str) -> OperatorError {
    if is_api_not_found(&err) {
        debug!("GatewayClass {gc_name} not found");
        return OperatorError::GatewayClassNotFound(gc_name.to_owned());
    }

    debug!(%err, "GatewayClass lookup failed");
    OperatorError::Kube(err)
}

/// Returns `true` when the error is a 404 API response.
fn is_api_not_found(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(resp) if resp.code == 404)
}

// -----------------------------------------------------------------------------
// Route Collection
// -----------------------------------------------------------------------------

/// Collects `HTTPRoute` resources attached to the Gateway, filtered by
/// namespace policies.
pub(super) fn collect_routes<'route>(
    gw: &Gateway,
    all_routes: &'route [Arc<HTTPRoute>],
    stores: &Stores,
) -> Vec<AttachedRoute<'route>> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let attached = attachment::attached_routes(&name, &ns, &gw.spec.listeners, all_routes);
    namespace_filter::filter_routes_by_allowed_namespaces(&attached, &gw.spec.listeners, &ns, stores)
}

// -----------------------------------------------------------------------------

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use gateway_api::{
        gateways::{GatewayListeners, GatewaySpec},
        httproutes::{HttpRouteParentRefs, HttpRouteSpec},
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use serde_json::json;

    use super::*;
    use crate::testing;

    // -----------------------------------------------------------------------
    // GatewayClass Validation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_class_naming_this_controller_is_ours() {
        let (client, _) = testing::fake_client(vec![class_response(CONTROLLER_NAME)]);

        assert!(
            validate_gateway_class(&client, &gateway())
                .await
                .expect("the class exists"),
            "a Gateway whose class names this controller is ours to reconcile"
        );
    }

    #[tokio::test]
    async fn test_a_class_naming_another_controller_is_skipped() {
        let (client, _) = testing::fake_client(vec![class_response("example.com/other")]);

        assert!(
            !validate_gateway_class(&client, &gateway())
                .await
                .expect("the class exists"),
            "reconciling another controller's Gateway would fight it for every child resource"
        );
    }

    #[tokio::test]
    async fn test_a_missing_class_is_its_own_error() {
        let (client, _) = testing::fake_client(vec![]);

        let error = validate_gateway_class(&client, &gateway())
            .await
            .expect_err("a Gateway naming no existing class cannot be reconciled");

        assert!(
            matches!(&error, OperatorError::GatewayClassNotFound(name) if name == "praxis"),
            "a 404 is a user-visible misconfiguration and gets its own variant, not a generic API \
             error the reconciler would retry forever: {error}"
        );
    }

    #[tokio::test]
    async fn test_a_failed_lookup_stays_an_api_error() {
        let (client, _) = testing::failing_client();

        let error = validate_gateway_class(&client, &gateway())
            .await
            .expect_err("a 500 is not an answer");

        assert!(
            matches!(error, OperatorError::Kube(_)),
            "an API server that is merely down must be retried, not reported as a missing class: \
             {error}"
        );
    }

    // -----------------------------------------------------------------------
    // Route Collection
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_routes_returns_the_routes_naming_this_gateway() {
        let routes = vec![Arc::new(route("mine", "gw")), Arc::new(route("theirs", "other-gw"))];
        let stores = Stores::fake(vec![], vec![], vec![]);

        let collected = collect_routes(&gateway(), &routes, &stores);

        let names: Vec<_> = collected.iter().filter_map(|a| a.route.metadata.name.clone()).collect();
        assert_eq!(
            names,
            vec!["mine".to_owned()],
            "a route naming another Gateway contributes nothing to this one's config"
        );
    }

    #[test]
    fn test_collect_routes_is_empty_when_nothing_attaches() {
        let routes = vec![Arc::new(route("theirs", "other-gw"))];
        let stores = Stores::fake(vec![], vec![], vec![]);

        assert!(
            collect_routes(&gateway(), &routes, &stores).is_empty(),
            "a Gateway with no attached routes gets an empty route table, not every route"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds the Gateway these tests reconcile.
    fn gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners: vec![GatewayListeners {
                    name: "http".to_owned(),
                    port: 80,
                    protocol: "HTTP".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds a route naming `parent` as its Gateway.
    fn route(name: &str, parent: &str) -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some("infra".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: parent.to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        }
    }

    /// The `GatewayClass` the fake API server hands back.
    fn class_response(controller: &str) -> testing::Canned {
        testing::Canned::ok(
            "/gatewayclasses/praxis",
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "GatewayClass",
                "metadata": { "name": "praxis" },
                "spec": { "controllerName": controller },
            }),
        )
    }
}
