// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Praxis Gateway API operator.

#![deny(unsafe_code)]

mod config;
mod context;
mod controller;
mod endpoints;
mod error;
mod gateway_api;
mod resources;

use std::{future::Future, sync::Arc};

use ::gateway_api::{
    gatewayclasses::GatewayClass, gateways::Gateway, httproutes::HTTPRoute, referencegrants::ReferenceGrant,
};
use futures::StreamExt as _;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Service},
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
#[tokio::main]
async fn main() -> error::Result<()> {
    tracing_subscriber::fmt().with_env_filter(env_filter()).json().init();

    info!("starting praxis-operator");
    let client = Client::try_default().await?;
    info!("connected to cluster, controller={}", context::CONTROLLER_NAME);

    let ctx = Arc::new(context::Context { client: client.clone() });

    let gc = build_gc_controller(&client, Arc::clone(&ctx));
    let gw = build_gw_controller(&client, Arc::clone(&ctx));
    let rt = build_route_controller(&client, ctx);

    info!("starting controllers");
    tokio::join!(gc, gw, rt);

    Ok(())
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
        .for_each(|res| async {
            match res {
                Ok((obj, _action)) => info!("reconciled GatewayClass {obj}"),
                Err(e) => tracing::warn!("GatewayClass reconcile error: {e:?}"),
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

    controller
        .owns(Api::<Deployment>::all(client.clone()), managed_children())
        .owns(Api::<ConfigMap>::all(client.clone()), managed_children())
        .owns(Api::<Service>::all(client.clone()), managed_children())
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
        .shutdown_on_signal()
        .run(controller::gateway::reconcile, controller::gateway::error_policy, ctx)
        .for_each(|res| async {
            match res {
                Ok((obj, _action)) => info!("reconciled Gateway {obj}"),
                Err(e) => tracing::warn!("Gateway reconcile error: {e:?}"),
            }
        })
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
        .for_each(|res| async {
            match res {
                Ok((obj, _action)) => info!("reconciled HTTPRoute {obj}"),
                Err(e) => tracing::warn!("HTTPRoute reconcile error: {e:?}"),
            }
        })
}
