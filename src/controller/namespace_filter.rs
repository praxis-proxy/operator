// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `allowedRoutes.namespaces` evaluation.
//!
//! A listener decides which namespaces may attach routes to it. The
//! policy is one of three modes, and the selector mode carries a full
//! Kubernetes label selector, so the evaluation is large enough to
//! separate from the status writing that consumes its answer.

use std::collections::BTreeMap;

use gateway_api::{
    gateways::{
        GatewayListeners, GatewayListenersAllowedRoutesNamespacesFrom, GatewayListenersAllowedRoutesNamespacesSelector,
        GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions,
    },
    httproutes::HTTPRoute,
};
use k8s_openapi::api::core::v1::Namespace;
use kube::Api;
use tracing::warn;

use crate::{
    gateway_api::{attachment::AttachedRoute, route_status},
    listing,
};

// -----------------------------------------------------------------------------
// Namespace Filtering
// -----------------------------------------------------------------------------

/// Filters attached routes by the `allowedRoutes.namespaces` policy on
/// each listener.
///
/// A route is retained if at least one listener it targets allows its
/// namespace. The default policy (when unspecified) is `Same`.
pub(super) async fn filter_routes_by_allowed_namespaces<'a>(
    attached: &[AttachedRoute<'a>],
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    client: &kube::Client,
) -> Vec<AttachedRoute<'a>> {
    let all_namespaces = fetch_all_namespaces(client).await;

    attached
        .iter()
        .filter(|attached| {
            route_allowed_by_any_listener(
                attached.route,
                &attached.section_names,
                listeners,
                gateway_ns,
                all_namespaces.as_deref(),
            )
        })
        .cloned()
        .collect()
}

/// Fetches all namespaces from the cluster, returning `None` on error.
async fn fetch_all_namespaces(client: &kube::Client) -> Option<Vec<Namespace>> {
    match listing::list_all(&Api::<Namespace>::all(client.clone())).await {
        Ok(namespaces) => Some(namespaces),
        Err(e) => {
            warn!(%e, "failed to list namespaces for route filtering");
            None
        },
    }
}

/// Checks whether a route is allowed by at least one targeted listener.
fn route_allowed_by_any_listener(
    route: &HTTPRoute,
    section_names: &[Option<String>],
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    all_namespaces: Option<&[Namespace]>,
) -> bool {
    let route_ns = route_status::route_namespace(route);
    section_names.iter().any(|section| {
        let matching: Vec<&GatewayListeners> = match section {
            Some(name) => listeners.iter().filter(|l| l.name == *name).collect(),
            None => listeners.iter().collect(),
        };
        matching
            .iter()
            .any(|listener| is_namespace_allowed(listener, route_ns, gateway_ns, all_namespaces))
    })
}

/// Checks whether a route namespace is allowed by a listener's policy.
///
/// Defaults to `Same` when `allowedRoutes` is unspecified.
fn is_namespace_allowed(
    listener: &GatewayListeners,
    route_ns: &str,
    gateway_ns: &str,
    all_namespaces: Option<&[Namespace]>,
) -> bool {
    let from = listener
        .allowed_routes
        .as_ref()
        .and_then(|ar| ar.namespaces.as_ref())
        .and_then(|ns| ns.from.as_ref());

    match from {
        None | Some(GatewayListenersAllowedRoutesNamespacesFrom::Same) => route_ns == gateway_ns,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::All) => true,
        Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector) => {
            namespace_matches_selector(listener, route_ns, all_namespaces)
        },
    }
}

/// Checks whether a route namespace matches the listener's label selector.
fn namespace_matches_selector(
    listener: &GatewayListeners,
    route_ns: &str,
    all_namespaces: Option<&[Namespace]>,
) -> bool {
    let selector = listener
        .allowed_routes
        .as_ref()
        .and_then(|ar| ar.namespaces.as_ref())
        .and_then(|ns| ns.selector.as_ref());

    let Some(selector) = selector else {
        return false;
    };
    let Some(all_ns) = all_namespaces else {
        return false;
    };

    all_ns.iter().any(|ns_obj| {
        let ns_name = ns_obj.metadata.name.as_deref().unwrap_or("");
        ns_name == route_ns && matches_label_selector(ns_obj, selector)
    })
}

