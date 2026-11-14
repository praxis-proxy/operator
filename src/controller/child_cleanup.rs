// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! Reclaims owned data-plane resources when a Gateway has no usable listeners.

use std::fmt::Debug;

use gateway_api::gateways::Gateway;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Service},
    networking::v1::NetworkPolicy,
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Resource, ResourceExt as _,
    api::{DeleteParams, DynamicObject, Preconditions},
};
use serde::de::DeserializeOwned;

use crate::{
    error::{OperatorError, Result},
    resources::{labels::child_name, monitoring::pod_monitor_api_resource},
};

/// Removes only children controlled by this Gateway, tolerating prior deletion.
///
/// Every kind is attempted even when an earlier deletion fails: a
/// transient error on one resource must not strand the rest until a
/// later retry happens to succeed. The first failure is still returned,
/// so reconciliation keeps retrying until every child is gone.
pub(super) async fn delete_children(client: &kube::Client, gw: &Gateway) -> Result<()> {
    let namespace = gw.namespace().unwrap_or_default();
    let name = child_name(&gw.name_any());
    let owner_uid = gw.uid().ok_or(OperatorError::MissingObjectKey(".metadata.uid"))?;
    let data_plane = [
        delete_owned::<Service>(client, &namespace, &name, &owner_uid).await,
        Box::pin(delete_owned::<Deployment>(client, &namespace, &name, &owner_uid)).await,
        delete_owned::<ConfigMap>(client, &namespace, &name, &owner_uid).await,
        delete_owned::<PodDisruptionBudget>(client, &namespace, &name, &owner_uid).await,
    ];
    let monitoring = delete_monitoring_children(client, gw).await;

    first_error(data_plane.into_iter().chain([monitoring]))
}

/// Removes monitoring resources regardless of the current enablement setting.
///
/// Both kinds are attempted even when the first one fails, for the same
/// reason [`delete_children`] does not stop early.
pub(super) async fn delete_monitoring_children(client: &kube::Client, gw: &Gateway) -> Result<()> {
    let namespace = gw.namespace().unwrap_or_default();
    let name = child_name(&gw.name_any());
    let owner_uid = gw.uid().ok_or(OperatorError::MissingObjectKey(".metadata.uid"))?;
    let monitors = Api::<DynamicObject>::namespaced_with(client.clone(), &namespace, &pod_monitor_api_resource());
    let deletions = [
        delete_owned::<NetworkPolicy>(client, &namespace, &name, &owner_uid).await,
        delete_from_api(&monitors, &name, &owner_uid).await,
    ];

    first_error(deletions)
}

/// Returns the first failure from a batch whose entries all ran already.
fn first_error(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    results
        .into_iter()
        .find_map(std::result::Result::err)
        .map_or(Ok(()), Err)
}

/// Checks controller ownership and guards deletion against concurrent replacement.
async fn delete_owned<K>(client: &kube::Client, namespace: &str, name: &str, owner_uid: &str) -> Result<()>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope> + DeserializeOwned + Clone + Debug,
    <K as Resource>::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), namespace);
    delete_from_api(&api, name, owner_uid).await
}

/// Uses the same ownership and concurrency guards for static and dynamic kinds.
/// A missing object or absent optional CRD both return 404 and need no deletion.
async fn delete_from_api<K>(api: &Api<K>, name: &str, owner_uid: &str) -> Result<()>
where
    K: Resource + DeserializeOwned + Clone + Debug,
{
    let Some(child) = ignore_not_found(api.get(name).await)? else {
        return Ok(());
    };
    if !child
        .owner_references()
        .iter()
        .any(|owner| owner.controller == Some(true) && owner.uid == owner_uid)
    {
        return Ok(());
    }
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(child.uid().ok_or(OperatorError::MissingObjectKey(".metadata.uid"))?),
            resource_version: child.resource_version(),
        }),
        ..DeleteParams::default()
    };
    ignore_not_found(api.delete(name, &params).await).map(|_| ())
}

