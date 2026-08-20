// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway and listener status reporting.
//!
//! Assembles `status.listeners` and the Gateway's own `Accepted` and
//! `Programmed` conditions. `Programmed` is gated on both a ready
//! Deployment and an assigned load-balancer address, because a Gateway
//! with no address is not reachable however healthy the pods are.

use gateway_api::gateways::{Gateway, GatewayListeners};
use k8s_openapi::{
    api::{apps::v1::Deployment, core::v1::Service},
    apimachinery::pkg::apis::meta::v1::Condition,
};
use kube::{
    Api, ResourceExt as _,
    api::{Patch, PatchParams},
};
use serde_json::Value;
use tracing::{debug, info};

use super::listener_validation;
use crate::{
    context::{Context, FIELD_MANAGER},
    error::Result,
    gateway_api::{
        attachment::AttachedRoute,
        conditions, hostname, listener_conflict,
        protocol::ListenerProtocol,
        status,
        status_types::{self, GatewayAddress, GatewayStatus, ListenerStatus},
    },
    observability::metrics,
    resources::labels::child_name,
};

// -----------------------------------------------------------------------------
// Gateway Status
// -----------------------------------------------------------------------------

/// Builds and applies the Gateway status (listener statuses + conditions).
///
/// Gates the `Programmed` condition on both Deployment readiness and
/// load-balancer address availability, per the Gateway API spec.
pub(super) async fn build_and_apply_gateway_status(
    ctx: &Context,
    gw: &Gateway,
    listeners: &[GatewayListeners],
    attached: &[AttachedRoute<'_>],
) -> Result<()> {
    let client = &ctx.client;
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    let generation = gw.metadata.generation.unwrap_or(1);
    let child = child_name(&name);

    let addresses = resolve_lb_addresses(client, &ns, &child).await;
    let deployment_ready = is_deployment_ready(client, &ns, &child).await;
    let (listener_statuses, any_accepted, any_rejected) =
        build_listener_statuses(listeners, generation, &ns, ctx, attached).await;

    let data_plane_ready = deployment_ready && !addresses.is_empty();
    let status = serde_json::to_value(GatewayStatus {
        conditions: vec![
            gateway_accepted_condition(generation, any_accepted, any_rejected),
            gateway_programmed_condition(generation, any_accepted, data_plane_ready),
        ],
        addresses,
        listeners: listener_statuses,
    })?;

    apply_gateway_status(client, gw, &status).await?;
    info!("Gateway {ns}/{name} reconciled successfully");
    Ok(())
}

/// Patches the Gateway status via server-side apply.
///
/// Carries condition transition times forward and returns without
/// contacting the API server when the computed status already matches
/// the live object. Writing an unchanged status re-triggers the
/// controller's own watch, which would keep an idle Gateway reconciling
/// forever.
pub(super) async fn apply_gateway_status(client: &kube::Client, gw: &Gateway, status_json: &Value) -> Result<()> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let observed = serde_json::to_value(&gw.status)?;
    let mut desired = status_json.clone();
    status::preserve_condition_times(&mut desired, &observed);

    if status::is_status_unchanged(&desired, &observed) {
        metrics::global().record_status_skipped();
        debug!("Gateway {ns}/{name} status unchanged, skipping patch");
        return Ok(());
    }
    metrics::global().record_status_written();

    let payload = status_types::status_patch("Gateway", &name, Some(&ns), desired);

    Api::<Gateway>::namespaced(client.clone(), &ns)
        .patch_status(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&payload),
        )
        .await?;
    Ok(())
}

/// Queries the child Service for load-balancer ingress IP addresses.
async fn resolve_lb_addresses(client: &kube::Client, ns: &str, child: &str) -> Vec<GatewayAddress> {
    Api::<Service>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|svc| svc.status)
        .and_then(|svc_status| svc_status.load_balancer)
        .and_then(|lb| lb.ingress)
        .map(|ingress| {
            ingress
                .iter()
                .filter_map(|i| i.ip.as_deref().map(GatewayAddress::ip))
                .collect()
        })
        .unwrap_or_default()
}

/// Checks whether the child Deployment has at least one ready replica.
///
/// Used for the Gateway `Programmed` condition, which reflects whether
/// the data plane can serve traffic at all (even with a stale config).
async fn is_deployment_ready(client: &kube::Client, ns: &str, child: &str) -> bool {
    Api::<Deployment>::namespaced(client.clone(), ns)
        .get(child)
        .await
        .ok()
        .and_then(|deploy| deploy.status)
        .is_some_and(|deploy_status| deploy_status.ready_replicas.unwrap_or(0) > 0)
}

