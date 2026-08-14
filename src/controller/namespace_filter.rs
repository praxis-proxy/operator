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

use crate::{
    gateway_api::{attachment::AttachedRoute, route_status},
    stores::Stores,
};

// -----------------------------------------------------------------------------
// Namespace Filtering
// -----------------------------------------------------------------------------

/// Filters attached routes by the `allowedRoutes.namespaces` policy on
/// each listener.
///
/// A route is retained if at least one listener it targets allows its
/// namespace. The default policy (when unspecified) is `Same`.
pub(super) fn filter_routes_by_allowed_namespaces<'a>(
    attached: &[AttachedRoute<'a>],
    listeners: &[GatewayListeners],
    gateway_ns: &str,
    stores: &Stores,
) -> Vec<AttachedRoute<'a>> {
    let all_namespaces = Some(stores.namespaces());

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
pub(super) fn is_namespace_allowed(
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

    // -----------------------------------------------------------------------
    // Route Filtering
    // -----------------------------------------------------------------------

    #[test]
    fn test_a_route_from_a_disallowed_namespace_is_dropped() {
        let listeners = vec![same_namespace_listener()];
        let route = route_in("apps");
        let attached = vec![attached(&route)];
        let stores = Stores::fake(vec![], vec![], vec![]);

        assert!(
            filter_routes_by_allowed_namespaces(&attached, &listeners, "infra", &stores).is_empty(),
            "a Same-namespace listener must not serve a route from another namespace, and a route \
             the filter lets through reaches the generated config"
        );
    }

    #[test]
    fn test_a_route_from_the_gateway_namespace_is_kept() {
        let listeners = vec![same_namespace_listener()];
        let route = route_in("infra");
        let attached = vec![attached(&route)];
        let stores = Stores::fake(vec![], vec![], vec![]);

        assert_eq!(
            filter_routes_by_allowed_namespaces(&attached, &listeners, "infra", &stores).len(),
            1,
            "Same is the default policy and it allows the Gateway's own namespace"
        );
    }

    #[test]
    fn test_one_permissive_listener_is_enough() {
        let listeners = vec![same_namespace_listener(), all_namespaces_listener()];
        let route = route_in("apps");
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![None],
        }];
        let stores = Stores::fake(vec![], vec![], vec![]);

        assert_eq!(
            filter_routes_by_allowed_namespaces(&attached, &listeners, "infra", &stores).len(),
            1,
            "a parentRef naming no section targets every listener, so one that allows the route's \
             namespace admits it even while another refuses"
        );
    }

    #[test]
    fn test_a_section_name_confines_the_check_to_that_listener() {
        let listeners = vec![same_namespace_listener(), all_namespaces_listener()];
        let route = route_in("apps");
        let attached = vec![AttachedRoute {
            route: &route,
            section_names: vec![Some("same".to_owned())],
        }];
        let stores = Stores::fake(vec![], vec![], vec![]);

        assert!(
            filter_routes_by_allowed_namespaces(&attached, &listeners, "infra", &stores).is_empty(),
            "naming a listener means asking that listener, and the permissive one beside it does \
             not answer for it"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a listener refusing every namespace but the Gateway's.
    fn same_namespace_listener() -> GatewayListeners {
        GatewayListeners {
            name: "same".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            ..Default::default()
        }
    }

    /// Builds a listener admitting routes from anywhere.
    fn all_namespaces_listener() -> GatewayListeners {
        use gateway_api::gateways::{GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesNamespaces};

        GatewayListeners {
            name: "all".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            allowed_routes: Some(GatewayListenersAllowedRoutes {
                namespaces: Some(GatewayListenersAllowedRoutesNamespaces {
                    from: Some(GatewayListenersAllowedRoutesNamespacesFrom::All),
                    selector: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Builds a route living in `namespace`.
    fn route_in(namespace: &str) -> HTTPRoute {
        HTTPRoute {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: gateway_api::httproutes::HttpRouteSpec::default(),
            status: None,
        }
    }

    /// Attaches a route to every listener, as a `parentRef` with no
    /// `sectionName` does.
    fn attached(route: &HTTPRoute) -> AttachedRoute<'_> {
        AttachedRoute {
            route,
            section_names: vec![None],
        }
    }
}
