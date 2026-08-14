// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Shared `HTTPRoute` parent-status construction and writing.
//!
//! Both the Gateway and the `HTTPRoute` controller report on
//! `status.parents`. They share this module so the two never disagree
//! about a condition's reason or message, and so each merges into the
//! live list rather than replacing it.

use gateway_api::{
    httproutes::{HTTPRoute, HttpRouteParentRefs, HttpRouteRulesBackendRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{api::core::v1::Service, apimachinery::pkg::apis::meta::v1::Condition};
use kube::{
    Api, ResourceExt as _,
    api::{Patch, PatchParams},
};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    context::CONTROLLER_NAME,
    error::Result,
    gateway_api::{conditions, reference_grant, status},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Field manager used for every server-side apply the operator issues.
const FIELD_MANAGER: &str = "praxis-operator";

/// API group owning `Gateway` and `HTTPRoute`.
const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

/// Namespace assumed for routes whose metadata carries none.
const DEFAULT_NAMESPACE: &str = "default";

// -----------------------------------------------------------------------------
// Parent Ref Inspection
// -----------------------------------------------------------------------------

/// Returns the namespace of an [`HTTPRoute`], defaulting to `"default"`.
pub(crate) fn route_namespace(route: &HTTPRoute) -> &str {
    route.metadata.namespace.as_deref().unwrap_or(DEFAULT_NAMESPACE)
}

/// Returns `true` when a `parentRef` targets a `Gateway` resource.
pub(crate) fn is_gateway_parent_ref(parent_ref: &HttpRouteParentRefs) -> bool {
    let group = parent_ref.group.as_deref().unwrap_or(GATEWAY_GROUP);
    let kind = parent_ref.kind.as_deref().unwrap_or("Gateway");
    group == GATEWAY_GROUP && kind == "Gateway"
}

/// Returns `true` when `parent_ref` targets the named Gateway.
pub(crate) fn is_ref_targeting_gateway(
    parent_ref: &HttpRouteParentRefs,
    gw_name: &str,
    gw_ns: &str,
    route_ns: &str,
) -> bool {
    if !is_gateway_parent_ref(parent_ref) {
        return false;
    }

    let ref_ns = parent_ref.namespace.as_deref().unwrap_or(route_ns);
    parent_ref.name == gw_name && ref_ns == gw_ns
}

// -----------------------------------------------------------------------------
// Backend Resolution
// -----------------------------------------------------------------------------

/// Reason a backend ref could not be resolved.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResolveFailure {
    /// Unsupported group or kind.
    InvalidKind,

    /// Cross-namespace ref denied by [`ReferenceGrant`].
    RefNotPermitted,

    /// Backend `Service` does not exist.
    BackendNotFound,
}

/// Outcome of checking every backend ref in a route.
pub(crate) type ResolveResult = std::result::Result<(), ResolveFailure>;

/// Checks all backend refs in a route for validity.
pub(crate) async fn check_backend_refs(
    route: &HTTPRoute,
    route_ns: &str,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    let Some(rules) = &route.spec.rules else {
        return Ok(());
    };

    for rule in rules {
        let Some(backends) = &rule.backend_refs else {
            continue;
        };
        for backend in backends {
            validate_backend(backend, route_ns, client, grants).await?;
        }
    }
    Ok(())
}

/// Builds the `ResolvedRefs` condition from a resolution outcome.
pub(crate) fn resolved_refs_condition(result: &ResolveResult, generation: i64) -> Condition {
    match result {
        Ok(()) => conditions::resolved_refs(generation, "all backend refs resolved"),
        Err(ResolveFailure::InvalidKind) => {
            conditions::unresolved_refs(generation, "InvalidKind", "unsupported backend ref kind")
        },
        Err(ResolveFailure::RefNotPermitted) => conditions::unresolved_refs(
            generation,
            "RefNotPermitted",
            "cross-namespace backend ref not permitted",
        ),
        Err(ResolveFailure::BackendNotFound) => {
            conditions::unresolved_refs(generation, "BackendNotFound", "backend service not found")
        },
    }
}

/// Rejects backend refs that are not `core/Service`.
pub(crate) fn validate_backend_kind(backend: &HttpRouteRulesBackendRefs) -> ResolveResult {
    let group = backend.group.as_deref().unwrap_or("");
    let kind = backend.kind.as_deref().unwrap_or("Service");
    if !group.is_empty() || kind != "Service" {
        debug!(group, kind, "unsupported backend ref kind");
        return Err(ResolveFailure::InvalidKind);
    }
    Ok(())
}

/// Rejects cross-namespace refs not covered by a [`ReferenceGrant`].
pub(crate) fn validate_cross_namespace(
    backend: &HttpRouteRulesBackendRefs,
    route_ns: &str,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    if backend_ns == route_ns {
        return Ok(());
    }

    if reference_grant::is_reference_allowed(
        route_ns,
        GATEWAY_GROUP,
        "HTTPRoute",
        backend_ns,
        "",
        "Service",
        Some(&backend.name),
        grants,
    ) {
        return Ok(());
    }

    debug!(
        backend_ns,
        service = %backend.name,
        "cross-namespace backend ref not permitted by ReferenceGrant"
    );
    Err(ResolveFailure::RefNotPermitted)
}

// -----------------------------------------------------------------------------
// Parent Status Documents
// -----------------------------------------------------------------------------

/// Builds the `status.parents` entry for a single `parentRef`.
pub(crate) fn parent_status_json(
    parent_ref: &HttpRouteParentRefs,
    gw_ns: &str,
    accepted: &Condition,
    resolved: &Condition,
) -> Value {
    parent_status_with_conditions(parent_ref, gw_ns, &[accepted.clone(), resolved.clone()])
}

/// Builds the `status.parents` entry from an explicit condition list.
///
/// Used when a route carries more than the usual `Accepted` and
/// `ResolvedRefs` pair, such as a `PartiallyInvalid` route.
pub(crate) fn parent_status_with_conditions(
    parent_ref: &HttpRouteParentRefs,
    gw_ns: &str,
    conditions: &[Condition],
) -> Value {
    let mut ref_json = json!({
        "group": GATEWAY_GROUP,
        "kind": "Gateway",
        "name": parent_ref.name,
        "namespace": gw_ns,
    });

    if let Some(section) = &parent_ref.section_name
        && let Some(object) = ref_json.as_object_mut()
    {
        object.insert("sectionName".to_owned(), json!(section));
    }

    json!({
        "parentRef": ref_json,
        "controllerName": CONTROLLER_NAME,
        "conditions": conditions,
    })
}

/// Merges `computed` parent entries into the route's live status and
/// patches it when the result differs.
///
/// A writer owns only the parentRefs it computed on this pass. Entries
/// for every other parent — including those written by the other
/// controller — are preserved in place, so the two writers no longer
/// overwrite each other's list.
pub(crate) async fn apply_parent_statuses(client: &kube::Client, route: &HTTPRoute, computed: &[Value]) -> Result<()> {
    let ns = route_namespace(route);
    let name = route.name_any();

    let observed = json!({ "parents": observed_parents(route)? });
    let mut desired = json!({ "parents": merge_parent_statuses(&observed, computed) });
    status::preserve_condition_times(&mut desired, &observed);

    if status::is_status_unchanged(&desired, &observed) {
        debug!("HTTPRoute {ns}/{name} parent status unchanged, skipping patch");
        return Ok(());
    }

    let payload = json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": { "name": name, "namespace": ns },
        "status": desired,
    });

    Api::<HTTPRoute>::namespaced(client.clone(), ns)
        .patch_status(
            &name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&payload),
        )
        .await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Validates a single backend ref.
