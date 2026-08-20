// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Route attachment logic for `HTTPRoute` `parentRefs`.

use std::sync::Arc;

use gateway_api::{
    gateways::GatewayListeners,
    httproutes::{HTTPRoute, HttpRouteParentRefs},
};

// -----------------------------------------------------------------------------
// AttachedRoute
// -----------------------------------------------------------------------------

/// A route bound to a Gateway, with the listeners it targets.
///
/// A route may name the same Gateway more than once, so `section_names`
/// carries one entry per matching `parentRef`. A `None` entry means that
/// ref named no `sectionName` and therefore targets every listener.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachedRoute<'route> {
    /// The route itself.
    pub route: &'route HTTPRoute,

    /// Listener section names this route targets.
    pub section_names: Vec<Option<String>>,
}

impl AttachedRoute<'_> {
    /// Returns whether the route targets the named listener.
    ///
    /// A ref without a `sectionName` targets every listener, so it
    /// matches whatever name is asked about.
    pub fn targets_listener(&self, listener: &str) -> bool {
        self.section_names
            .iter()
            .any(|section| section.as_deref().is_none_or(|name| name == listener))
    }
}

// -----------------------------------------------------------------------------
// Route Attachment
// -----------------------------------------------------------------------------

/// Checks if a `HttpRouteParentRefs` matches the given `Gateway`.
///
/// The Gateway API spec defines defaults for `group` (`gateway.networking.k8s.io`),
/// `kind` (`Gateway`), and `namespace` (route's namespace). All fields must match
/// the target gateway to be considered attached.
pub fn parent_ref_matches_gateway(
    parent: &HttpRouteParentRefs,
    gateway_name: &str,
    gateway_ns: &str,
    route_ns: &str,
) -> bool {
    let group = parent.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    let kind = parent.kind.as_deref().unwrap_or("Gateway");
    let namespace = parent.namespace.as_deref().unwrap_or(route_ns);

    group == "gateway.networking.k8s.io" && kind == "Gateway" && parent.name == gateway_name && namespace == gateway_ns
}

/// Returns `true` when a listener satisfies everything a `parentRef`
/// asks of the listener it attaches to.
///
/// `sectionName` and `port` are both optional and both narrowing, and
/// the Gateway API applies them together rather than as alternatives:
/// a ref carrying each selects the listener satisfying both, and
/// selects nothing when no listener does.
pub fn listener_matches_parent_ref(listener: &GatewayListeners, parent: &HttpRouteParentRefs) -> bool {
    parent.section_name.as_ref().is_none_or(|name| listener.name == *name)
        && parent.port.is_none_or(|port| listener.port == port)
}

/// Returns the listeners a `parentRef` attaches to, by section name.
///
/// A ref naming a `sectionName` targets that listener alone. A ref
/// naming only a `port` targets every listener serving that port — a
/// Gateway may have several, told apart by hostname. A ref naming
/// neither targets all of them, which is spelled `None` so that a
/// route does not have to be re-derived when listeners change.
fn targeted_sections(parent: &HttpRouteParentRefs, listeners: &[GatewayListeners]) -> Vec<Option<String>> {
    if parent.section_name.is_none() && parent.port.is_none() {
        return vec![None];
    }

    listeners
        .iter()
        .filter(|listener| listener_matches_parent_ref(listener, parent))
        .map(|listener| Some(listener.name.clone()))
        .collect()
}