/// Treats any API 404 as absent, including API groups returning plain-text errors.
fn ignore_not_found<T>(response: std::result::Result<T, kube::Error>) -> Result<Option<T>> {
    match response {
        Ok(value) => Ok(Some(value)),
        Err(kube::Error::Api(status)) if status.code == 404 => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing;

    #[tokio::test]
    /// Only Gateway-owned children are deleted, guarded by UID preconditions.
    async fn cleanup_deletes_only_owned_children_with_identity_preconditions() {
        let service = json!({"apiVersion":"v1", "kind":"Service", "metadata": {
            "name":"praxis-gw", "namespace":"infra", "uid":"service-uid", "resourceVersion":"7",
            "ownerReferences":[{"apiVersion":"gateway.networking.k8s.io/v1", "kind":"Gateway",
                "name":"gw", "uid":"gateway-uid", "controller":true}]
        }});
        let (ctx, journal) = testing::fake_context(
            vec![testing::Canned::ok("/services/praxis-gw", service)],
            testing::Cached::default(),
        );
        delete_owned::<Service>(&ctx.client, "infra", "praxis-gw", "gateway-uid")
            .await
            .unwrap();
        let requests: Vec<_> = journal
            .requests()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(requests.len(), 1, "owned resources must be removed");
        assert_eq!(
            requests[0].body.as_ref().unwrap()["preconditions"],
            json!({"uid":"service-uid", "resourceVersion":"7"}),
            "replacement and ownership races must fail deletion"
        );
    }

    #[tokio::test]
    /// Foreign owners and non-controller references are left untouched.
    async fn cleanup_preserves_foreign_and_non_controller_resources() {
        for (owner, controller) in [("someone-else", true), ("gateway-uid", false)] {
            let service = json!({"apiVersion":"v1", "kind":"Service", "metadata": {
                "name":"praxis-gw", "uid":"service-uid", "ownerReferences":[{
                    "apiVersion":"gateway.networking.k8s.io/v1", "kind":"Gateway", "name":"gw",
                    "uid":owner, "controller":controller}]
            }});
            let (ctx, journal) = testing::fake_context(
                vec![testing::Canned::ok("/services/praxis-gw", service)],
                testing::Cached::default(),
            );
            delete_owned::<Service>(&ctx.client, "infra", "praxis-gw", "gateway-uid")
                .await
                .unwrap();
            assert!(
                journal.requests().iter().all(|request| request.method != "DELETE"),
                "a matching resource name is not ownership"
            );
        }
    }

    #[tokio::test]
    /// Missing resources are ignored; other API failures still surface.
    async fn cleanup_tolerates_missing_resources_and_propagates_api_failures() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());
        delete_owned::<Service>(&ctx.client, "infra", "praxis-gw", "gateway-uid")
            .await
            .unwrap();
        let (ctx, _) = testing::fake_context(
            vec![testing::Canned::server_error("/services/praxis-gw")],
            testing::Cached::default(),
        );
        assert!(
            delete_owned::<Service>(&ctx.client, "infra", "praxis-gw", "gateway-uid")
                .await
                .is_err(),
            "cleanup failures must be retried instead of reporting stale resources removed"
        );
    }
    #[tokio::test]
    /// One failing kind does not stop the others from being reclaimed.
    async fn cleanup_attempts_every_kind_after_a_failure() {
        let (ctx, journal) = testing::fake_context(
            vec![testing::Canned::server_error("/services/praxis-gw")],
            testing::Cached::default(),
        );
        let gateway: Gateway = serde_json::from_value(serde_json::json!({"metadata":{"name":"gw","namespace":"infra","uid":"gateway-uid"},"spec":{"gatewayClassName":"praxis","listeners":[]}})).unwrap();

        assert!(
            delete_children(&ctx.client, &gateway).await.is_err(),
            "a failed deletion must still reach error_policy, or the stale child is never retried"
        );
        for kind in [
            "/deployments/praxis-gw",
            "/configmaps/praxis-gw",
            "/poddisruptionbudgets/praxis-gw",
            "/networkpolicies/praxis-gw",
            "/podmonitors/praxis-gw",
        ] {
            assert!(
                journal
                    .requests()
                    .iter()
                    .any(|request| request.method == "GET" && request.uri.contains(kind)),
                "a failure on one kind must not strand the rest until a retry happens to succeed: {kind}"
            );
        }
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "explicit monitoring fixtures cover policy and reconciliation contracts"
    )]
    /// Monitoring cleanup keeps the owner check and tolerates an absent CRD.
    async fn monitoring_cleanup_obeys_ownership_and_tolerates_absent_crd() {
        for owner in ["gateway-uid", "foreign-uid"] {
            let mut monitor = serde_json::json!({"apiVersion":"monitoring.coreos.com/v1", "kind":"PodMonitor", "metadata": {
                "name":"praxis-gw", "namespace":"infra", "uid":"monitor-uid", "resourceVersion":"9",
                "ownerReferences":[{"apiVersion":"gateway.networking.k8s.io/v1", "kind":"Gateway", "name":"gw", "uid":owner, "controller":true}]
            }});
            let mut policy = monitor.clone();
            policy["apiVersion"] = "networking.k8s.io/v1".into();
            policy["kind"] = "NetworkPolicy".into();
            monitor["spec"] = serde_json::json!({});
            let (ctx, journal) = testing::fake_context(
                vec![
                    testing::Canned::ok("/networkpolicies/praxis-gw", policy),
                    testing::Canned::ok("/podmonitors/praxis-gw", monitor),
                ],
                testing::Cached::default(),
            );
            let gateway: Gateway = serde_json::from_value(serde_json::json!({"metadata":{"name":"gw","namespace":"infra","uid":"gateway-uid"},"spec":{"gatewayClassName":"praxis","listeners":[]}})).unwrap();
            delete_monitoring_children(&ctx.client, &gateway).await.unwrap();
            let deletes: Vec<_> = journal
                .requests()
                .into_iter()
                .filter(|request| request.method == "DELETE")
                .collect();
            assert_eq!(
                deletes.len(),
                if owner == "gateway-uid" { 2 } else { 0 },
                "only this Gateway's monitoring can be deleted"
            );
            for request in deletes {
                assert_eq!(
                    request.body.unwrap()["preconditions"]["resourceVersion"],
                    "9",
                    "deletion must guard replacement races"
                );
            }
        }
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());
        let monitors = Api::<DynamicObject>::namespaced_with(ctx.client.clone(), "infra", &pod_monitor_api_resource());
        delete_from_api(&monitors, "praxis-gw", "gateway-uid").await.unwrap();
    }
    #[test]
    /// A CRD-less API group is already deleted, not an error to report.
    fn absent_api_group_does_not_require_a_not_found_reason() {
        let response: kube::core::Status = serde_json::from_value(json!({"status":"Failure", "code":404, "message":"404 page not found\n", "reason":"Failed to parse error data"})).unwrap();
        assert_eq!(
            ignore_not_found::<()>(Err(kube::Error::Api(Box::new(response)))).unwrap(),
            None,
            "absent API groups return unstructured 404 responses"
        );
    }
}