async fn validate_backend(
    backend: &HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
    grants: &[ReferenceGrant],
) -> ResolveResult {
    validate_backend_kind(backend)?;
    validate_cross_namespace(backend, route_ns, grants)?;
    validate_service_exists(backend, route_ns, client).await
}

/// Verifies the referenced `Service` exists in the cluster.
async fn validate_service_exists(
    backend: &HttpRouteRulesBackendRefs,
    route_ns: &str,
    client: &kube::Client,
) -> ResolveResult {
    let backend_ns = backend.namespace.as_deref().unwrap_or(route_ns);
    let svc_api = Api::<Service>::namespaced(client.clone(), backend_ns);

    if svc_api.get(&backend.name).await.is_ok() {
        Ok(())
    } else {
        Err(ResolveFailure::BackendNotFound)
    }
}

/// Returns the route's live `status.parents` entries as JSON.
fn observed_parents(route: &HTTPRoute) -> Result<Vec<Value>> {
    let Some(status) = route.status.as_ref() else {
        return Ok(Vec::new());
    };

    let parents = serde_json::to_value(&status.parents)?;
    Ok(parents.as_array().cloned().unwrap_or_default())
}

/// Replaces observed entries the caller recomputed, keeping all others.
fn merge_parent_statuses(observed: &Value, computed: &[Value]) -> Vec<Value> {
    let observed = observed
        .get("parents")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);

    let mut merged: Vec<Value> = observed
        .iter()
        .map(|entry| find_by_parent_ref(computed, entry).unwrap_or(entry).clone())
        .collect();

    merged.extend(
        computed
            .iter()
            .filter(|entry| find_by_parent_ref(observed, entry).is_none())
            .cloned(),
    );
    merged
}

