// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Praxis Gateway API operator.
//!
//! Reconciles Gateway API resources into Praxis proxy deployments. The
//! crate is a library so its helpers can carry doctests and be
//! benchmarked; `main` is a thin shim over [`run`].

#![deny(unsafe_code)]

pub mod config;
pub mod context;
pub mod controller;
pub mod endpoints;
pub mod error;
pub mod gateway_api;
pub mod leader;
pub mod observability;
pub mod resources;
pub mod stores;

#[cfg(test)]
mod testing;

use std::{future::Future, sync::Arc};

use ::gateway_api::{
    gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute, referencegrants::ReferenceGrant,
};
use futures::StreamExt as _;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Service},
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Client,
    runtime::{controller::Controller, watcher},
};
use tracing::info;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Default tracing directive for the operator crate.
const DEFAULT_DIRECTIVE: &str = "praxis_operator=info";

/// Label selector restricting owned-resource watches to operator-managed
/// objects.
const MANAGED_BY_SELECTOR: &str = "app.kubernetes.io/managed-by=praxis-operator";

// -----------------------------------------------------------------------------
// Entry Point
// -----------------------------------------------------------------------------

/// Entry point: wires and runs `GatewayClass`, `Gateway`, and `HTTPRoute`
/// controllers.
/// Runs the operator until a controller exits or leadership is lost.
///
/// # Errors
///
/// Returns [`OperatorError::LeadershipLost`] when another replica takes
/// the lease, and any error from connecting to the cluster.
///
/// [`OperatorError::LeadershipLost`]: error::OperatorError::LeadershipLost
pub async fn run() -> error::Result<()> {
    tracing_subscriber::fmt().with_env_filter(env_filter()).json().init();

    info!("starting praxis-operator");
    let client = Client::try_default().await?;
    info!("connected to cluster, controller={}", context::CONTROLLER_NAME);

    let health = Arc::new(observability::server::Health::default());
    let observability = tokio::spawn(observability::server::serve(Arc::clone(&health)));

    // Readiness reflects process health, not leadership. A standby is
    // healthy and must report ready, or a rolling update never completes:
    // the Deployment waits for every replica, and a replica that only
    // turns ready on winning the lease can never satisfy it.
    health.mark_ready();

    let identity = leader::identity();
    info!("standing for election as {identity}");
    leader::acquire(&client, &identity).await?;
    observability::metrics::global().set_leader(true);

    let result = Box::pin(run_controllers(&client, &identity)).await;

    observability.abort();
    result
}

/// Runs every controller until one exits or leadership is lost.
///
/// # Errors
///
/// Returns [`OperatorError::LeadershipLost`] when another replica takes
/// the lease, so the process exits non-zero and restarts as a follower.
///
/// [`OperatorError::LeadershipLost`]: error::OperatorError::LeadershipLost
async fn run_controllers(client: &Client, identity: &str) -> error::Result<()> {
    let ctx = Arc::new(context::Context {
        client: client.clone(),
        recorder: kube::runtime::events::Recorder::new(client.clone(), context::reporter()),
        stores: stores::Stores::spawn(client).await?,
    });
    let gc = build_gc_controller(client, Arc::clone(&ctx));
    let gw = build_gw_controller(client, Arc::clone(&ctx));
    let rt = build_route_controller(client, ctx);

    info!("starting controllers");

    tokio::select! {
        () = async { tokio::join!(gc, gw, rt); } => Ok(()),
        outcome = leader::renew_until_lost(client, identity) => outcome,
    }
}

// -----------------------------------------------------------------------------
// Controller Builders
// -----------------------------------------------------------------------------

/// Builds the tracing filter from `RUST_LOG`, adding the crate default.
///
/// A malformed default directive degrades to the environment filter alone
/// rather than aborting startup.
fn env_filter() -> tracing_subscriber::EnvFilter {
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    match DEFAULT_DIRECTIVE.parse() {
        Ok(directive) => filter.add_directive(directive),
        Err(_) => filter,
    }
}

