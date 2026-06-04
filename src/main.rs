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
use futures::StreamExt;
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

// -----------------------------------------------------------------------------
// Entry Point
// -----------------------------------------------------------------------------

/// Entry point: wires and runs `GatewayClass`, `Gateway`, and `HTTPRoute`
/// controllers.
#[tokio::main]
async fn main() -> error::Result<()> {
    let directive = DEFAULT_DIRECTIVE
        .parse()
        .unwrap_or_else(|_| unreachable!("static directive {DEFAULT_DIRECTIVE} must parse"));

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive(directive))
        .json()
        .init();

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
/// `ReferenceGrant` changes.
fn build_gw_controller(client: &Client, ctx: Arc<context::Context>) -> impl Future<Output = ()> {
    Controller::new(Api::<Gateway>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
        .owns(Api::<ConfigMap>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
        .watches(
            Api::<HTTPRoute>::all(client.clone()),
            watcher::Config::default(),
            |route| controller::gateway::map_route_to_gateway(&route),
        )
        .watches(
            Api::<ReferenceGrant>::all(client.clone()),
            watcher::Config::default(),
            |grant| controller::gateway::map_grant_to_gateways(&grant),
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
