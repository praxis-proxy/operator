// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway reconciler with finalizer-based lifecycle management.

use std::{fmt::Debug, sync::Arc, time::Duration};

use gateway_api::{gateways::Gateway, httproutes::HTTPRoute, referencegrants::ReferenceGrant};
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{
        controller::Action,
        finalizer::{self, Event},
        reflector::ObjectRef,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use tracing::{debug, error, info};

use super::gateway_helpers;
use crate::{
    context::{Context, GATEWAY_FINALIZER},
    error::{Error, Result},
    gateway_api::conditions,
};

// -----------------------------------------------------------------------------
// Reconciler
// -----------------------------------------------------------------------------

/// Reconciles a [`Gateway`] through its full lifecycle.
///
/// Uses a finalizer to ensure cleanup runs before deletion. On apply,
/// generates Praxis configuration and applies child `Deployment`,
/// `ConfigMap`, and `Service` resources via server-side apply.
pub(crate) async fn reconcile(gw: Arc<Gateway>, ctx: Arc<Context>) -> Result<Action> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    info!("reconciling Gateway {ns}/{name}");

    let api = Api::<Gateway>::namespaced(ctx.client.clone(), &ns);
    finalizer::finalizer(&api, GATEWAY_FINALIZER, gw, |event| {
        Box::pin(async {
            match event {
                Event::Apply(gw) => Box::pin(apply(gw, &ctx)).await,
                Event::Cleanup(gw) => {
                    cleanup(&gw);
                    Ok(Action::await_change())
                },
            }
        })
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// Error policy for Gateway reconciliation failures.
///
/// Uses differentiated backoff: shorter for transient API errors,
/// longer for configuration or logic errors.
pub(crate) fn error_policy(_gw: Arc<Gateway>, error: &Error, _ctx: Arc<Context>) -> Action {
    let delay = match error {
        Error::Kube(_) | Error::Finalizer(_) => Duration::from_secs(15),
        _ => Duration::from_secs(30),
    };
    error!(
        %error, "Gateway reconciliation failed, retrying in {delay:?}"
    );
    Action::requeue(delay)
}

// -----------------------------------------------------------------------------
// Apply
// -----------------------------------------------------------------------------

/// Full apply path: validate, generate config, apply child resources,
/// update Gateway and route statuses.
///
/// Route parent statuses are set **after** child resources are applied
/// and the Deployment rollout is verified, preventing the conformance
/// test from sending traffic before the data plane has the latest
/// configuration.
async fn apply(gw: Arc<Gateway>, ctx: &Context) -> Result<Action> {
    if !gateway_helpers::validate_gateway_class(&ctx.client, &gw).await?
        || reject_if_parameters_ref(&ctx.client, &gw).await?
    {
        return Ok(Action::await_change());
    }

    let routes = list_all_routes(&ctx.client).await?;
    let attached = gateway_helpers::collect_routes(&ctx.client, &gw, &routes).await;
    let ns = gw.namespace().unwrap_or_default();
    let config_changed = apply_config_if_supported(&ctx.client, &gw, &attached, &ns).await?;

    gateway_helpers::build_and_apply_gateway_status(&ctx.client, &gw, &gw.spec.listeners, &attached).await?;

    let can_accept = check_route_acceptance(&ctx.client, &gw, &attached, &ns, config_changed).await?;
    let requeue_secs = if can_accept { 15 } else { 2 };
    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
}

/// Generates and applies Praxis config for supported listeners.
///
/// Returns `true` when the config hash changed (new Deployment rollout
/// was triggered).
async fn apply_config_if_supported(
    client: &kube::Client,
    gw: &Gateway,
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    ns: &str,
) -> Result<bool> {
    let has_supported = gw
        .spec
        .listeners
        .iter()
        .any(|l| l.protocol == "HTTP" || l.protocol == "HTTPS");
    if !has_supported {
        return Ok(false);
    }

    let child = crate::resources::labels::child_name(&gw.name_any());
    let prev_hash = gateway_helpers::current_deployment_hash(client, ns, &child).await;
    let config = gateway_helpers::build_praxis_config(client, &gw.spec.listeners, attached, ns).await?;
    let new_hash = Box::pin(gateway_helpers::apply_child_resources(client, gw, &config)).await?;
    let changed = prev_hash.as_deref() != Some(&new_hash);
    log_config_apply(&gw.name_any(), changed, attached.len());
    Ok(changed)
}

/// Accepts routes when the config is stable and the Deployment is
/// fully rolled out.
async fn check_route_acceptance(
    client: &kube::Client,
    gw: &Gateway,
    attached: &[(&HTTPRoute, Vec<Option<String>>)],
    ns: &str,
    config_changed: bool,
) -> Result<bool> {
    let child = crate::resources::labels::child_name(&gw.name_any());
    let rolled_out = gateway_helpers::is_deployment_rolled_out(client, ns, &child).await;
    let can_accept = !config_changed && rolled_out;
    log_acceptance_decision(&gw.name_any(), can_accept);
    if can_accept {
        gateway_helpers::update_route_parent_statuses(client, gw, attached).await?;
    }
    Ok(can_accept)
}

/// Logs the result of a config apply cycle.
fn log_config_apply(gateway: &str, config_changed: bool, routes: usize) {
    debug!(gateway, config_changed, routes, "config apply result");
}

/// Logs the route acceptance decision.
fn log_acceptance_decision(gateway: &str, can_accept: bool) {
    debug!(gateway, can_accept, "route acceptance decision");
}

/// Lists all `HTTPRoute` resources across all namespaces.
async fn list_all_routes(client: &kube::Client) -> Result<Vec<HTTPRoute>> {
    let api = Api::<HTTPRoute>::all(client.clone());
    let list = api.list(&kube::api::ListParams::default()).await?;
    Ok(list.items)
}

/// Lists all `ReferenceGrant` resources across all namespaces.
pub(super) async fn list_all_grants(client: &kube::Client) -> Result<Vec<ReferenceGrant>> {
    let api = Api::<ReferenceGrant>::all(client.clone());
    let list = api.list(&kube::api::ListParams::default()).await?;
    Ok(list.items)
}

/// Rejects the Gateway if it has a `parametersRef`. Returns `true` when
/// rejected.
async fn reject_if_parameters_ref(client: &kube::Client, gw: &Gateway) -> Result<bool> {
    if !has_parameters_ref(gw) {
        return Ok(false);
    }
    let generation = gw.metadata.generation.unwrap_or(1);
    reject_gateway(
        client,
        gw,
        generation,
        "InvalidParameters",
        "parametersRef is not supported",
    )
    .await?;
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();
    info!("Gateway {ns}/{name} rejected: unsupported parametersRef");
    Ok(true)
}

// -----------------------------------------------------------------------------
// Cleanup
// -----------------------------------------------------------------------------

/// Cleanup path: owner references handle child deletion automatically.
fn cleanup(gw: &Gateway) {
    let name = gw.name_any();
    let ns = gw.namespace().unwrap_or_else(|| {
        tracing::warn!(gateway = %name, "Gateway has no namespace during cleanup");
        String::new()
    });
    log_cleanup(&ns, &name);
}

/// Logs a Gateway cleanup event.
fn log_cleanup(ns: &str, name: &str) {
    info!("cleaning up Gateway {ns}/{name} (owner refs handle child deletion)");
}

// -----------------------------------------------------------------------------
// Server-side Apply
// -----------------------------------------------------------------------------

/// Applies a namespaced Kubernetes resource via server-side apply.
pub(super) async fn apply_resource<K>(client: &kube::Client, ns: &str, resource: &K) -> Result<()>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + Send
        + Sync,
    <K as Resource>::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), ns);
    let name = resource
        .meta()
        .name
        .as_deref()
        .ok_or(Error::MissingObjectKey(".metadata.name"))?;
    api.patch(
        name,
        &PatchParams::apply("praxis-operator").force(),
        &Patch::Apply(resource),
    )
    .await?;
    debug!("applied {name}");
    Ok(())
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Checks whether a `Gateway` has `spec.infrastructure.parametersRef` set.
fn has_parameters_ref(gw: &Gateway) -> bool {
    gw.spec
        .infrastructure
        .as_ref()
        .is_some_and(|infra| infra.parameters_ref.is_some())
}

/// Rejects a Gateway by setting `Accepted: False` with the given reason.
async fn reject_gateway(
    client: &kube::Client,
    gw: &Gateway,
    generation: i64,
    reason: &str,
    message: &str,
) -> Result<()> {
    let ns = gw.namespace().unwrap_or_default();
    let name = gw.name_any();

    let status = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "Gateway",
        "metadata": { "name": name, "namespace": ns },
        "status": {
            "conditions": [
                conditions::not_accepted(generation, reason, message),
                conditions::not_programmed(
                    generation, "Invalid", message,
                ),
            ],
        },
    });

    let gw_api = Api::<Gateway>::namespaced(client.clone(), &ns);
    gw_api
        .patch_status(
            &name,
            &PatchParams::apply("praxis-operator").force(),
            &Patch::Apply(&status),
        )
        .await?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Watch Mappers