/// Returns routes attached to the given Gateway with their section names.
///
/// Each entry pairs a route with the listener section names its
/// `parentRefs` resolve to. A `None` section name means the route
/// attaches to all listeners.
pub fn attached_routes<'route>(
    gateway_name: &str,
    gateway_ns: &str,
    listeners: &[GatewayListeners],
    routes: &'route [Arc<HTTPRoute>],
) -> Vec<AttachedRoute<'route>> {
    let mut result = Vec::new();

    for route in routes {
        let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");

        if let Some(refs) = &route.spec.parent_refs {
            let mut section_names = Vec::new();
            for parent_ref in refs {
                if parent_ref_matches_gateway(parent_ref, gateway_name, gateway_ns, route_ns) {
                    section_names.extend(targeted_sections(parent_ref, listeners));
                }
            }

            if !section_names.is_empty() {
                result.push(AttachedRoute { route, section_names });
            }
        }
    }

    result
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::too_many_lines, reason = "tests")]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_parent_ref_matches_gateway_basic() {
        let parent = HttpRouteParentRefs {
            name: "test-gateway".to_owned(),
            ..Default::default()
        };

        assert!(
            parent_ref_matches_gateway(&parent, "test-gateway", "default", "default"),
            "should match with default group/kind and same namespace"
        );
    }

    #[test]
    fn test_parent_ref_matches_gateway_wrong_name() {
        let parent = HttpRouteParentRefs {
            name: "other-gateway".to_owned(),
            ..Default::default()
        };

        assert!(
            !parent_ref_matches_gateway(&parent, "test-gateway", "default", "default"),
            "should not match different gateway name"
        );
    }

    #[test]
    fn test_parent_ref_matches_gateway_cross_namespace() {
        let parent = HttpRouteParentRefs {
            name: "test-gateway".to_owned(),
            namespace: Some("gateway-ns".to_owned()),
            ..Default::default()
        };

        assert!(
            parent_ref_matches_gateway(&parent, "test-gateway", "gateway-ns", "route-ns"),
            "should match cross-namespace reference"
        );

        assert!(
            !parent_ref_matches_gateway(&parent, "test-gateway", "other-ns", "route-ns"),
            "should not match different namespace"
        );
    }

    #[test]
    fn test_parent_ref_matches_gateway_wrong_kind() {
        let parent = HttpRouteParentRefs {
            name: "test-gateway".to_owned(),
            kind: Some("Service".to_owned()),
            ..Default::default()
        };

        assert!(
            !parent_ref_matches_gateway(&parent, "test-gateway", "default", "default"),
            "should not match different kind"
        );
    }

    #[test]
    fn test_parent_ref_matches_gateway_wrong_group() {
        let parent = HttpRouteParentRefs {
            name: "test-gateway".to_owned(),
            group: Some("other.group".to_owned()),
            ..Default::default()
        };

        assert!(
            !parent_ref_matches_gateway(&parent, "test-gateway", "default", "default"),
            "should not match different group"
        );
    }

    #[test]
    fn test_attached_routes_none() {
        let routes = vec![];
        let attached = attached_routes("test-gateway", "default", &listeners(), &routes);
        assert!(attached.is_empty(), "no routes should be attached");
    }

    #[test]
    fn test_attached_routes_single() {
        use gateway_api::httproutes::HttpRouteSpec;

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "test-gateway".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![Arc::new(route)];
        let attached = attached_routes("test-gateway", "default", &listeners(), &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(
            attached[0].route.metadata.name.as_deref(),
            Some("test-route"),
            "should match route name"
        );
        assert_eq!(attached[0].section_names.len(), 1, "should have one section name entry");
        assert_eq!(attached[0].section_names[0], None, "section name should be None");
    }

    #[test]
    fn test_attached_routes_with_section_name() {
        use gateway_api::httproutes::HttpRouteSpec;

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "test-gateway".to_owned(),
                    section_name: Some("https".to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![Arc::new(route)];
        let attached = attached_routes("test-gateway", "default", &listeners(), &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(
            attached[0].section_names[0],
            Some("https".to_owned()),
            "section name should match"
        );
    }

    #[test]
    fn test_attached_routes_multiple_parent_refs() {
        use gateway_api::httproutes::HttpRouteSpec;

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![
                    HttpRouteParentRefs {
                        name: "test-gateway".to_owned(),
                        section_name: Some("http".to_owned()),
                        ..Default::default()
                    },
                    HttpRouteParentRefs {
                        name: "test-gateway".to_owned(),
                        section_name: Some("https".to_owned()),
                        ..Default::default()
                    },
                    HttpRouteParentRefs {
                        name: "other-gateway".to_owned(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![Arc::new(route)];
        let attached = attached_routes("test-gateway", "default", &listeners(), &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(
            attached[0].section_names.len(),
            2,
            "should have two section name entries"
        );
        assert_eq!(
            attached[0].section_names[0],
            Some("http".to_owned()),
            "first section should be http"
        );
        assert_eq!(
            attached[0].section_names[1],
            Some("https".to_owned()),
            "second section should be https"
        );
    }

    #[test]
    fn test_attached_routes_no_match() {
        use gateway_api::httproutes::HttpRouteSpec;

        let route = HTTPRoute {
            metadata: ObjectMeta {
                name: Some("test-route".to_owned()),
                namespace: Some("default".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec {
                parent_refs: Some(vec![HttpRouteParentRefs {
                    name: "other-gateway".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            status: None,
        };

        let routes = vec![Arc::new(route)];
        let attached = attached_routes("test-gateway", "default", &listeners(), &routes);

        assert!(attached.is_empty(), "no routes should be attached");
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds the listener set the attachment tests resolve against.
    fn listeners() -> Vec<GatewayListeners> {
        vec![
            GatewayListeners {
                name: "http".to_owned(),
                port: 80,
                protocol: "HTTP".to_owned(),
                ..Default::default()
            },
            GatewayListeners {
                name: "https".to_owned(),
                port: 443,
                protocol: "HTTPS".to_owned(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn test_a_port_only_parent_ref_targets_every_listener_on_it() {
        let listeners = vec![
            GatewayListeners {
                name: "foo".to_owned(),
                port: 8080,
                protocol: "HTTP".to_owned(),
                ..Default::default()
            },
            GatewayListeners {
                name: "bar".to_owned(),
                port: 8080,
                protocol: "HTTP".to_owned(),
                ..Default::default()
            },
            GatewayListeners {
                name: "other".to_owned(),
                port: 80,
                protocol: "HTTP".to_owned(),
                ..Default::default()
            },
        ];
        let parent = HttpRouteParentRefs {
            name: "gw".to_owned(),
            port: Some(8080),
            ..Default::default()
        };

        assert_eq!(
            targeted_sections(&parent, &listeners),
            vec![Some("foo".to_owned()), Some("bar".to_owned())],
            "a Gateway may serve one port from several listeners, told apart by hostname, and the \
             route attaches to all of them"
        );
    }

    #[test]
    fn test_a_parent_ref_naming_neither_stays_unresolved() {
        let parent = HttpRouteParentRefs {
            name: "gw".to_owned(),
            ..Default::default()
        };

        assert_eq!(
            targeted_sections(&parent, &listeners()),
            vec![None],
            "resolving it to today's listener names would leave the route bound to a stale set \
             the next time the Gateway gained one"
        );
    }
}
