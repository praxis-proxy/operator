// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Route attachment logic for `HTTPRoute` `parentRefs`.

use gateway_api::httproutes::{HTTPRoute, HttpRouteParentRefs};

// -----------------------------------------------------------------------------
// Route Attachment
// -----------------------------------------------------------------------------

/// Checks if a `HttpRouteParentRefs` matches the given `Gateway`.
///
/// The Gateway API spec defines defaults for `group` (`gateway.networking.k8s.io`),
/// `kind` (`Gateway`), and `namespace` (route's namespace). All fields must match
/// the target gateway to be considered attached.
pub(crate) fn parent_ref_matches_gateway(
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

/// Returns routes attached to the given Gateway with their section names.
///
/// Each tuple contains a route and a vector of section names (one per matching
/// parentRef). A `None` section name means the route attaches to all listeners.
pub(crate) fn attached_routes<'a>(
    gateway_name: &str,
    gateway_ns: &str,
    routes: &'a [HTTPRoute],
) -> Vec<(&'a HTTPRoute, Vec<Option<String>>)> {
    let mut result = Vec::new();

    for route in routes {
        let route_ns = route.metadata.namespace.as_deref().unwrap_or("default");

        if let Some(refs) = &route.spec.parent_refs {
            let mut section_names = Vec::new();
            for parent_ref in refs {
                if parent_ref_matches_gateway(parent_ref, gateway_name, gateway_ns, route_ns) {
                    section_names.push(parent_ref.section_name.clone());
                }
            }

            if !section_names.is_empty() {
                result.push((route, section_names));
            }
        }
    }

    result
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
        let attached = attached_routes("test-gateway", "default", &routes);
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

        let routes = vec![route];
        let attached = attached_routes("test-gateway", "default", &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(
            attached[0].0.metadata.name.as_deref(),
            Some("test-route"),
            "should match route name"
        );
        assert_eq!(attached[0].1.len(), 1, "should have one section name entry");
        assert_eq!(attached[0].1[0], None, "section name should be None");
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

        let routes = vec![route];
        let attached = attached_routes("test-gateway", "default", &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(attached[0].1[0], Some("https".to_owned()), "section name should match");
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

        let routes = vec![route];
        let attached = attached_routes("test-gateway", "default", &routes);

        assert_eq!(attached.len(), 1, "one route should be attached");
        assert_eq!(attached[0].1.len(), 2, "should have two section name entries");
        assert_eq!(
            attached[0].1[0],
            Some("http".to_owned()),
            "first section should be http"
        );
        assert_eq!(
            attached[0].1[1],
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

        let routes = vec![route];
        let attached = attached_routes("test-gateway", "default", &routes);

        assert!(attached.is_empty(), "no routes should be attached");
    }
}