// -----------------------------------------------------------------------------

/// Extracts a [`Gateway`] [`ObjectRef`] from an [`HTTPRoute`]'s parent refs.
///
/// Finds the first parentRef targeting a `Gateway` and returns an
/// [`ObjectRef`] pointing to it. Used by [`Controller::watches`] to trigger
/// Gateway reconciliation on route changes.
///
/// [`Controller::watches`]: kube::runtime::controller::Controller::watches
pub(crate) fn map_route_to_gateway(route: &HTTPRoute) -> Option<ObjectRef<Gateway>> {
    let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");
    let parent_refs = route.spec.parent_refs.as_deref()?;
    find_gateway_parent_ref(parent_refs, route_ns)
}

/// Maps a [`ReferenceGrant`] change to affected Gateways.
///
/// Returns an empty vec because the mapper lacks API access to find
/// specific Gateways. The Gateway controller's 15-second requeue
/// ensures convergence after grant changes.
///
/// The watch itself ensures the controller is notified of grant
/// creation/deletion events, which resets the requeue timer.
pub(crate) fn map_grant_to_gateways(_grant: &ReferenceGrant) -> Vec<ObjectRef<Gateway>> {
    Vec::new()
}

/// Finds the first Gateway parent ref and returns an [`ObjectRef`] for it.
fn find_gateway_parent_ref(
    parent_refs: &[gateway_api::httproutes::HttpRouteParentRefs],
    route_ns: &str,
) -> Option<ObjectRef<Gateway>> {
    for parent in parent_refs {
        let group = parent.group.as_deref().unwrap_or("gateway.networking.k8s.io");
        let kind = parent.kind.as_deref().unwrap_or("Gateway");
        if group == "gateway.networking.k8s.io" && kind == "Gateway" {
            let gw_ns = parent.namespace.as_deref().unwrap_or(route_ns);
            return Some(ObjectRef::new(&parent.name).within(gw_ns));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
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
    use gateway_api::httproutes::{HTTPRoute, HttpRouteParentRefs, HttpRouteSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_map_route_to_gateway_basic() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-gateway".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let result = map_route_to_gateway(&route);
        assert!(result.is_some(), "should map route to gateway");

        let obj_ref = result.unwrap();
        assert_eq!(obj_ref.name, "my-gateway", "gateway name should match");
    }

    #[test]
    fn test_map_route_to_gateway_no_parent_refs() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: None,
                ..Default::default()
            },
            status: None,
        };

        assert!(
            map_route_to_gateway(&route).is_none(),
            "should return None with no parent refs"
        );
    }

    #[test]
    fn test_map_route_to_gateway_non_gateway_parent() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-svc".to_owned(),
                    kind: Some("Service".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        assert!(
            map_route_to_gateway(&route).is_none(),
            "should return None for non-Gateway parent"
        );
    }

    #[test]
    fn test_map_route_to_gateway_cross_namespace() {
        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("app-ns".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "my-gateway".to_owned(),
                    namespace: Some("gateway-ns".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let result = map_route_to_gateway(&route);
        assert!(result.is_some(), "should map cross-namespace route");
    }

    #[test]
    fn test_has_parameters_ref_none() {
        let gw = Gateway {
            metadata: ObjectMeta::default(),
            spec: Default::default(),
            status: None,
        };
        assert!(
            !has_parameters_ref(&gw),
            "default gateway should have no parameters ref"
        );
    }
}
