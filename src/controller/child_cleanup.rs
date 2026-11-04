// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! Reclaims owned data-plane resources when a Gateway has no usable listeners.

use std::fmt::Debug;

use gateway_api::gateways::Gateway;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Service},
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Resource, ResourceExt as _,
    api::{DeleteParams, Preconditions},
};
use serde::de::DeserializeOwned;

use crate::{
    error::{OperatorError, Result},
    resources::labels::child_name,
};

/// Removes only children controlled by this Gateway, tolerating prior deletion.
pub(super) async fn delete_children(client: &kube::Client, gw: &Gateway) -> Result<()> {
    let namespace = gw.namespace().unwrap_or_default();
    let name = child_name(&gw.name_any());
    let owner_uid = gw.uid().ok_or(OperatorError::MissingObjectKey(".metadata.uid"))?;
    delete_owned::<Service>(client, &namespace, &name, &owner_uid).await?;
    Box::pin(delete_owned::<Deployment>(client, &namespace, &name, &owner_uid)).await?;
    delete_owned::<ConfigMap>(client, &namespace, &name, &owner_uid).await?;
    delete_owned::<PodDisruptionBudget>(client, &namespace, &name, &owner_uid).await
}

/// Checks controller ownership and guards deletion against concurrent replacement.
async fn delete_owned<K>(client: &kube::Client, namespace: &str, name: &str, owner_uid: &str) -> Result<()>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope> + DeserializeOwned + Clone + Debug,
    <K as Resource>::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), namespace);
    let Some(child) = api.get_opt(name).await? else {
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
    match api.delete(name, &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing;

    #[tokio::test]
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
}