/// Wires the `GatewayClass` controller.
///
/// Watches all `GatewayClass` resources and reconciles their `Accepted`
/// status.
fn build_gc_controller(client: &Client, ctx: Arc<context::Context>) -> impl Future<Output = ()> {
    Controller::new(Api::<GatewayClass>::all(client.clone()), watcher::Config::default())
        .shutdown_on_signal()
        .run(
            controller::gateway_class::reconcile,
            controller::gateway_class::error_policy,
            ctx,
        )
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    observability::metrics::global().record_reconcile(observability::metrics::Controller::GatewayClass);
                    info!("reconciled GatewayClass {obj}");
                },
                Err(e) => {
                    observability::metrics::global().record_error(observability::metrics::Controller::GatewayClass);
                    tracing::warn!("GatewayClass reconcile error: {e:?}");
                },
            }
        })
}

/// Wires the `Gateway` controller with owned-resource watches.
///
/// Watches `Gateway` resources and their owned `Deployment`, `ConfigMap`,
/// and `Service` children, `HTTPRoute` cross-references, and
/// `ReferenceGrant` changes. Child watches carry the managed-by label
/// selector so the operator never deserializes unrelated cluster objects.
fn build_gw_controller(client: &Client, ctx: Arc<context::Context>) -> impl Future<Output = ()> {
    let controller = Controller::new(Api::<Gateway>::all(client.clone()), watcher::Config::default());
    let gateways = controller.store();

    with_gateway_watches(controller, client, gateways)
        .shutdown_on_signal()
        .run(controller::gateway::reconcile, controller::gateway::error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    observability::metrics::global().record_reconcile(observability::metrics::Controller::Gateway);
                    info!("reconciled Gateway {obj}");
                },
                Err(e) => {
                    observability::metrics::global().record_error(observability::metrics::Controller::Gateway);
                    tracing::warn!("Gateway reconcile error: {e:?}");
                },
            }
        })
}

/// Registers the owned children and cross-references a Gateway depends
/// on.
///
/// Child watches carry the managed-by selector so the operator never
/// deserializes unrelated cluster objects.
fn with_gateway_watches(
    controller: Controller<Gateway>,
    client: &Client,
    gateways: kube::runtime::reflector::Store<Gateway>,
) -> Controller<Gateway> {
    controller
        .owns(Api::<Deployment>::all(client.clone()), managed_children())
        .owns(Api::<ConfigMap>::all(client.clone()), managed_children())
        .owns(Api::<Service>::all(client.clone()), managed_children())
        .owns(Api::<PodDisruptionBudget>::all(client.clone()), managed_children())
        .watches(
            Api::<HTTPRoute>::all(client.clone()),
            watcher::Config::default(),
            |route| controller::gateway::map_route_to_gateway(&route),
        )
        .watches(
            Api::<ReferenceGrant>::all(client.clone()),
            watcher::Config::default(),
            move |grant| controller::gateway::map_grant_to_gateways(&grant, &gateways.state()),
        )
}

/// Watcher config scoped to the child resources this operator manages.
fn managed_children() -> watcher::Config {
    watcher::Config::default().labels(MANAGED_BY_SELECTOR)
}

/// Wires the `HTTPRoute` controller.
///
/// Watches all `HTTPRoute` resources and reconciles parent status entries.
fn build_route_controller(client: &Client, ctx: Arc<context::Context>) -> impl Future<Output = ()> {
    Controller::new(Api::<HTTPRoute>::all(client.clone()), watcher::Config::default())
        .shutdown_on_signal()
        .run(
            controller::httproute::reconcile,
            controller::httproute::error_policy,
            ctx,
        )
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    observability::metrics::global().record_reconcile(observability::metrics::Controller::HttpRoute);
                    info!("reconciled HTTPRoute {obj}");
                },
                Err(e) => {
                    observability::metrics::global().record_error(observability::metrics::Controller::HttpRoute);
                    tracing::warn!("HTTPRoute reconcile error: {e:?}");
                },
            }
        })
}