/// Finds the entry in `entries` describing the same `parentRef` as
/// `target`.
fn find_by_parent_ref<'a>(entries: &'a [Value], target: &Value) -> Option<&'a Value> {
    let key = target.get("parentRef")?;
    entries.iter().find(|entry| entry.get("parentRef") == Some(key))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::allow_attributes,
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
    use gateway_api::{
        httproutes::{HttpRouteSpec, HttpRouteStatus, HttpRouteStatusParents, HttpRouteStatusParentsParentRef},
        referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo},
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    #[test]
    fn test_is_gateway_parent_ref_defaults_to_gateway() {
        assert!(
            is_gateway_parent_ref(&parent_ref("gw", None)),
            "an unqualified parentRef defaults to a Gateway in the Gateway API group"
        );
    }

    #[test]
    fn test_is_gateway_parent_ref_rejects_other_kinds() {
        let mut reference = parent_ref("svc", None);
        reference.kind = Some("Service".to_owned());

        assert!(
            !is_gateway_parent_ref(&reference),
            "a Service parentRef is not a Gateway"
        );
    }

    #[test]
    fn test_is_gateway_parent_ref_rejects_other_groups() {
        let mut reference = parent_ref("gw", None);
        reference.group = Some("example.com".to_owned());

        assert!(
            !is_gateway_parent_ref(&reference),
            "a parentRef outside the Gateway API group is not a Gateway"
        );
    }

    #[test]
    fn test_is_ref_targeting_gateway_same_namespace() {
        assert!(
            is_ref_targeting_gateway(&parent_ref("gw", None), "gw", "apps", "apps"),
            "a parentRef without a namespace targets the route's own namespace"
        );
    }

    #[test]
    fn test_is_ref_targeting_gateway_cross_namespace() {
        let reference = parent_ref("gw", Some("infra"));

        assert!(
            is_ref_targeting_gateway(&reference, "gw", "infra", "apps"),
            "an explicit namespace should be honoured"
        );
        assert!(
            !is_ref_targeting_gateway(&reference, "gw", "apps", "apps"),
            "a parentRef naming another namespace must not match"
        );
    }

    #[test]
    fn test_is_ref_targeting_gateway_rejects_other_names() {
        assert!(
            !is_ref_targeting_gateway(&parent_ref("other", None), "gw", "apps", "apps"),
            "a parentRef naming a different Gateway must not match"
        );
    }

    #[test]
    fn test_resolved_refs_condition_ok() {
        let condition = resolved_refs_condition(&Ok(()), 7);

        assert_eq!(condition.type_, "ResolvedRefs", "type should be ResolvedRefs");
        assert_eq!(condition.status, "True", "a clean resolution is True");
        assert_eq!(condition.observed_generation, Some(7), "generation should be carried");
    }

    #[test]
    fn test_resolved_refs_condition_failures_map_to_reasons() {
        let cases = [
            (ResolveFailure::InvalidKind, "InvalidKind"),
            (ResolveFailure::RefNotPermitted, "RefNotPermitted"),
            (ResolveFailure::BackendNotFound, "BackendNotFound"),
        ];

        for (failure, reason) in cases {
            let condition = resolved_refs_condition(&Err(failure), 1);
            assert_eq!(condition.status, "False", "a failed resolution is False");
            assert_eq!(condition.reason, reason, "failure should map to its Gateway API reason");
        }
    }

    #[test]
    fn test_validate_backend_kind_accepts_implicit_service() {
        assert_eq!(
            validate_backend_kind(&backend_ref("svc", None, None)),
            Ok(()),
            "an unqualified backendRef defaults to a core Service"
        );
    }

    #[test]
    fn test_validate_backend_kind_rejects_non_service() {
        let backend = HttpRouteRulesBackendRefs {
            kind: Some("Pod".to_owned()),
            ..backend_ref("p", None, None)
        };

        assert_eq!(
            validate_backend_kind(&backend),
            Err(ResolveFailure::InvalidKind),
            "a non-Service backendRef is unsupported"
        );
    }

    #[test]
    fn test_validate_backend_kind_rejects_non_core_group() {
        let backend = HttpRouteRulesBackendRefs {
            group: Some("example.com".to_owned()),
            ..backend_ref("svc", None, None)
        };

        assert_eq!(
            validate_backend_kind(&backend),
            Err(ResolveFailure::InvalidKind),
            "a backendRef outside the core group is unsupported"
        );
    }

    #[test]
    fn test_validate_cross_namespace_same_namespace_needs_no_grant() {
        assert_eq!(
            validate_cross_namespace(&backend_ref("svc", None, None), "apps", &[]),
            Ok(()),
            "a same-namespace backendRef never needs a ReferenceGrant"
        );
    }

    #[test]
    fn test_validate_cross_namespace_without_grant_is_denied() {
        assert_eq!(
            validate_cross_namespace(&backend_ref("svc", Some("data"), None), "apps", &[]),
            Err(ResolveFailure::RefNotPermitted),
            "a cross-namespace backendRef without a grant must be denied"
        );
    }

    #[test]
    fn test_validate_cross_namespace_with_grant_is_allowed() {
        let grants = [grant("data", "apps")];

        assert_eq!(
            validate_cross_namespace(&backend_ref("svc", Some("data"), None), "apps", &grants),
            Ok(()),
            "a matching ReferenceGrant should permit the backendRef"
        );
    }

    #[test]
    fn test_parent_status_json_shape() {
        let accepted = conditions::accepted(1, "route accepted");
        let resolved = conditions::resolved_refs(1, "all backend refs resolved");
        let entry = parent_status_json(&parent_ref("gw", None), "infra", &accepted, &resolved);

        assert_eq!(entry["parentRef"]["name"], "gw", "parentRef should name the Gateway");
        assert_eq!(
            entry["parentRef"]["namespace"], "infra",
            "parentRef should carry the Gateway namespace"
        );
        assert_eq!(
            entry["controllerName"], CONTROLLER_NAME,
            "the operator must claim the entry it writes"
        );
        assert_eq!(
            entry["conditions"].as_array().map(Vec::len),
            Some(2),
            "both Accepted and ResolvedRefs should be written"
        );
    }

    #[test]
    fn test_parent_status_json_includes_section_name() {
        let accepted = conditions::accepted(1, "route accepted");
        let resolved = conditions::resolved_refs(1, "all backend refs resolved");
        let mut reference = parent_ref("gw", None);
        reference.section_name = Some("https".to_owned());

        let entry = parent_status_json(&reference, "infra", &accepted, &resolved);

        assert_eq!(
            entry["parentRef"]["sectionName"], "https",
            "an explicit sectionName should be reported back"
        );
    }

    #[test]
    fn test_merge_replaces_only_recomputed_parents() {
        let mine = status_entry("gw-a", "True");
        let theirs = status_entry("gw-b", "False");
        let observed = json!({ "parents": [mine.clone(), theirs.clone()] });
        let recomputed = status_entry("gw-a", "False");

        let merged = merge_parent_statuses(&observed, std::slice::from_ref(&recomputed));

        assert_eq!(
            merged,
            vec![recomputed, theirs],
            "only the recomputed parentRef should change; the other writer's entry must survive"
        );
    }

    #[test]
    fn test_merge_appends_new_parents() {
        let existing = status_entry("gw-a", "True");
        let observed = json!({ "parents": [existing.clone()] });
        let fresh = status_entry("gw-b", "True");

        let merged = merge_parent_statuses(&observed, std::slice::from_ref(&fresh));

        assert_eq!(
            merged,
            vec![existing, fresh],
            "an unseen parentRef should be appended without disturbing existing entries"
        );
    }

    #[test]
    fn test_merge_of_empty_computed_is_identity() {
        let existing = status_entry("gw-a", "True");
        let observed = json!({ "parents": [existing.clone()] });

        assert_eq!(
            merge_parent_statuses(&observed, &[]),
            vec![existing],
            "computing nothing must not erase the live status"
        );
    }

    #[test]
    fn test_observed_parents_of_route_without_status() {
        let route = route_with_status(None);

        assert_eq!(
            observed_parents(&route).unwrap(),
            Vec::<Value>::new(),
            "a route with no status has no parent entries"
        );
    }

    #[test]
    fn test_observed_parents_round_trips_parent_ref() {
        let route = route_with_status(Some(HttpRouteStatus {
            parents: vec![HttpRouteStatusParents {
                conditions: vec![conditions::accepted(1, "route accepted")],
                controller_name: CONTROLLER_NAME.to_owned(),
                parent_ref: HttpRouteStatusParentsParentRef {
                    group: Some(GATEWAY_GROUP.to_owned()),
                    kind: Some("Gateway".to_owned()),
                    name: "gw".to_owned(),
                    namespace: Some("infra".to_owned()),
                    ..Default::default()
                },
            }],
        }));

        let parents = observed_parents(&route).unwrap();

        assert_eq!(parents.len(), 1, "one live parent entry should be read back");
        assert_eq!(
            parents[0]["parentRef"]["name"], "gw",
            "the live parentRef should survive the JSON round trip"
        );
        assert_eq!(
            parents[0]["controllerName"], CONTROLLER_NAME,
            "the controller name should survive the JSON round trip"
        );
    }

    #[test]
    fn test_route_namespace_defaults() {
        assert_eq!(
            route_namespace(&route_with_status(None)),
            "apps",
            "an explicit namespace should be returned"
        );

        let bare = HTTPRoute {
            metadata: ObjectMeta::default(),
            spec: HttpRouteSpec::default(),
            status: None,
        };
        assert_eq!(
            route_namespace(&bare),
            "default",
            "a route without a namespace falls back to default"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Builds a `parentRef` naming `name` in an optional namespace.
    fn parent_ref(name: &str, namespace: Option<&str>) -> HttpRouteParentRefs {
        HttpRouteParentRefs {
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            ..Default::default()
        }
    }

    /// Builds a `backendRef` naming `name` in an optional namespace.
    fn backend_ref(name: &str, namespace: Option<&str>, port: Option<i32>) -> HttpRouteRulesBackendRefs {
        HttpRouteRulesBackendRefs {
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            port,
            ..Default::default()
        }
    }

    /// Builds a `ReferenceGrant` in `grant_ns` trusting `HTTPRoutes` from
    /// `from_ns` to reach any Service.
    fn grant(grant_ns: &str, from_ns: &str) -> ReferenceGrant {
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow".to_owned()),
                namespace: Some(grant_ns.to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: GATEWAY_GROUP.to_owned(),
                    kind: "HTTPRoute".to_owned(),
                    namespace: from_ns.to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Service".to_owned(),
                    name: None,
                }],
            },
        }
    }

    /// Builds a minimal parent status entry for `gateway`.
    fn status_entry(gateway: &str, accepted: &str) -> Value {
        json!({
            "parentRef": { "group": GATEWAY_GROUP, "kind": "Gateway", "name": gateway, "namespace": "infra" },
            "controllerName": CONTROLLER_NAME,
            "conditions": [{ "type": "Accepted", "status": accepted }],
        })
    }

    /// Builds an `HTTPRoute` in the `apps` namespace with the given status.
    fn route_with_status(status: Option<HttpRouteStatus>) -> HTTPRoute {
        HTTPRoute {
            metadata: ObjectMeta {
                name: Some("route".to_owned()),
                namespace: Some("apps".to_owned()),
                ..Default::default()
            },
            spec: HttpRouteSpec::default(),
            status,
        }
    }
}
