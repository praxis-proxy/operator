// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Writing `Accepted` and `ResolvedRefs` onto attached routes.
//!
//! The Gateway controller owns route acceptance, but only once the data
//! plane is actually serving the matching configuration — otherwise a
//! client following the status would send traffic to a proxy that has
//! not caught up yet.

use gateway_api::{
    gateways::Gateway,
    httproutes::{HTTPRoute, HttpRouteParentRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::ResourceExt as _;

use crate::{
    error::Result,
    gateway_api::{
        attachment::AttachedRoute, conditions, route_status, route_validation, status_types::RouteParentStatus,
    },
};

// -----------------------------------------------------------------------------
// Route Parent Status
// -----------------------------------------------------------------------------

/// Updates parent status on attached `HTTPRoutes`.
///
/// Sets `Accepted = True` and evaluates `ResolvedRefs` for each route
/// that targets this Gateway. Called by the Gateway controller **after**
/// child resources are applied and the Deployment rollout is verified,
/// so the conformance test cannot send traffic before the data plane
/// is serving the matching configuration.
pub(super) async fn update_route_parent_statuses(
    client: &kube::Client,
    gw: &Gateway,
    attached: &[AttachedRoute<'_>],
    grants: &[ReferenceGrant],
) -> Result<()> {
    let gw_ns = gw.namespace().unwrap_or_default();
    let gw_name = gw.name_any();

    for AttachedRoute { route, .. } in attached {
        let route_ns = route_status::route_namespace(route);
        let generation = route.metadata.generation.unwrap_or(0);
        let Some(parent_refs) = &route.spec.parent_refs else {
            continue;
        };

        let statuses = build_route_statuses(
            route,
            parent_refs,
            route_ns,
            &gw_name,
            &gw_ns,
            generation,
            client,
            grants,
        )
        .await;

        if !statuses.is_empty() {
            route_status::apply_parent_statuses(client, route, &statuses).await?;
        }
    }
    Ok(())
}

/// Builds parent status entries for refs targeting this Gateway.
#[expect(clippy::too_many_arguments, reason = "route status needs full context")]
async fn build_route_statuses(
    route: &HTTPRoute,
    parent_refs: &[HttpRouteParentRefs],
    route_ns: &str,
    gw_name: &str,
    gw_ns: &str,
    generation: i64,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> Vec<RouteParentStatus> {
    let validation = route_validation::validate_route(route);

    let mut statuses = Vec::new();
    for parent_ref in parent_refs {
        if !route_status::is_ref_targeting_gateway(parent_ref, gw_name, gw_ns, route_ns) {
            continue;
        }

        let resolved = route_status::check_backend_refs(route, route_ns, client, grants).await;
        let resolved_cond = route_status::resolved_refs_condition(&resolved, generation);
        let mut route_conditions = validation_conditions(&validation, generation);
        route_conditions.push(resolved_cond);

        statuses.push(route_status::parent_status_with_conditions(
            parent_ref,
            gw_ns,
            &route_conditions,
        ));
    }
    statuses
}

/// Builds the `Accepted` condition, plus `PartiallyInvalid` when only
/// some rules were dropped.
fn validation_conditions(validation: &route_validation::RouteValidation, generation: i64) -> Vec<Condition> {
    let detail = validation.message().unwrap_or_default();

    if validation.is_fully_rejected() {
        return vec![conditions::not_accepted(generation, "UnsupportedValue", &detail)];
    }

    let accepted = conditions::accepted(generation, "route accepted");
    if validation.is_partially_rejected() {
        return vec![accepted, conditions::partially_invalid(generation, &detail)];
    }

    vec![accepted]
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use serde_json::Value;

    use super::*;
    use crate::{controller::fixtures::regex_route, gateway_api::route_validation, testing};

    #[test]
    fn test_validation_conditions_accepts_a_supported_route() {
        let conds = validation_conditions(&route_validation::RouteValidation::default(), 1);

        assert_eq!(conds.len(), 1, "a supported route needs only an Accepted condition");
        assert_eq!(conds[0].type_, "Accepted", "the condition should be Accepted");
        assert_eq!(conds[0].status, "True", "a supported route is accepted");
    }

    #[test]
    fn test_validation_conditions_rejects_a_fully_invalid_route() {
        let route = regex_route(1);
        let conds = validation_conditions(&route_validation::validate_route(&route), 1);

        assert_eq!(conds[0].type_, "Accepted", "the first condition should be Accepted");
        assert_eq!(
            conds[0].status, "False",
            "a route whose every rule is unsupported must not be accepted"
        );
        assert_eq!(
            conds[0].reason, "UnsupportedValue",
            "the Gateway API reason for an unrepresentable value is UnsupportedValue"
        );
    }

    #[test]
    fn test_validation_conditions_marks_a_partially_invalid_route() {
        let route = regex_route(2);
        let conds = validation_conditions(&route_validation::validate_route(&route), 1);

        assert_eq!(conds.len(), 2, "a partially invalid route carries a second condition");
        assert_eq!(conds[0].status, "True", "surviving rules keep the route accepted");
        assert_eq!(
            conds[1].type_, "PartiallyInvalid",
            "dropped rules must be signalled with PartiallyInvalid"
        );
        assert_eq!(conds[1].status, "True", "PartiallyInvalid should be True");
    }

    // -----------------------------------------------------------------------
    // Status Writing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_an_attached_route_is_reported_accepted() {
        let (client, journal) = testing::fake_client(vec![route_response()]);
        let route = attached_route();
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        update_route_parent_statuses(&client, &gateway(), &attached, &[])
            .await
            .expect("the patch is answered");

        let parent = written_parent(&journal).expect("an attached route should get a status entry");
        assert_eq!(
            parent.pointer("/parentRef/name").and_then(Value::as_str),
            Some("gw"),
            "the entry has to name the parent it reports on, or it belongs to nobody"
        );
        assert_eq!(
            parent.pointer("/conditions/0/status").and_then(Value::as_str),
            Some("True"),
            "the Gateway controller only calls this once the data plane has caught up"
        );
    }

    #[tokio::test]
    async fn test_a_ref_naming_another_gateway_is_skipped() {
        let (client, journal) = testing::fake_client(vec![route_response()]);
        let mut route = attached_route();
        route.spec.parent_refs = Some(vec![HttpRouteParentRefs {
            name: "other-gw".to_owned(),
            ..Default::default()
        }]);
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        update_route_parent_statuses(&client, &gateway(), &attached, &[])
            .await
            .expect("skipping is not a failure");

        assert!(
            journal.matching("/status").is_empty(),
            "this Gateway has no business reporting on a ref that names a different one"
        );
    }

    #[tokio::test]
    async fn test_a_route_with_no_parent_refs_is_skipped() {
        let (client, journal) = testing::fake_client(vec![route_response()]);
        let mut route = attached_route();
        route.spec.parent_refs = None;
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        update_route_parent_statuses(&client, &gateway(), &attached, &[])
            .await
            .expect("skipping is not a failure");

        assert!(
            journal.matching("/status").is_empty(),
            "there is no parent to report on"
        );
    }

    #[tokio::test]
    async fn test_a_route_this_operator_cannot_express_is_reported_unaccepted() {
        let (client, journal) = testing::fake_client(vec![route_response()]);
        let mut route = attached_route();
        route.spec.rules = regex_route(1).spec.rules;
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        update_route_parent_statuses(&client, &gateway(), &attached, &[])
            .await
            .expect("the patch is answered");

        let parent = written_parent(&journal).expect("a rejected route still gets a status entry");
        assert_eq!(
            parent.pointer("/conditions/0/reason").and_then(Value::as_str),
            Some("UnsupportedValue"),
            "silently dropping a rule the operator cannot express would send traffic the author \
             never asked for, so the refusal has to be on the route"
        );
    }

    #[tokio::test]
    async fn test_a_refused_patch_fails_the_update() {
        let (client, _) = testing::failing_client();
        let route = attached_route();
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        let error = update_route_parent_statuses(&client, &gateway(), &attached, &[])
            .await
            .expect_err("a 500 is not a written status");

        assert!(
            matches!(error, crate::error::OperatorError::Kube(_)),
            "the Gateway reconcile has to retry, or the route stays unaccepted with nothing \
             scheduled to fix it: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Returns the first parent entry of the status that was written.
    fn written_parent(journal: &testing::Journal) -> Option<Value> {
        journal
            .matching("/status")
            .pop()?
            .body?
            .pointer("/status/parents/0")
            .cloned()
    }

    /// Builds the Gateway the route attaches to.
    fn gateway() -> Gateway {
        use gateway_api::gateways::GatewaySpec;

        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("apps".to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners: vec![],
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds a route naming that Gateway as its parent.
    fn attached_route() -> HTTPRoute {
        use gateway_api::httproutes::HttpRouteSpec;

        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some("apps".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "gw".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        }
    }

    /// The object the API server hands back from a status apply.
    fn route_response() -> testing::Canned {
        testing::Canned::ok(
            "/httproutes/route",
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": { "name": "route", "namespace": "apps" },
                "spec": {},
            }),
        )
    }
}
