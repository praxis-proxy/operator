// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Shane Utt

//! Integration tests requiring a Kubernetes cluster with Gateway API CRDs.
//!
//! Run with `cargo test --features integration -- --ignored`.
//! Requires a KIND cluster with `MetalLB`, Gateway API CRDs installed,
//! and the praxis-operator deployed (via `make kind-up`).

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::default_trait_access,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_assert_message,
    clippy::needless_pass_by_value,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    reason = "integration tests"
)]
#![allow(missing_docs, reason = "integration test module")]

use gateway_api::{
    gatewayclasses::{GatewayClass, GatewayClassSpec},
    gateways::{Gateway, GatewayInfrastructure, GatewayInfrastructureParametersRef, GatewayListeners, GatewaySpec},
    httproutes::{
        HTTPRoute, HttpRouteParentRefs, HttpRouteRules, HttpRouteRulesBackendRefs, HttpRouteRulesMatches,
        HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType, HttpRouteSpec,
    },
    referencegrants::{ReferenceGrant, ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo},
};
use k8s_openapi::{
    api::{
        apps::v1::Deployment,
        core::v1::{ConfigMap, Namespace, Pod, Service, ServicePort, ServiceSpec},
    },
    apimachinery::pkg::apis::meta::v1::{Condition, ObjectMeta},
};
use kube::{
    Api, Client, ResourceExt as _,
    api::{DeleteParams, ListParams, PostParams},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CONTROLLER_NAME: &str = "praxis.sh/gateway-controller";
const POLL_INTERVAL_MS: u64 = 500;
const RECONCILE_TIMEOUT_SECS: u64 = 60;
const DATA_PLANE_TIMEOUT_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// GatewayClass Lifecycle Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn gatewayclass_accepted() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gc-accept").await;

    let cond = ctx
        .await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    assert_eq!(cond.reason, "Accepted", "reason should be Accepted");
    assert!(
        cond.observed_generation.unwrap_or(0) >= 1,
        "observedGeneration should be set"
    );

    ctx.cleanup_gateway_class(&gc).await;
}

#[tokio::test]
#[ignore]
async fn gatewayclass_observed_generation_bumps_on_update() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gc-obsgen").await;

    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gc_api: Api<GatewayClass> = Api::all(ctx.client.clone());
    let mut current = gc_api.get(&gc).await.expect("failed to get GatewayClass");
    current.spec.description = Some("updated description".to_owned());
    gc_api
        .replace(&gc, &PostParams::default(), &current)
        .await
        .expect("failed to update GatewayClass");

    let cond = ctx.await_generation_bump(&gc, 1, RECONCILE_TIMEOUT_SECS).await;
    assert_eq!(cond.status, "True", "should still be Accepted after update");

    ctx.cleanup_gateway_class(&gc).await;
}

#[tokio::test]
#[ignore]
async fn gatewayclass_wrong_controller_ignored() {
    let ctx = TestContext::new().await;
    let name = unique_name("gc-foreign");

    let gc_api: Api<GatewayClass> = Api::all(ctx.client.clone());
    let gc = GatewayClass {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            ..Default::default()
        },
        spec: GatewayClassSpec {
            controller_name: "other.io/controller".to_owned(),
            ..Default::default()
        },
        status: None,
    };
    gc_api
        .create(&PostParams::default(), &gc)
        .await
        .expect("failed to create foreign GatewayClass");

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let fetched = gc_api.get(&name).await.expect("failed to get foreign GatewayClass");
    let has_our_accepted = fetched
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Accepted" && c.status == "True" && c.reason == "Accepted")
        });
    assert!(
        !has_our_accepted,
        "operator should not accept GatewayClass with foreign controller"
    );

    gc_api
        .delete(&name, &DeleteParams::default())
        .await
        .expect("failed to delete foreign GatewayClass");
}