/// Builds per-listener status entries.
///
/// Returns `(statuses, any_accepted, any_rejected)`.
async fn build_listener_statuses(
    listeners: &[GatewayListeners],
    generation: i64,
    gateway_ns: &str,
    ctx: &Context,
    attached: &[AttachedRoute<'_>],
) -> (Vec<ListenerStatus>, bool, bool) {
    let conflicts = listener_conflict::detect_conflicts(listeners);
    let mut statuses = Vec::new();
    let mut any_accepted = false;
    let mut any_rejected = false;

    for listener in listeners {
        if let Some(reason) = conflicts.get(&listener.name) {
            any_rejected = true;
            statuses.push(conflicted_listener_status(listener, generation, *reason));
            continue;
        }

        let protocol_supported = ListenerProtocol::is_supported(&listener.protocol);
        if !protocol_supported {
            any_rejected = true;
            statuses.push(unsupported_listener_status(listener, generation));
            continue;
        }

        any_accepted = true;
        let count = count_attached_routes(attached, listener);
        let status = accepted_listener_status(listener, generation, gateway_ns, ctx, count).await;
        statuses.push(status);
    }

    (statuses, any_accepted, any_rejected)
}

/// Builds a status entry for a listener conflicting with another.
///
/// A conflicted listener is not accepted, not programmed, and attaches
/// no routes: it never reaches the data plane, so claiming otherwise
/// would misreport what is serving traffic.
fn conflicted_listener_status(
    listener: &GatewayListeners,
    generation: i64,
    reason: listener_conflict::ConflictReason,
) -> ListenerStatus {
    ListenerStatus {
        name: listener.name.clone(),
        attached_routes: 0,
        supported_kinds: vec![],
        conditions: vec![
            conditions::not_accepted(generation, reason.as_str(), reason.message()),
            conditions::conflicted(generation, reason.as_str(), reason.message()),
            conditions::not_programmed(generation, reason.as_str(), reason.message()),
        ],
    }
}

/// Builds a status entry for an unsupported-protocol listener.
fn unsupported_listener_status(listener: &GatewayListeners, generation: i64) -> ListenerStatus {
    ListenerStatus {
        name: listener.name.clone(),
        attached_routes: 0,
        supported_kinds: vec![],
        conditions: vec![
            conditions::not_accepted(generation, "UnsupportedProtocol", "protocol not supported"),
            conditions::not_programmed(generation, "Invalid", "unsupported protocol"),
        ],
    }
}

/// Counts routes attached to a specific listener.
fn count_attached_routes(attached: &[AttachedRoute<'_>], listener: &GatewayListeners) -> usize {
    attached
        .iter()
        .filter(|attached| {
            if !attached.targets_listener(&listener.name) {
                return false;
            }
            let route_hostnames = attached.route.spec.hostnames.as_deref().unwrap_or(&[]);
            if route_hostnames.is_empty() {
                return true;
            }
            match &listener.hostname {
                None => true,
                Some(lh) => route_hostnames.iter().any(|rh| hostname::hostname_matches(rh, lh)),
            }
        })
        .count()
}

/// Builds a status entry for an accepted listener.
async fn accepted_listener_status(
    listener: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    ctx: &Context,
    count: usize,
) -> ListenerStatus {
    let (supported_kinds, resolved_refs_condition) =
        listener_validation::listener_resolved_refs(listener, generation, gateway_ns, ctx).await;

    let refs_resolved = resolved_refs_condition.status == "True";
    let programmed_condition = if refs_resolved {
        conditions::programmed(generation, "listener programmed")
    } else {
        conditions::not_programmed(generation, "Invalid", "listener has unresolved refs")
    };

    ListenerStatus {
        name: listener.name.clone(),
        attached_routes: count,
        supported_kinds,
        conditions: vec![
            conditions::accepted(generation, "listener accepted"),
            programmed_condition,
            conditions::no_conflicts(generation),
            resolved_refs_condition,
        ],
    }
}

/// Returns the `Accepted` condition for the Gateway.
///
/// `ListenersNotValid` is only a valid reason alongside `Accepted:
/// False`, so a Gateway with a mix of valid and invalid listeners
/// reports `Accepted`/`Accepted` and carries the partial failure in the
/// message; the per-listener conditions describe which ones failed.
fn gateway_accepted_condition(generation: i64, any_accepted: bool, any_rejected: bool) -> Condition {
    if !any_accepted {
        return conditions::not_accepted(
            generation,
            "ListenersNotValid",
            "no listeners have a supported protocol",
        );
    }

    if any_rejected {
        // Accepted, but the reason has to say why it is not a clean
        // acceptance. The Gateway API reserves `ListenersNotValid` for
        // exactly this state — the Gateway stands, some listeners do
        // not — and conformance asserts on the reason, not just the
        // status. Reporting `Accepted` here loses the only signal that
        // distinguishes a fully valid Gateway from a partly broken one.
        return conditions::make_condition(
            "Accepted",
            "True",
            "ListenersNotValid",
            "Gateway accepted, but some listeners are invalid",
            generation,
        );
    }

    conditions::accepted(generation, "Gateway accepted")
}

/// Returns the `Programmed` condition for the Gateway.
///
/// Requires accepted listeners, a ready Deployment, and at least one
/// load-balancer address before reporting `True`.
fn gateway_programmed_condition(generation: i64, any_accepted: bool, data_plane_ready: bool) -> Condition {
    if !any_accepted {
        return conditions::not_programmed(generation, "Invalid", "no valid listeners");
    }
    if !data_plane_ready {
        return conditions::not_programmed(generation, "Pending", "data plane not ready");
    }
    conditions::programmed(generation, "Data plane ready")
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        controller::fixtures::{https_listener, listener, route_with_hostnames},
        testing,
    };

    #[test]
    fn test_gateway_programmed_all_ready() {
        let cond = gateway_programmed_condition(1, true, true);
        assert_eq!(cond.type_, "Programmed", "type should be Programmed");
        assert_eq!(cond.status, "True", "should be True when all ready");
        assert_eq!(cond.reason, "Programmed", "reason should be Programmed");
        assert_eq!(cond.observed_generation, Some(1), "generation should match");
    }

    #[test]
    fn test_gateway_programmed_no_accepted_listeners() {
        let cond = gateway_programmed_condition(2, false, false);
        assert_eq!(cond.status, "False", "should be False without accepted listeners");
        assert_eq!(cond.reason, "Invalid", "reason should be Invalid");
    }

    #[test]
    fn test_gateway_programmed_deployment_not_ready() {
        let cond = gateway_programmed_condition(3, true, false);
        assert_eq!(cond.status, "False", "should be False when data plane not ready");
        assert_eq!(cond.reason, "Pending", "reason should be Pending");
    }

    #[test]
    fn test_gateway_programmed_invalid_takes_precedence() {
        let cond = gateway_programmed_condition(4, false, true);
        assert_eq!(cond.status, "False", "should be False without accepted listeners");
        assert_eq!(
            cond.reason, "Invalid",
            "Invalid should take precedence over data plane readiness"
        );
    }

    #[test]
    fn test_gateway_accepted_all_valid() {
        let cond = gateway_accepted_condition(1, true, false);
        assert_eq!(cond.type_, "Accepted", "type should be Accepted");
        assert_eq!(cond.status, "True", "should be True when all accepted");
        assert_eq!(cond.reason, "Accepted", "reason should be Accepted");
    }

    #[test]
    fn test_gateway_accepted_none_valid() {
        let cond = gateway_accepted_condition(1, false, true);
        assert_eq!(cond.status, "False", "should be False with no accepted listeners");
    }

    #[test]
    fn test_gateway_accepted_mixed_listeners() {
        let cond = gateway_accepted_condition(1, true, true);
        assert_eq!(cond.status, "True", "should be True when some listeners are accepted");
        assert_eq!(
            cond.reason, "ListenersNotValid",
            "this assertion used to require `Accepted`, on the belief that ListenersNotValid was \
             a False-only reason. It is not: the GatewayListenerUnsupportedProtocol conformance \
             case reports `Accepted condition Reason set to Accepted, expected ListenersNotValid` \
             for a Gateway whose listeners are partly valid. Status stays True — the Gateway is \
             accepted — while the reason carries the fact that some listeners are not"
        );
        assert!(
            cond.message.contains("some listeners are invalid"),
            "the partial failure belongs in the message: {}",
            cond.message
        );
    }

    #[test]
    fn test_count_attached_routes_matches_hostname() {
        let listener = https_listener("https", 443, "cert");
        let route = route_with_hostnames(&["a.example.com"]);
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            0,
            "a route whose hostname misses the listener must not be counted"
        );
    }

    #[test]
    fn test_count_attached_routes_counts_unconstrained_routes() {
        let listener = listener("http", 80, "HTTP");
        let route = route_with_hostnames(&[]);
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            1,
            "a route without hostnames attaches to any listener"
        );
    }

    #[test]
    fn test_count_attached_routes_respects_section_name() {
        let listener = listener("http", 80, "HTTP");
        let route = route_with_hostnames(&[]);
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![Some("https".to_owned())],
        }];

        assert_eq!(
            count_attached_routes(&attached, &listener),
            0,
            "a route bound to another section is not attached here"
        );
    }

    // -----------------------------------------------------------------------
    // Status Assembly
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_ready_data_plane_reports_programmed_with_its_address() {
        let (ctx, journal) = testing::fake_context(
            vec![service_with_address(), ready_deployment(), gateway_response()],
            testing::Cached::default(),
        );

        build_and_apply_gateway_status(&ctx, &gateway(), &[http_listener()], &[])
            .await
            .expect("a reachable API server accepts the patch");

        let status = patched_status(&journal);
        assert_eq!(
            status.pointer("/addresses/0/value").and_then(Value::as_str),
            Some("10.0.0.1"),
            "the load-balancer address is how a client finds the Gateway, so it belongs in status"
        );
        assert_eq!(
            condition_status(&status, "Programmed"),
            Some("True"),
            "a Deployment with a ready replica and an address is a programmed data plane"
        );
    }

    #[tokio::test]
    async fn test_an_address_without_ready_pods_is_not_programmed() {
        let (ctx, journal) = testing::fake_context(
            vec![service_with_address(), gateway_response()],
            testing::Cached::default(),
        );

        build_and_apply_gateway_status(&ctx, &gateway(), &[http_listener()], &[])
            .await
            .expect("a reachable API server accepts the patch");

        assert_eq!(
            condition_status(&patched_status(&journal), "Programmed"),
            Some("False"),
            "an address in front of no running pods answers nothing, and reporting Programmed \
             would tell conformance to start sending traffic"
        );
    }

    #[tokio::test]
    async fn test_a_ready_deployment_without_an_address_is_not_programmed() {
        let (ctx, journal) =
            testing::fake_context(vec![ready_deployment(), gateway_response()], testing::Cached::default());

        build_and_apply_gateway_status(&ctx, &gateway(), &[http_listener()], &[])
            .await
            .expect("a reachable API server accepts the patch");

        let status = patched_status(&journal);
        assert_eq!(
            status.pointer("/addresses").and_then(Value::as_array).map(Vec::len),
            Some(0),
            "a Service with no ingress yet yields no addresses, not a missing field"
        );
        assert_eq!(
            condition_status(&status, "Programmed"),
            Some("False"),
            "pods with no address in front of them are unreachable"
        );
    }

    #[tokio::test]
    async fn test_an_unsupported_protocol_listener_is_rejected_in_status() {
        let (ctx, journal) = testing::fake_context(vec![gateway_response()], testing::Cached::default());
        let mut listener = http_listener();
        listener.protocol = "TCP".to_owned();

        build_and_apply_gateway_status(&ctx, &gateway(), &[listener], &[])
            .await
            .expect("a reachable API server accepts the patch");

        let status = patched_status(&journal);
        assert_eq!(
            status
                .pointer("/listeners/0/conditions/0/reason")
                .and_then(Value::as_str),
            Some("UnsupportedProtocol"),
            "the listener has to say why it is not accepted"
        );
        assert_eq!(
            condition_status(&status, "Accepted"),
            Some("False"),
            "a Gateway whose only listener was rejected accepts nothing"
        );
    }

    #[tokio::test]
    async fn test_a_conflicting_pair_of_listeners_is_reported_on_both() {
        let (ctx, journal) = testing::fake_context(vec![gateway_response()], testing::Cached::default());
        let mut second = http_listener();
        second.name = "http-2".to_owned();
        second.protocol = "HTTPS".to_owned();

        build_and_apply_gateway_status(&ctx, &gateway(), &[http_listener(), second], &[])
            .await
            .expect("a reachable API server accepts the patch");

        let status = patched_status(&journal);
        let conflicted = status
            .pointer("/listeners")
            .and_then(Value::as_array)
            .map(|listeners| {
                listeners
                    .iter()
                    .filter(|l| {
                        l.pointer("/conditions")
                            .and_then(Value::as_array)
                            .is_some_and(|c| c.iter().any(|c| c["type"] == "Conflicted" && c["status"] == "True"))
                    })
                    .count()
            })
            .unwrap_or_default();

        assert_eq!(
            conflicted, 2,
            "two protocols on one port is a conflict both listeners are party to, and reporting \
             it on one leaves the other looking healthy"
        );
    }

    #[tokio::test]
    async fn test_an_unchanged_status_is_not_rewritten() {
        let (ctx, journal) = testing::fake_context(vec![gateway_response()], testing::Cached::default());
        let mut gw = gateway();

        build_and_apply_gateway_status(&ctx, &gw, &[http_listener()], &[])
            .await
            .expect("the first pass writes");
        let first = patched_status(&journal);
        gw.status = serde_json::from_value(first).expect("the computed status is a Gateway status");

        let (ctx, second_journal) = testing::fake_context(vec![gateway_response()], testing::Cached::default());
        build_and_apply_gateway_status(&ctx, &gw, &[http_listener()], &[])
            .await
            .expect("an unchanged status is not a failure");

        assert!(
            second_journal.matching("/status").is_empty(),
            "patching an unchanged status wakes this controller's own watch, and an idle Gateway \
             would reconcile forever"
        );
    }

    #[tokio::test]
    async fn test_a_rejected_status_patch_surfaces() {
        let (client, _) = testing::failing_client();

        let error = apply_gateway_status(&client, &gateway(), &serde_json::json!({ "conditions": [] }))
            .await
            .expect_err("a 500 is not success");

        assert!(
            matches!(error, crate::error::OperatorError::Kube(_)),
            "a status the API server refused has to be retried, not reported as written: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Returns the `status` sub-object of the patch that was sent.
    fn patched_status(journal: &testing::Journal) -> Value {
        journal
            .matching("/status")
            .pop()
            .and_then(|request| request.body)
            .and_then(|body| body.get("status").cloned())
            .expect("a status patch should have been sent")
    }

    /// Returns the status of the named Gateway-level condition.
    fn condition_status<'status>(status: &'status Value, kind: &str) -> Option<&'status str> {
        status
            .pointer("/conditions")?
            .as_array()?
            .iter()
            .find(|cond| cond["type"] == kind)?
            .get("status")?
            .as_str()
    }

    /// Builds the Gateway these tests report on.
    fn gateway() -> Gateway {
        use gateway_api::gateways::GatewaySpec;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

        Gateway {
            metadata: ObjectMeta {
                name: Some("gw".to_owned()),
                namespace: Some("infra".to_owned()),
                generation: Some(1),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: "praxis".to_owned(),
                listeners: vec![http_listener()],
                ..Default::default()
            },
            status: None,
        }
    }

    /// Builds a plain HTTP listener on port 80.
    fn http_listener() -> GatewayListeners {
        GatewayListeners {
            name: "http".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            ..Default::default()
        }
    }

    /// The child Service, carrying a load-balancer address.
    fn service_with_address() -> testing::Canned {
        testing::Canned::ok(
            "/services/praxis-gw",
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": { "name": "praxis-gw", "namespace": "infra" },
                "status": { "loadBalancer": { "ingress": [{ "ip": "10.0.0.1" }] } },
            }),
        )
    }

    /// The child Deployment, with a ready replica.
    fn ready_deployment() -> testing::Canned {
        testing::Canned::ok(
            "/deployments/praxis-gw",
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": { "name": "praxis-gw", "namespace": "infra" },
                "spec": { "selector": { "matchLabels": {} }, "template": {} },
                "status": { "readyReplicas": 1 },
            }),
        )
    }

    /// The object the API server hands back from a status apply.
    fn gateway_response() -> testing::Canned {
        testing::Canned::ok(
            "/gateways/gw",
            serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "Gateway",
                "metadata": { "name": "gw", "namespace": "infra" },
                "spec": { "gatewayClassName": "praxis", "listeners": [] },
            }),
        )
    }
}
