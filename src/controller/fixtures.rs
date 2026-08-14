// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway API object builders shared by the controller's tests.
//!
//! The reconciliation path is split across several modules that all
//! need the same handful of listeners, routes, and namespaces. Keeping
//! one copy here means a change to a Gateway API type is a change in
//! one place, and the fixtures cannot drift apart between modules.

use std::collections::BTreeMap;

use gateway_api::{
    gateways::{
        GatewayListeners, GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesNamespaces,
        GatewayListenersAllowedRoutesNamespacesFrom, GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions,
        GatewayListenersTls, GatewayListenersTlsCertificateRefs,
    },
    httproutes::{HTTPRoute, HttpRouteSpec},
};
use k8s_openapi::{
    ByteString,
    api::{
        apps::v1::{DeploymentCondition, DeploymentStatus},
        core::v1::Namespace,
    },
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time},
    jiff::Timestamp,
};

// -----------------------------------------------------------------------------
// Builders
// -----------------------------------------------------------------------------

/// Builds a Gateway listener with the given name, port, and protocol.
pub(super) fn listener(name: &str, port: i32, protocol: &str) -> GatewayListeners {
    GatewayListeners {
        name: name.to_owned(),
        port,
        protocol: protocol.to_owned(),
        ..Default::default()
    }
}

/// Builds an HTTPS listener referencing a TLS secret, scoped by hostname.
pub(super) fn https_listener(name: &str, port: i32, secret: &str) -> GatewayListeners {
    GatewayListeners {
        hostname: Some(format!("{name}.example.com")),
        tls: Some(GatewayListenersTls {
            certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                name: secret.to_owned(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..listener(name, port, "HTTPS")
    }
}

/// Builds a listener carrying an explicit `allowedRoutes.namespaces.from`.
pub(super) fn listener_with_namespace_policy(from: GatewayListenersAllowedRoutesNamespacesFrom) -> GatewayListeners {
    GatewayListeners {
        allowed_routes: Some(GatewayListenersAllowedRoutes {
            namespaces: Some(GatewayListenersAllowedRoutesNamespaces {
                from: Some(from),
                selector: None,
            }),
            ..Default::default()
        }),
        ..listener("http", 80, "HTTP")
    }
}

/// Builds an `HTTPRoute` carrying the given hostnames.
pub(super) fn route_with_hostnames(hostnames: &[&str]) -> HTTPRoute {
    HTTPRoute {
        metadata: ObjectMeta {
            name: Some("route".to_owned()),
            namespace: Some("apps".to_owned()),
            ..Default::default()
        },
        spec: HttpRouteSpec {
            hostnames: Some(hostnames.iter().map(|h| (*h).to_owned()).collect()),
            ..Default::default()
        },
        status: None,
    }
}

/// Builds Secret data with the given `tls.crt` and `tls.key` contents.
pub(super) fn secret_data(cert: &str, key: &str) -> BTreeMap<String, ByteString> {
    [
        ("tls.crt".to_owned(), ByteString(cert.as_bytes().to_vec())),
        ("tls.key".to_owned(), ByteString(key.as_bytes().to_vec())),
    ]
    .into_iter()
    .collect()
}

/// Builds a `DeploymentStatus` carrying a single condition.
pub(super) fn deployment_status(type_: &str, status: &str, reason: &str) -> DeploymentStatus {
    DeploymentStatus {
        conditions: Some(vec![DeploymentCondition {
            last_transition_time: Some(Time(Timestamp::UNIX_EPOCH)),
            last_update_time: Some(Time(Timestamp::UNIX_EPOCH)),
            message: None,
            reason: Some(reason.to_owned()),
            status: status.to_owned(),
            type_: type_.to_owned(),
        }]),
        ..Default::default()
    }
}

/// Builds a `Namespace` with the given labels.
pub(super) fn namespace(name: &str, labels: &[(&str, &str)]) -> Namespace {
    Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            labels: Some(labels.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Builds a label-selector match expression.
pub(super) fn expression(
    key: &str,
    operator: &str,
    values: &[&str],
) -> GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions {
    GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions {
        key: key.to_owned(),
        operator: operator.to_owned(),
        values: Some(values.iter().map(|v| (*v).to_owned()).collect()),
    }
}

/// Builds a route with `rules` rules, the last of which uses an
/// unsupported `RegularExpression` path match.
pub(super) fn regex_route(rules: usize) -> HTTPRoute {
    use gateway_api::httproutes::{
        HttpRouteRules, HttpRouteRulesMatches, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType, HttpRouteSpec,
    };

    let built = (0..rules)
        .map(|index| {
            let kind = if index + 1 == rules {
                HttpRouteRulesMatchesPathType::RegularExpression
            } else {
                HttpRouteRulesMatchesPathType::Exact
            };
            HttpRouteRules {
                matches: Some(vec![HttpRouteRulesMatches {
                    path: Some(HttpRouteRulesMatchesPath {
                        r#type: Some(kind),
                        value: Some("/x".to_owned()),
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }
        })
        .collect();

    HTTPRoute {
        metadata: ObjectMeta::default(),
        spec: HttpRouteSpec {
            rules: Some(built),
            ..Default::default()
        },
        status: None,
    }
}