// ---------------------------------------------------------------------------
// Gateway Lifecycle Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn gateway_accepted_and_programmed() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-basic-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-basic").await;
    let gw = ctx.create_gateway("gw-basic", &ns, &gc, 8080).await;

    let accepted = ctx
        .await_gateway_condition(&gw, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(accepted.reason, "Accepted", "Gateway Accepted reason");

    let programmed = ctx
        .await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(programmed.reason, "Programmed", "Gateway Programmed reason");

    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_gets_loadbalancer_address() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-addr-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-addr").await;
    let gw = ctx.create_gateway("gw-addr", &ns, &gc, 8081).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);
    let addr = await_gateway_address(&gw_api, &gw, DATA_PLANE_TIMEOUT_SECS).await;
    assert!(!addr.is_empty(), "Gateway should have at least one address");

    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_multiple_listeners() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-multi-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-multi").await;
    let gw_name = unique_name("gw-multi");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![
                GatewayListeners {
                    name: "http".to_owned(),
                    port: 8082,
                    protocol: "HTTP".to_owned(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                },
                GatewayListeners {
                    name: "http-alt".to_owned(),
                    port: 8083,
                    protocol: "HTTP".to_owned(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                },
            ],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create multi-listener Gateway");

    ctx.await_gateway_condition(&gw_name, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    ctx.await_gateway_condition(&gw_name, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let fetched = gw_api.get(&gw_name).await.expect("failed to get Gateway");
    let listener_statuses = fetched
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .expect("Gateway should have listener statuses");

    assert_eq!(listener_statuses.len(), 2, "should have status for both listeners");

    for ls in listener_statuses {
        let accepted = ls
            .conditions
            .iter()
            .any(|c| c.type_ == "Accepted" && c.status == "True");
        assert!(accepted, "listener {name} should be Accepted", name = ls.name);
    }

    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_invalid_gateway_class_not_accepted() {
    let ctx = TestContext::new().await;
    let ns = ctx.create_test_namespace("gw-badgc").await;
    let gw_name = unique_name("gw-badgc");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: "nonexistent-class".to_owned(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8084,
                protocol: "HTTP".to_owned(),
                hostname: None,
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway with bad GatewayClass");

    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    let fetched = gw_api.get(&gw_name).await.expect("failed to get Gateway");
    let is_accepted = fetched
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|conds| conds.iter().any(|c| c.type_ == "Accepted" && c.status == "True"));
    assert!(
        !is_accepted,
        "Gateway referencing nonexistent GatewayClass should not be Accepted"
    );

    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_parameters_ref_rejected() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-pref-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-pref").await;
    let gw_name = unique_name("gw-pref");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8085,
                protocol: "HTTP".to_owned(),
                hostname: None,
                tls: None,
                allowed_routes: None,
            }],
            infrastructure: Some(GatewayInfrastructure {
                parameters_ref: Some(GatewayInfrastructureParametersRef {
                    group: "example.io".to_owned(),
                    kind: "Config".to_owned(),
                    name: "some-config".to_owned(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway with parametersRef");

    let cond = ctx
        .await_gateway_condition(&gw_name, &ns, "Accepted", "False", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(cond.reason, "InvalidParameters", "should reject with InvalidParameters");

    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_child_resources_created() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-child-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-child").await;
    let gw = ctx.create_gateway("gw-child", &ns, &gc, 8086).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let child = format!("praxis-{gw}");

    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), &ns);
    let cm = cm_api.get(&child).await.expect("child ConfigMap should exist");
    assert!(
        cm.data.is_some_and(|d| d.contains_key("config.yaml")),
        "ConfigMap should contain config.yaml"
    );

    let deploy_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let deploy = deploy_api.get(&child).await.expect("child Deployment should exist");
    let owner_refs = deploy
        .metadata
        .owner_references
        .as_ref()
        .expect("Deployment should have owner references");
    assert!(
        owner_refs.iter().any(|r| r.kind == "Gateway" && r.name == gw),
        "Deployment should be owned by the Gateway"
    );

    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
    let svc = svc_api.get(&child).await.expect("child Service should exist");
    let svc_type = svc.spec.as_ref().and_then(|s| s.type_.as_deref()).unwrap_or("");
    assert_eq!(svc_type, "LoadBalancer", "Service type should be LoadBalancer");

    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_listener_modification_updates_status() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-mod-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-mod").await;
    let gw_name = unique_name("gw-mod");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8087,
                protocol: "HTTP".to_owned(),
                hostname: None,
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway");

    ctx.await_gateway_condition(&gw_name, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let fetched = gw_api.get(&gw_name).await.expect("failed to get Gateway");
    let initial_listeners = fetched
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .map(|l| l.len())
        .unwrap_or(0);
    assert_eq!(initial_listeners, 1, "should start with one listener status");

    let mut updated = gw_api.get(&gw_name).await.expect("failed to get Gateway for update");
    updated.spec.listeners.push(GatewayListeners {
        name: "http-second".to_owned(),
        port: 8088,
        protocol: "HTTP".to_owned(),
        hostname: None,
        tls: None,
        allowed_routes: None,
    });
    gw_api
        .replace(&gw_name, &PostParams::default(), &updated)
        .await
        .expect("failed to update Gateway with second listener");

    let final_gw = await_listener_count(&gw_api, &gw_name, 2, RECONCILE_TIMEOUT_SECS).await;
    let listener_statuses = final_gw
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .expect("should have listener statuses");
    assert_eq!(
        listener_statuses.len(),
        2,
        "should have two listener statuses after update"
    );

    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

// ---------------------------------------------------------------------------
// HTTPRoute Lifecycle Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn httproute_accepted_with_valid_parent_ref() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-accept-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-accept").await;
    let gw = ctx.create_gateway("hr-accept-gw", &ns, &gc, 8090).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-accept-svc", &ns, 80).await;
    let route = ctx.create_httproute("hr-accept-route", &ns, &gw, &backend, 80).await;

    let cond = ctx
        .await_httproute_parent_condition(&route, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(cond.reason, "Accepted", "HTTPRoute should be accepted");

    ctx.cleanup_httproute(&route, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_resolved_refs_with_valid_backend() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-resolved-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-resolved").await;
    let gw = ctx.create_gateway("hr-resolved-gw", &ns, &gc, 8091).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-resolved-svc", &ns, 80).await;
    let route = ctx.create_httproute("hr-resolved-rt", &ns, &gw, &backend, 80).await;

    let cond = ctx
        .await_httproute_parent_condition(&route, &ns, "ResolvedRefs", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(cond.reason, "ResolvedRefs", "refs should be resolved");

    ctx.cleanup_httproute(&route, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_unresolved_refs_with_missing_backend() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-unresolved-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-unresolved").await;
    let gw = ctx.create_gateway("hr-unresolved-gw", &ns, &gc, 8092).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let route_name = unique_name("hr-unresolved");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: "does-not-exist".to_owned(),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create HTTPRoute with missing backend");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &ns, "ResolvedRefs", "False", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(cond.reason, "BackendNotFound", "should report BackendNotFound");

    ctx.cleanup_httproute(&route_name, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_invalid_backend_kind_unresolved() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-badkind-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-badkind").await;
    let gw = ctx.create_gateway("hr-badkind-gw", &ns, &gc, 8093).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let route_name = unique_name("hr-badkind");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: "some-resource".to_owned(),
                    kind: Some("ConfigMap".to_owned()),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create HTTPRoute with bad backend kind");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &ns, "ResolvedRefs", "False", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.status, "False",
        "non-Service backend kind should produce unresolved refs"
    );

    ctx.cleanup_httproute(&route_name, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_hostname_mismatch_not_accepted() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-hostmis-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-hostmis").await;
    let gw_name = unique_name("hr-hostmis-gw");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8094,
                protocol: "HTTP".to_owned(),
                hostname: Some("example.com".to_owned()),
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway with hostname");

    ctx.await_gateway_condition(&gw_name, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-hostmis-svc", &ns, 80).await;
    let route_name = unique_name("hr-hostmis");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            hostnames: Some(vec!["other-domain.org".to_owned()]),
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw_name.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: backend.clone(),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create HTTPRoute with mismatched hostname");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &ns, "Accepted", "False", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.reason, "NoMatchingListenerHostname",
        "should reject for hostname mismatch"
    );

    ctx.cleanup_httproute(&route_name, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_wildcard_hostname_accepted() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-wild-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-wild").await;
    let gw_name = unique_name("hr-wild-gw");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8095,
                protocol: "HTTP".to_owned(),
                hostname: Some("*.example.com".to_owned()),
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway with wildcard hostname");

    ctx.await_gateway_condition(&gw_name, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-wild-svc", &ns, 80).await;
    let route_name = unique_name("hr-wild");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            hostnames: Some(vec!["app.example.com".to_owned()]),
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw_name.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: backend.clone(),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create HTTPRoute with matching wildcard");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.reason, "Accepted",
        "wildcard hostname should match subdomain route"
    );

    ctx.cleanup_httproute(&route_name, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_no_hostname_accepted_by_any_listener() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-nohost-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-nohost").await;
    let gw_name = unique_name("hr-nohost-gw");
    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);

    let gw = Gateway {
        metadata: ObjectMeta {
            name: Some(gw_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: GatewaySpec {
            gateway_class_name: gc.clone(),
            listeners: vec![GatewayListeners {
                name: "http".to_owned(),
                port: 8096,
                protocol: "HTTP".to_owned(),
                hostname: Some("specific.example.com".to_owned()),
                tls: None,
                allowed_routes: None,
            }],
            ..Default::default()
        },
        status: None,
    };
    gw_api
        .create(&PostParams::default(), &gw)
        .await
        .expect("failed to create Gateway");

    ctx.await_gateway_condition(&gw_name, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-nohost-svc", &ns, 80).await;
    let route = ctx.create_httproute("hr-nohost-rt", &ns, &gw_name, &backend, 80).await;

    let cond = ctx
        .await_httproute_parent_condition(&route, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.reason, "Accepted",
        "route with no hostnames should be accepted by any listener"
    );

    ctx.cleanup_httproute(&route, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw_name, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_path_match_prefix() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-path-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-path").await;
    let gw = ctx.create_gateway("hr-path-gw", &ns, &gc, 8097).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-path-svc", &ns, 80).await;
    let route_name = unique_name("hr-path");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw.clone(),
                namespace: Some(ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                matches: Some(vec![HttpRouteRulesMatches {
                    path: Some(HttpRouteRulesMatchesPath {
                        r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/api".to_owned()),
                    }),
                    ..Default::default()
                }]),
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: backend.clone(),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create HTTPRoute with path prefix");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(cond.reason, "Accepted", "path-prefix route should be accepted");

    ctx.cleanup_httproute(&route_name, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_cross_namespace_denied_without_grant() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-xns-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gw_ns = ctx.create_test_namespace("hr-xns-gw").await;
    let backend_ns = ctx.create_test_namespace("hr-xns-bk").await;
    let gw = ctx.create_gateway("hr-xns-gw", &gw_ns, &gc, 8098).await;
    ctx.await_gateway_condition(&gw, &gw_ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-xns-svc", &backend_ns, 80).await;
    let route_name = unique_name("hr-xns");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &gw_ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(gw_ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw.clone(),
                namespace: Some(gw_ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: backend.clone(),
                    namespace: Some(backend_ns.clone()),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create cross-namespace HTTPRoute");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &gw_ns, "ResolvedRefs", "False", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.reason, "RefNotPermitted",
        "cross-namespace ref without ReferenceGrant should be denied"
    );

    ctx.cleanup_httproute(&route_name, &gw_ns).await;
    ctx.cleanup_service(&backend, &backend_ns).await;
    ctx.cleanup_gateway(&gw, &gw_ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&gw_ns).await;
    ctx.cleanup_namespace(&backend_ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_cross_namespace_allowed_with_grant() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-grant-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gw_ns = ctx.create_test_namespace("hr-grant-gw").await;
    let backend_ns = ctx.create_test_namespace("hr-grant-bk").await;
    let gw = ctx.create_gateway("hr-grant-gw", &gw_ns, &gc, 8099).await;
    ctx.await_gateway_condition(&gw, &gw_ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-grant-svc", &backend_ns, 80).await;

    let grant_api: Api<ReferenceGrant> = Api::namespaced(ctx.client.clone(), &backend_ns);
    let grant = ReferenceGrant {
        metadata: ObjectMeta {
            name: Some("allow-from-gw-ns".to_owned()),
            namespace: Some(backend_ns.clone()),
            ..Default::default()
        },
        spec: ReferenceGrantSpec {
            from: vec![ReferenceGrantFrom {
                group: "gateway.networking.k8s.io".to_owned(),
                kind: "HTTPRoute".to_owned(),
                namespace: gw_ns.clone(),
            }],
            to: vec![ReferenceGrantTo {
                group: String::new(),
                kind: "Service".to_owned(),
                name: None,
            }],
        },
    };
    grant_api
        .create(&PostParams::default(), &grant)
        .await
        .expect("failed to create ReferenceGrant");

    let route_name = unique_name("hr-grant");
    let route_api: Api<HTTPRoute> = Api::namespaced(ctx.client.clone(), &gw_ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(route_name.clone()),
            namespace: Some(gw_ns.clone()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                name: gw.clone(),
                namespace: Some(gw_ns.clone()),
                ..Default::default()
            }]),
            rules: Some(vec![HttpRouteRules {
                backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                    name: backend.clone(),
                    namespace: Some(backend_ns.clone()),
                    port: Some(80),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };
    route_api
        .create(&PostParams::default(), &route)
        .await
        .expect("failed to create cross-namespace HTTPRoute with grant");

    let cond = ctx
        .await_httproute_parent_condition(&route_name, &gw_ns, "ResolvedRefs", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    assert_eq!(
        cond.reason, "ResolvedRefs",
        "cross-namespace ref with ReferenceGrant should be resolved"
    );

    ctx.cleanup_httproute(&route_name, &gw_ns).await;
    grant_api
        .delete("allow-from-gw-ns", &DeleteParams::default())
        .await
        .expect("failed to delete ReferenceGrant");
    ctx.cleanup_service(&backend, &backend_ns).await;
    ctx.cleanup_gateway(&gw, &gw_ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&gw_ns).await;
    ctx.cleanup_namespace(&backend_ns).await;
}

#[tokio::test]
#[ignore]
async fn httproute_attached_count_reflected_in_listener() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("hr-count-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("hr-count").await;
    let gw = ctx.create_gateway("hr-count-gw", &ns, &gc, 8100).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let backend = ctx.create_backend_service("hr-count-svc", &ns, 80).await;
    let r1 = ctx.create_httproute("hr-count-r1", &ns, &gw, &backend, 80).await;
    let r2 = ctx.create_httproute("hr-count-r2", &ns, &gw, &backend, 80).await;

    ctx.await_httproute_parent_condition(&r1, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;
    ctx.await_httproute_parent_condition(&r2, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);
    let fetched = gw_api.get(&gw).await.expect("failed to get Gateway");

    let listener_statuses = fetched
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .expect("Gateway should have listener statuses");

    assert!(
        listener_statuses.iter().any(|ls| ls.attached_routes >= 2),
        "at least one listener should report >= 2 attached routes"
    );

    ctx.cleanup_httproute(&r1, &ns).await;
    ctx.cleanup_httproute(&r2, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

// ---------------------------------------------------------------------------
// Data-Plane Verification Tests
// ---------------------------------------------------------------------------

// Requires stable proxy pods (no concurrent rolling updates). Health probes
// go through the API server's pod proxy, so this test needs no load-balancer
// address for the reserved metrics port.
#[tokio::test]
#[ignore]
async fn data_plane_health_probes_respond() {
    if std::env::var("RUN_DATAPLANE_TESTS").is_err() {
        eprintln!("skipped: set RUN_DATAPLANE_TESTS=1 (requires MetalLB)");
        return;
    }
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("dp-health-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("dp-health").await;
    let gw = ctx.create_gateway("dp-health-gw", &ns, &gc, 8101).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    // Praxis serves /healthy and /ready on admin.metrics_address, the reserved
    // metrics port 9902, which the Gateway Service does not expose. Probe the
    // proxy pod directly through the API server's pod proxy.
    for path in ["healthy", "ready"] {
        await_pod_endpoint_ok(&ctx, &ns, &gw, 9902, path).await;
    }

    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

// Requires MetalLB and stable proxy pods (no concurrent rolling updates).
#[tokio::test]
#[ignore]
async fn data_plane_routes_traffic_to_backend() {
    if std::env::var("RUN_DATAPLANE_TESTS").is_err() {
        eprintln!("skipped: set RUN_DATAPLANE_TESTS=1 (requires MetalLB)");
        return;
    }
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("dp-traffic-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("dp-traffic").await;
    let gw = ctx.create_gateway("dp-traffic-gw", &ns, &gc, 8102).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    ctx.deploy_echo_server("echo-server", &ns, 8080).await;
    let backend = ctx.create_backend_service("echo-server", &ns, 8080).await;
    let route = ctx.create_httproute("dp-traffic-rt", &ns, &gw, &backend, 8080).await;
    ctx.await_httproute_parent_condition(&route, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);
    let addr = await_gateway_address(&gw_api, &gw, DATA_PLANE_TIMEOUT_SECS).await;

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client");

    let url = format!("http://{addr}:8102/test");
    let resp = retry_http_get(&http, &url, DATA_PLANE_TIMEOUT_SECS).await;
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 404,
        "proxy should forward traffic (got status {status})",
        status = resp.status()
    );

    ctx.cleanup_httproute(&route, &ns).await;
    ctx.cleanup_service(&backend, &ns).await;
    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

// ---------------------------------------------------------------------------
// Cleanup and Owner Reference Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn gateway_deletion_cascades_to_children() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-cascade-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-cascade").await;
    let gw = ctx.create_gateway("gw-cascade", &ns, &gc, 8103).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let child = format!("praxis-{gw}");
    let deploy_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    deploy_api
        .get(&child)
        .await
        .expect("child Deployment should exist before deletion");

    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);
    gw_api
        .delete(&gw, &DeleteParams::default())
        .await
        .expect("failed to delete Gateway");

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(RECONCILE_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("child Deployment was not garbage collected within timeout");
        }
        if deploy_api.get(&child).await.is_err() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }

    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
    assert!(
        svc_api.get(&child).await.is_err(),
        "child Service should also be garbage collected"
    );

    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), &ns);
    assert!(
        cm_api.get(&child).await.is_err(),
        "child ConfigMap should also be garbage collected"
    );

    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

#[tokio::test]
#[ignore]
async fn gateway_listener_conditions_complete() {
    let ctx = TestContext::new().await;
    let gc = ctx.create_gateway_class("gw-lcond-gc").await;
    ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let ns = ctx.create_test_namespace("gw-lcond").await;
    let gw = ctx.create_gateway("gw-lcond", &ns, &gc, 8104).await;
    ctx.await_gateway_condition(&gw, &ns, "Programmed", "True", RECONCILE_TIMEOUT_SECS)
        .await;

    let gw_api: Api<Gateway> = Api::namespaced(ctx.client.clone(), &ns);
    let fetched = gw_api.get(&gw).await.expect("failed to get Gateway");

    let listener_statuses = fetched
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_ref())
        .expect("Gateway should have listener statuses");

    assert_eq!(listener_statuses.len(), 1, "should have exactly one listener status");

    let ls = &listener_statuses[0];
    let cond_types: Vec<&str> = ls.conditions.iter().map(|c| c.type_.as_str()).collect();

    assert!(
        cond_types.contains(&"Accepted"),
        "listener should have Accepted condition"
    );
    assert!(
        cond_types.contains(&"Programmed"),
        "listener should have Programmed condition"
    );
    assert!(
        cond_types.contains(&"Conflicted"),
        "listener should have Conflicted condition"
    );
    assert!(
        cond_types.contains(&"ResolvedRefs"),
        "listener should have ResolvedRefs condition"
    );

    let conflicted = ls
        .conditions
        .iter()
        .find(|c| c.type_ == "Conflicted")
        .expect("Conflicted condition should exist");
    assert_eq!(conflicted.status, "False", "single listener should have no conflicts");

    ctx.cleanup_gateway(&gw, &ns).await;
    ctx.cleanup_gateway_class(&gc).await;
    ctx.cleanup_namespace(&ns).await;
}

// ---------------------------------------------------------------------------
// Test Utilities
// ---------------------------------------------------------------------------

struct TestContext {
    client: Client,
}

impl TestContext {
    async fn new() -> Self {
        let client = Client::try_default().await.expect("failed to create kube client");
        Self { client }
    }

    async fn create_gateway_class(&self, prefix: &str) -> String {
        let name = unique_name(prefix);
        let gc_api: Api<GatewayClass> = Api::all(self.client.clone());

        let gc = GatewayClass {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            spec: GatewayClassSpec {
                controller_name: CONTROLLER_NAME.to_owned(),
                ..Default::default()
            },
            status: None,
        };
        gc_api
            .create(&PostParams::default(), &gc)
            .await
            .expect("failed to create GatewayClass");
        name
    }

    async fn create_test_namespace(&self, prefix: &str) -> String {
        let name = unique_name(prefix);
        let ns_api: Api<Namespace> = Api::all(self.client.clone());
        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        ns_api
            .create(&PostParams::default(), &ns)
            .await
            .expect("failed to create test namespace");
        name
    }

    async fn create_gateway(&self, prefix: &str, namespace: &str, gc_name: &str, port: i32) -> String {
        let name = unique_name(prefix);
        let gw_api: Api<Gateway> = Api::namespaced(self.client.clone(), namespace);

        let gw = Gateway {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: GatewaySpec {
                gateway_class_name: gc_name.to_owned(),
                listeners: vec![GatewayListeners {
                    name: "http".to_owned(),
                    port,
                    protocol: "HTTP".to_owned(),
                    hostname: None,
                    tls: None,
                    allowed_routes: None,
                }],
                ..Default::default()
            },
            status: None,
        };
        gw_api
            .create(&PostParams::default(), &gw)
            .await
            .expect("failed to create Gateway");
        name
    }

    async fn create_backend_service(&self, name: &str, namespace: &str, port: i32) -> String {
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(vec![ServicePort {
                    port,
                    protocol: Some("TCP".to_owned()),
                    ..Default::default()
                }]),
                selector: Some([("app".to_owned(), name.to_owned())].into_iter().collect()),
                ..Default::default()
            }),
            ..Default::default()
        };
        svc_api
            .create(&PostParams::default(), &svc)
            .await
            .expect("failed to create backend Service");
        name.to_owned()
    }

    async fn create_httproute(&self, prefix: &str, namespace: &str, gateway: &str, backend: &str, port: i32) -> String {
        let name = unique_name(prefix);
        let route_api: Api<HTTPRoute> = Api::namespaced(self.client.clone(), namespace);

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: gateway.to_owned(),
                    namespace: Some(namespace.to_owned()),
                    ..Default::default()
                }]),
                rules: Some(vec![HttpRouteRules {
                    backend_refs: Some(vec![HttpRouteRulesBackendRefs {
                        name: backend.to_owned(),
                        port: Some(port),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };
        route_api
            .create(&PostParams::default(), &route)
            .await
            .expect("failed to create HTTPRoute");
        name
    }

    async fn deploy_echo_server(&self, name: &str, namespace: &str, port: i32) {
        use k8s_openapi::{
            api::{
                apps::v1::DeploymentSpec,
                core::v1::{Container, ContainerPort, PodSpec, PodTemplateSpec},
            },
            apimachinery::pkg::apis::meta::v1::LabelSelector,
        };

        let labels: std::collections::BTreeMap<String, String> =
            [("app".to_owned(), name.to_owned())].into_iter().collect();

        let deploy_api: Api<Deployment> = Api::namespaced(self.client.clone(), namespace);
        let deploy = Deployment {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            name: "echo".to_owned(),
                            image: Some("hashicorp/http-echo:latest".to_owned()),
                            args: Some(vec![format!("-listen=:{port}"), "-text=ok".to_owned()]),
                            ports: Some(vec![ContainerPort {
                                container_port: port,
                                ..Default::default()
                            }]),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        deploy_api
            .create(&PostParams::default(), &deploy)
            .await
            .expect("failed to deploy echo server");

        await_deployment_ready(&deploy_api, name, DATA_PLANE_TIMEOUT_SECS).await;
    }

    async fn await_cluster_condition<R>(&self, name: &str, type_: &str, status: &str, timeout_secs: u64) -> Condition
    where
        R: kube::Resource<Scope = k8s_openapi::ClusterResourceScope>
            + serde::de::DeserializeOwned
            + Clone
            + std::fmt::Debug
            + Send
            + Sync
            + 'static,
        R: HasConditions,
        <R as kube::Resource>::DynamicType: Default,
    {
        let api: Api<R> = Api::all(self.client.clone());
        poll_condition(&api, name, type_, status, timeout_secs).await
    }

    async fn await_gateway_condition(
        &self,
        name: &str,
        namespace: &str,
        type_: &str,
        status: &str,
        timeout_secs: u64,
    ) -> Condition {
        let api: Api<Gateway> = Api::namespaced(self.client.clone(), namespace);
        poll_condition(&api, name, type_, status, timeout_secs).await
    }

    async fn await_generation_bump(&self, name: &str, min_generation: i64, timeout_secs: u64) -> Condition {
        let api: Api<GatewayClass> = Api::all(self.client.clone());
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

        loop {
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for observedGeneration > {min_generation} on {name}");
            }
            if let Ok(resource) = api.get(name).await {
                if let Some(cond) = find_condition(&resource, "Accepted", "True") {
                    if cond.observed_generation.unwrap_or(0) > min_generation {
                        return cond;
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    async fn await_httproute_parent_condition(
        &self,
        route_name: &str,
        namespace: &str,
        type_: &str,
        status: &str,
        timeout_secs: u64,
    ) -> Condition {
        let api: Api<HTTPRoute> = Api::namespaced(self.client.clone(), namespace);
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

        loop {
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for parent condition {type_}={status} on HTTPRoute {namespace}/{route_name}");
            }
            if let Ok(route) = api.get(route_name).await {
                if let Some(cond) = find_httproute_parent_condition(&route, type_, status) {
                    return cond;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    async fn cleanup_gateway_class(&self, name: &str) {
        let api: Api<GatewayClass> = Api::all(self.client.clone());
        drop(api.delete(name, &DeleteParams::default()).await);
    }

    async fn cleanup_gateway(&self, name: &str, namespace: &str) {
        let api: Api<Gateway> = Api::namespaced(self.client.clone(), namespace);
        drop(api.delete(name, &DeleteParams::default()).await);
    }

    async fn cleanup_httproute(&self, name: &str, namespace: &str) {
        let api: Api<HTTPRoute> = Api::namespaced(self.client.clone(), namespace);
        drop(api.delete(name, &DeleteParams::default()).await);
    }

    async fn cleanup_service(&self, name: &str, namespace: &str) {
        let api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        drop(api.delete(name, &DeleteParams::default()).await);
    }

    async fn cleanup_namespace(&self, name: &str) {
        let api: Api<Namespace> = Api::all(self.client.clone());
        drop(api.delete(name, &DeleteParams::default()).await);
    }
}

trait HasConditions {
    fn conditions(&self) -> Option<&[Condition]>;
}

impl HasConditions for GatewayClass {
    fn conditions(&self) -> Option<&[Condition]> {
        self.status.as_ref().and_then(|s| s.conditions.as_deref())
    }
}

impl HasConditions for Gateway {
    fn conditions(&self) -> Option<&[Condition]> {
        self.status.as_ref().and_then(|s| s.conditions.as_deref())
    }
}

async fn poll_condition<R>(api: &Api<R>, name: &str, type_: &str, status: &str, timeout_secs: u64) -> Condition
where
    R: kube::Resource + serde::de::DeserializeOwned + Clone + std::fmt::Debug + Send + Sync + 'static,
    R: HasConditions,
{
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for condition {type_}={status} on {name}");
        }
        if let Ok(resource) = api.get(name).await {
            if let Some(cond) = find_condition(&resource, type_, status) {
                return cond;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

fn find_condition<R: HasConditions>(resource: &R, type_: &str, status: &str) -> Option<Condition> {
    resource
        .conditions()?
        .iter()
        .find(|c| c.type_ == type_ && c.status == status)
        .cloned()
}

fn find_httproute_parent_condition(route: &HTTPRoute, type_: &str, status: &str) -> Option<Condition> {
    let parents = &route.status.as_ref()?.parents;
    for parent in parents {
        for cond in &parent.conditions {
            if cond.type_ == type_ && cond.status == status {
                return Some(cond.clone());
            }
        }
    }
    None
}

fn unique_name(prefix: &str) -> String {
    let ts = chrono::Utc::now().timestamp_millis();
    let rand: u16 = (ts & 0xFFFF) as u16;
    format!("{prefix}-{ts}-{rand}")
}

async fn await_gateway_address(api: &Api<Gateway>, name: &str, timeout_secs: u64) -> String {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for Gateway address on {name}");
        }
        if let Ok(gw) = api.get(name).await {
            if let Some(addr) = gw
                .status
                .as_ref()
                .and_then(|s| s.addresses.as_ref())
                .and_then(|addrs| addrs.first())
                .map(|a| a.value.clone())
            {
                if !addr.is_empty() {
                    return addr;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

async fn await_listener_count(api: &Api<Gateway>, name: &str, expected: usize, timeout_secs: u64) -> Gateway {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {expected} listener statuses on {name}");
        }
        if let Ok(gw) = api.get(name).await {
            let count = gw
                .status
                .as_ref()
                .and_then(|s| s.listeners.as_ref())
                .map(|l| l.len())
                .unwrap_or(0);
            if count == expected {
                return gw;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

async fn await_deployment_ready(api: &Api<Deployment>, name: &str, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for Deployment {name} to be ready");
        }
        if let Ok(deploy) = api.get(name).await {
            let ready = deploy.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
            if ready >= 1 {
                return;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

async fn retry_http_get(client: &reqwest::Client, url: &str, timeout_secs: u64) -> reqwest::Response {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for HTTP response from {url}");
        }
        match client.get(url).send().await {
            Ok(resp) => return resp,
            Err(_) => {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            },
        }
    }
}

// Waits until `path` on the gateway's proxy pod answers 2xx on `port` through
// the API server's pod proxy.
//
// The reserved metrics port is not exposed by the Gateway Service, so probes
// for admin endpoints must reach the pod itself. The pod is looked up by the
// gateway's instance label on every attempt, so a replaced pod is followed
// while its replacement starts serving.
async fn await_pod_endpoint_ok(ctx: &TestContext, namespace: &str, gateway: &str, port: u16, path: &str) {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), namespace);
    let selector = format!("app.kubernetes.io/instance={gateway}");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(DATA_PLANE_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for 2xx from pod port {port} /{path}");
        }
        let pod_list = pods
            .list(&ListParams::default().labels(&selector))
            .await
            .expect("list proxy pods");
        let pod_name = pod_list.items.first().map(|pod| pod.name_any());
        if let Some(pod_name) = pod_name {
            let request = http::Request::get(format!(
                "/api/v1/namespaces/{namespace}/pods/{pod_name}:{port}/proxy/{path}"
            ))
            .body(kube::client::Body::empty())
            .expect("build pod proxy request");
            if let Ok(response) = ctx.client.send(request).await {
                if response.status().is_success() {
                    return;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

// Run this test with PRAXIS_MONITORING_ENABLED matching the operator Deployment.
// Exercise disabled mode before installing the PodMonitor CRD, enabled mode after
// installing it, and disabled mode again to cover stale-resource cleanup.
#[cfg(test)]
mod monitoring_tests {
    use k8s_openapi::api::networking::v1::NetworkPolicy;
    use kube::{
        ResourceExt as _,
        api::{DynamicObject, Patch, PatchParams},
    };
    use praxis_operator::resources::monitoring::{
        MonitoringConfig, build_metrics_network_policy, build_pod_monitor, monitoring_config, pod_monitor_api_resource,
    };

    use super::*;

    #[tokio::test]
    #[ignore]
    async fn monitoring_reconciliation() {
        let ctx = TestContext::new().await;
        let config = monitoring_config().expect("valid monitoring environment");
        let gc = ctx.create_gateway_class("monitoring-gc").await;
        ctx.await_cluster_condition::<GatewayClass>(&gc, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
            .await;
        let ns = ctx.create_test_namespace("monitoring").await;
        let name = ctx.create_gateway("monitoring", &ns, &gc, 8080).await;
        ctx.await_gateway_condition(&name, &ns, "Accepted", "True", RECONCILE_TIMEOUT_SECS)
            .await;
        let child = format!("praxis-{name}");
        let deployments = Api::<Deployment>::namespaced(ctx.client.clone(), &ns);
        await_deployment_ready(&deployments, &child, DATA_PLANE_TIMEOUT_SECS).await;
        let gateways = Api::<Gateway>::namespaced(ctx.client.clone(), &ns);
        let gw = gateways.get(&name).await.unwrap();
        let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
        let monitors = Api::<DynamicObject>::namespaced_with(ctx.client.clone(), &ns, &pod_monitor_api_resource());
        if let Some(config) = config {
            assert_monitoring_matches(&policies, &monitors, &child, &ns, &gw, &config).await;
            let pods = Api::<Pod>::namespaced(ctx.client.clone(), &ns)
                .list(&ListParams::default().labels(&format!("app.kubernetes.io/instance={name}")))
                .await
                .unwrap();
            let pod_name = pods.items.first().expect("proxy pod").name_any();
            let request = http::Request::get(format!("/api/v1/namespaces/{ns}/pods/{pod_name}:8080/proxy/"))
                .body(kube::client::Body::empty())
                .unwrap();
            let response = ctx.client.send(request).await.expect("reach routeless proxy");
            assert_eq!(
                response.status(),
                http::StatusCode::NOT_FOUND,
                "routeless published proxy returns HTTP 404"
            );
            gateways
                .patch(
                    &name,
                    &PatchParams::default(),
                    &Patch::Merge(
                        serde_json::json!({"spec":{"listeners":[{"name":"http","port":9902,"protocol":"HTTP"}]}}),
                    ),
                )
                .await
                .unwrap();
            await_monitoring_absent(&policies, &monitors, &child).await;
            gateways
                .patch(
                    &name,
                    &PatchParams::default(),
                    &Patch::Merge(
                        serde_json::json!({"spec":{"listeners":[{"name":"http","port":8080,"protocol":"HTTP"}]}}),
                    ),
                )
                .await
                .unwrap();
            assert_monitoring_matches(&policies, &monitors, &child, &ns, &gw, &config).await;
        } else {
            let crds = Api::<
                k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
            >::all(ctx.client.clone());
            let has_crd = crds
                .get_opt("podmonitors.monitoring.coreos.com")
                .await
                .unwrap()
                .is_some();
            let stale_config = MonitoringConfig {
                namespace: "monitoring".to_owned(),
                pod_selector: std::collections::BTreeMap::from([("app".to_owned(), "prometheus".to_owned())]),
            };
            let policy =
                build_metrics_network_policy(&child, &ns, &gw, &stale_config, &[("http".to_owned(), 8080)]).unwrap();
            // `force` because the point is to stage a policy the operator
            // owns from the enabled phase: without it the apply loses the
            // `.spec.ingress` field-manager race and 409s.
            policies
                .patch(
                    &child,
                    &PatchParams::apply("monitoring-test").force(),
                    &Patch::Apply(&policy),
                )
                .await
                .unwrap();
            if has_crd {
                let monitor = build_pod_monitor(&child, &ns, &gw).unwrap();
                monitors
                    .patch(
                        &child,
                        &PatchParams::apply("monitoring-test").force(),
                        &Patch::Apply(&monitor),
                    )
                    .await
                    .unwrap();
            }
            gateways.patch(&name, &PatchParams::default(), &Patch::Merge(serde_json::json!({"metadata":{"annotations":{"test.praxis.sh/reconcile":"monitoring-cleanup"}}}))).await.unwrap();
            await_monitoring_absent(&policies, &monitors, &child).await;
            assert!(
                deployments.get_opt(&child).await.unwrap().is_some(),
                "disabled monitoring does not prevent data-plane reconciliation without the CRD"
            );
        }
        ctx.cleanup_gateway(&name, &ns).await;
        ctx.cleanup_gateway_class(&gc).await;
        ctx.cleanup_namespace(&ns).await;
    }

    async fn await_monitoring_absent(policies: &Api<NetworkPolicy>, monitors: &Api<DynamicObject>, name: &str) {
        let deadline = tokio::time::Instant::now()
            .checked_add(tokio::time::Duration::from_secs(RECONCILE_TIMEOUT_SECS))
            .expect("test deadline fits the clock");
        loop {
            if policies.get_opt(name).await.unwrap().is_none() && monitor_is_absent(monitors, name).await {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "owned monitoring resources must be reclaimed"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }
    async fn await_monitoring_present(
        policies: &Api<NetworkPolicy>,
        monitors: &Api<DynamicObject>,
        name: &str,
    ) -> (NetworkPolicy, DynamicObject) {
        let deadline = tokio::time::Instant::now()
            .checked_add(tokio::time::Duration::from_secs(RECONCILE_TIMEOUT_SECS))
            .expect("test deadline fits the clock");
        loop {
            if let (Some(policy), Some(monitor)) = (
                policies.get_opt(name).await.unwrap(),
                monitors.get_opt(name).await.unwrap(),
            ) {
                return (policy, monitor);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "enabled monitoring must reconcile both resources"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    /// Waits for both monitoring resources and checks their stored contents.
    async fn assert_monitoring_matches(
        policies: &Api<NetworkPolicy>,
        monitors: &Api<DynamicObject>,
        child: &str,
        ns: &str,
        gw: &Gateway,
        config: &MonitoringConfig,
    ) {
        let (policy, monitor) = await_monitoring_present(policies, monitors, child).await;
        let mut expected = build_metrics_network_policy(child, ns, gw, config, &[("http".to_owned(), 8080)]).unwrap();
        expected.metadata = policy.metadata.clone();
        assert_eq!(policy, expected, "the API stores the intended policy");
        assert_eq!(
            monitor.data,
            build_pod_monitor(child, ns, gw).unwrap().data,
            "the API stores the intended monitor spec"
        );
    }

    async fn monitor_is_absent(monitors: &Api<DynamicObject>, name: &str) -> bool {
        match monitors.get(name).await {
            Ok(_) => false,
            Err(kube::Error::Api(status)) if status.code == 404 => true,
            Err(error) => panic!("unexpected PodMonitor API failure: {error}"),
        }
    }
}
