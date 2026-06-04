// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! `ReferenceGrant` authorization checks for cross-namespace references.

use gateway_api::referencegrants::ReferenceGrant;

// -----------------------------------------------------------------------------
// ReferenceGrant Authorization
// -----------------------------------------------------------------------------

/// Checks if a cross-namespace reference is allowed by `ReferenceGrants`.
///
/// Returns `true` if the reference is within the same namespace or if a
/// `ReferenceGrant` permits the reference. The grant must match the `from`
/// (namespace, group, kind) and `to` (group, kind, optional name).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "params map 1:1 to Gateway API fields"
)]
pub(crate) fn is_reference_allowed(
    from_ns: &str,
    from_group: &str,
    from_kind: &str,
    to_ns: &str,
    to_group: &str,
    to_kind: &str,
    to_name: Option<&str>,
    grants: &[ReferenceGrant],
) -> bool {
    if from_ns == to_ns {
        return true;
    }

    for grant in grants {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        if grant_ns != to_ns {
            continue;
        }

        let from_matches = grant.spec.from.iter().any(|from| {
            let group_matches = from.group == from_group;
            let kind_matches = from.kind == from_kind;
            let ns_matches = from.namespace == from_ns;
            group_matches && kind_matches && ns_matches
        });

        if !from_matches {
            continue;
        }

        let to_matches = grant.spec.to.iter().any(|to| {
            let group_matches = to.group == to_group;
            let kind_matches = to.kind == to_kind;
            let name_matches = match (&to.name, to_name) {
                (Some(grant_name), Some(ref_name)) => grant_name == ref_name,
                (None, _) => true,
                (Some(_), None) => false,
            };
            group_matches && kind_matches && name_matches
        });

        if to_matches {
            return true;
        }
    }

    false
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
    use gateway_api::referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_same_namespace_always_allowed() {
        assert!(
            is_reference_allowed(
                "default",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "default",
                "",
                "Service",
                Some("backend"),
                &[]
            ),
            "same namespace should always be allowed"
        );
    }

    #[test]
    fn test_cross_namespace_without_grant_denied() {
        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[]
            ),
            "cross-namespace without grant should be denied"
        );
    }

    #[test]
    fn test_cross_namespace_with_matching_grant() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-httproutes".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant]
            ),
            "cross-namespace with matching grant should be allowed"
        );
    }

    #[test]
    fn test_cross_namespace_wrong_from_namespace() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-httproutes".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "other-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant]
            ),
            "wrong from namespace should be denied"
        );
    }

    #[test]
    fn test_cross_namespace_wrong_from_kind() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-gateways".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "Gateway".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant]
            ),
            "wrong from kind should be denied"
        );
    }

    #[test]
    fn test_cross_namespace_wrong_to_kind() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-httproutes".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Secret".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant]
            ),
            "wrong to kind should be denied"
        );
    }

    #[test]
    fn test_name_scoped_grant_allows_specific_name() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-specific".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: Some("allowed-backend".to_owned()),
                }],
            },
        };

        assert!(
            is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("allowed-backend"),
                &[grant]
            ),
            "name-scoped grant should allow specific name"
        );
    }

    #[test]
    fn test_name_scoped_grant_denies_other_name() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-specific".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: Some("allowed-backend".to_owned()),
                }],
            },
        };

        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("other-backend"),
                &[grant]
            ),
            "name-scoped grant should deny other names"
        );
    }

    #[test]
    fn test_grant_in_wrong_namespace() {
        let grant = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-httproutes".to_owned()),
                namespace: Some("other-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            !is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant]
            ),
            "grant in wrong namespace should not apply"
        );
    }

    #[test]
    fn test_multiple_grants_first_matches() {
        let grant1 = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-httproutes".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: "app-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        };

        let grant2 = ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("other-grant".to_owned()),
                namespace: Some("backend-ns".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "Gateway".to_owned(),
                    namespace: "other-ns".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Secret".to_owned(),
                    name: None,
                }],
            },
        };

        assert!(
            is_reference_allowed(
                "app-ns",
                "gateway.networking.k8s.io",
                "HTTPRoute",
                "backend-ns",
                "",
                "Service",
                Some("backend"),
                &[grant1, grant2]
            ),
            "should match first grant"
        );
    }
}