/// Checks whether a namespace's labels satisfy a label selector.
///
/// Evaluates both `matchLabels` and `matchExpressions`.
fn matches_label_selector(ns_obj: &Namespace, selector: &GatewayListenersAllowedRoutesNamespacesSelector) -> bool {
    let ns_labels = ns_obj.metadata.labels.as_ref();

    if let Some(match_labels) = &selector.match_labels {
        let Some(labels) = ns_labels else {
            return false;
        };
        if !match_labels
            .iter()
            .all(|(k, v)| labels.get(k).is_some_and(|lv| lv == v))
        {
            return false;
        }
    }

    if let Some(expressions) = &selector.match_expressions {
        let labels = ns_labels.cloned().unwrap_or_default();
        for expr in expressions {
            if !evaluate_match_expression(expr, &labels) {
                return false;
            }
        }
    }

    true
}

/// Evaluates a single label-selector match expression against a label set.
fn evaluate_match_expression(
    expr: &GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions,
    labels: &BTreeMap<String, String>,
) -> bool {
    let key = &expr.key;
    let op = expr.operator.as_str();
    let values = expr.values.as_deref().unwrap_or(&[]);
    let has_key = labels.contains_key(key);
    let label_val = labels.get(key).map(String::as_str);

    match op {
        "In" => label_val.is_some_and(|v| values.iter().any(|ev| ev == v)),
        "NotIn" => label_val.is_none_or(|v| !values.iter().any(|ev| ev == v)),
        "Exists" => has_key,
        "DoesNotExist" => !has_key,
        _ => false,
    }
}

// -----------------------------------------------------------------------------

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::fixtures::{expression, listener, listener_with_namespace_policy, namespace};

    #[test]
    fn test_is_namespace_allowed_defaults_to_same() {
        let l = listener("http", 80, "HTTP");

        assert!(
            is_namespace_allowed(&l, "infra", "infra", None),
            "the default policy allows only the Gateway's own namespace"
        );
        assert!(
            !is_namespace_allowed(&l, "apps", "infra", None),
            "the default policy rejects other namespaces"
        );
    }

    #[test]
    fn test_is_namespace_allowed_all() {
        let l = listener_with_namespace_policy(GatewayListenersAllowedRoutesNamespacesFrom::All);

        assert!(
            is_namespace_allowed(&l, "apps", "infra", None),
            "the All policy accepts every namespace"
        );
    }

    #[test]
    fn test_is_namespace_allowed_selector_without_namespaces_is_denied() {
        let l = listener_with_namespace_policy(GatewayListenersAllowedRoutesNamespacesFrom::Selector);

        assert!(
            !is_namespace_allowed(&l, "apps", "infra", None),
            "a selector policy cannot be evaluated without the namespace list"
        );
    }

    #[test]
    fn test_matches_label_selector_match_labels() {
        let ns = namespace("apps", &[("team", "core")]);
        let selector = GatewayListenersAllowedRoutesNamespacesSelector {
            match_labels: Some([("team".to_owned(), "core".to_owned())].into_iter().collect()),
            match_expressions: None,
        };

        assert!(
            matches_label_selector(&ns, &selector),
            "matching labels should satisfy the selector"
        );
    }

    #[test]
    fn test_matches_label_selector_rejects_missing_label() {
        let ns = namespace("apps", &[]);
        let selector = GatewayListenersAllowedRoutesNamespacesSelector {
            match_labels: Some([("team".to_owned(), "core".to_owned())].into_iter().collect()),
            match_expressions: None,
        };

        assert!(
            !matches_label_selector(&ns, &selector),
            "an unlabelled namespace cannot satisfy matchLabels"
        );
    }

    #[test]
    fn test_evaluate_match_expression_operators() {
        let labels: BTreeMap<String, String> = [("team".to_owned(), "core".to_owned())].into_iter().collect();

        assert!(
            evaluate_match_expression(&expression("team", "In", &["core", "infra"]), &labels),
            "In should match a listed value"
        );
        assert!(
            !evaluate_match_expression(&expression("team", "In", &["infra"]), &labels),
            "In should reject an unlisted value"
        );
        assert!(
            evaluate_match_expression(&expression("team", "NotIn", &["infra"]), &labels),
            "NotIn should accept an unlisted value"
        );
        assert!(
            evaluate_match_expression(&expression("team", "Exists", &[]), &labels),
            "Exists should match a present key"
        );
        assert!(
            evaluate_match_expression(&expression("tier", "DoesNotExist", &[]), &labels),
            "DoesNotExist should match an absent key"
        );
        assert!(
            !evaluate_match_expression(&expression("team", "Bogus", &[]), &labels),
            "an unknown operator must not match"
        );
    }
}
