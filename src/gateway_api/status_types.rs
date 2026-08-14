// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Typed status documents.
//!
//! Every status the operator writes used to be assembled with `json!`
//! at the point of use, which put the Gateway API's field names into
//! string literals scattered across four modules. A typo in any of them
//! produces a patch the API server accepts and silently ignores — the
//! field simply never appears — so the failure surfaces as a Gateway
//! that never becomes Programmed rather than as an error.
//!
//! Declaring the documents once makes the field names a compile-time
//! concern. The merge and comparison logic in [`status`] keeps operating
//! on `Value`, because it has to handle whatever the API server already
//! holds and not merely what this operator writes; only construction
//! moves into types.
//!
//! [`status`]: super::status

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use serde::Serialize;
use serde_json::{Value, json};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// API version every Gateway API status patch declares.
const GATEWAY_API_VERSION: &str = "gateway.networking.k8s.io/v1";

/// API group owning Gateway API kinds.
pub const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

// -----------------------------------------------------------------------------
// Route Status
// -----------------------------------------------------------------------------

/// The parent a `status.parents` entry reports on.
///
/// Entry identity is this whole object: the merge in [`super::status`]
/// pairs a computed entry with a live one by comparing `parentRef`, so
/// what is serialized here has to match what the API server stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    /// API group of the parent, always the Gateway API group.
    pub group: String,

    /// Parent kind, always `Gateway`.
    pub kind: String,

    /// Parent Gateway name.
    pub name: String,

    /// Namespace holding the parent Gateway.
    pub namespace: String,

    /// Listener the route named, when it named one.
    ///
    /// Omitted rather than null: a `parentRef` without a `sectionName`
    /// targets every listener, and an explicit null would not compare
    /// equal to the absent field the API server stores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_name: Option<String>,
}

/// A `status.parents` entry on an `HTTPRoute`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteParentStatus {
    /// The parent this entry reports on.
    pub parent_ref: ParentReference,

    /// Controller that wrote the entry, used to tell writers apart.
    pub controller_name: String,

    /// Conditions this controller reports for the parent.
    pub conditions: Vec<Condition>,
}

// -----------------------------------------------------------------------------
// Gateway Status
// -----------------------------------------------------------------------------

/// A route kind a listener accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteGroupKind {
    /// API group of the accepted kind.
    pub group: String,

    /// The accepted kind.
    pub kind: String,
}

impl RouteGroupKind {
    /// Returns the only route kind this operator serves.
    pub fn httproute() -> Self {
        Self {
            group: GATEWAY_GROUP.to_owned(),
            kind: "HTTPRoute".to_owned(),
        }
    }
}

/// A `status.listeners` entry on a `Gateway`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerStatus {
    /// Listener name, matching `spec.listeners[].name`.
    pub name: String,

    /// Number of routes attached to this listener.
    pub attached_routes: usize,

    /// Route kinds the listener accepts.
    pub supported_kinds: Vec<RouteGroupKind>,

    /// Conditions reported for the listener.
    pub conditions: Vec<Condition>,
}

/// A `status.addresses` entry on a `Gateway`.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayAddress {
    /// Address family, always `IPAddress` here.
    #[serde(rename = "type")]
    pub kind: String,

    /// The address itself.
    pub value: String,
}

impl GatewayAddress {
    /// Returns an `IPAddress` entry for `ip`.
    pub fn ip(ip: &str) -> Self {
        Self {
            kind: "IPAddress".to_owned(),
            value: ip.to_owned(),
        }
    }
}

/// The `status` sub-object of a `Gateway`.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    /// Addresses the data plane is reachable on.
    pub addresses: Vec<GatewayAddress>,

    /// Gateway-level conditions.
    pub conditions: Vec<Condition>,

    /// Per-listener status entries.
    pub listeners: Vec<ListenerStatus>,
}

// -----------------------------------------------------------------------------
// GatewayClass Status
// -----------------------------------------------------------------------------

/// A `status.supportedFeatures` entry.
#[derive(Debug, Clone, Serialize)]
pub struct SupportedFeature {
    /// Conformance feature name.
    pub name: String,
}

/// The `status` sub-object of a `GatewayClass`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayClassStatus {
    /// Class-level conditions.
    pub conditions: Vec<Condition>,

    /// Conformance features this implementation claims.
    pub supported_features: Vec<SupportedFeature>,
}

// -----------------------------------------------------------------------------
// Apply Patches
// -----------------------------------------------------------------------------

/// Builds the server-side-apply body for a status write.
///
/// Every writer sends the same envelope: the object's identity plus its
/// `status`. An apply patch missing `apiVersion` or `kind` is rejected
/// outright, and one naming the wrong kind is applied to nothing, so
/// the four writers share one construction rather than four literals.
///
/// Pass `None` for `namespace` on cluster-scoped kinds.
pub fn status_patch(kind: &str, name: &str, namespace: Option<&str>, status: Value) -> Value {
    let metadata = match namespace {
        Some(ns) => json!({ "name": name, "namespace": ns }),
        None => json!({ "name": name }),
    };

    // Built by hand rather than with `json!` so `status` is moved in
    // rather than re-serialized: it is the largest value in the patch,
    // and the macro would borrow and clone it.
    let mut root = serde_json::Map::new();
    root.insert("apiVersion".to_owned(), json!(GATEWAY_API_VERSION));
    root.insert("kind".to_owned(), json!(kind));
    root.insert("metadata".to_owned(), metadata);
    root.insert("status".to_owned(), status);
    Value::Object(root)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal condition for serialization checks.
    fn condition() -> Condition {
        Condition {
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
            ),
            message: "ok".to_owned(),
            observed_generation: Some(1),
            reason: "Accepted".to_owned(),
            status: "True".to_owned(),
            type_: "Accepted".to_owned(),
        }
    }

    #[test]
    fn test_parent_reference_omits_an_absent_section_name() {
        let value = serde_json::to_value(ParentReference {
            group: GATEWAY_GROUP.to_owned(),
            kind: "Gateway".to_owned(),
            name: "gw".to_owned(),
            namespace: "infra".to_owned(),
            section_name: None,
        })
        .expect("a parent reference is plain strings");

        assert_eq!(
            value,
            json!({
                "group": "gateway.networking.k8s.io",
                "kind": "Gateway",
                "name": "gw",
                "namespace": "infra",
            }),
            "an explicit null sectionName would not compare equal to the absent field the API \
             server stores, and entry matching during merge is exact"
        );
    }

    #[test]
    fn test_parent_reference_carries_a_section_name_when_set() {
        let value = serde_json::to_value(ParentReference {
            group: GATEWAY_GROUP.to_owned(),
            kind: "Gateway".to_owned(),
            name: "gw".to_owned(),
            namespace: "infra".to_owned(),
            section_name: Some("https".to_owned()),
        })
        .expect("a parent reference is plain strings");

        assert_eq!(
            value.get("sectionName").and_then(Value::as_str),
            Some("https"),
            "a route naming a listener must report against that listener"
        );
    }

    #[test]
    fn test_listener_status_uses_the_camel_case_crd_field_names() {
        let value = serde_json::to_value(ListenerStatus {
            name: "http".to_owned(),
            attached_routes: 2,
            supported_kinds: vec![RouteGroupKind::httproute()],
            conditions: vec![condition()],
        })
        .expect("a listener status is strings, numbers, and conditions");

        assert_eq!(
            value.get("attachedRoutes").and_then(Value::as_u64),
            Some(2),
            "the CRD spells this attachedRoutes; a snake_case key would be silently dropped"
        );
        assert_eq!(
            value.get("supportedKinds"),
            Some(&json!([{ "group": "gateway.networking.k8s.io", "kind": "HTTPRoute" }])),
            "supportedKinds carries group and kind per entry"
        );
    }

    #[test]
    fn test_gateway_status_serializes_empty_lists_as_arrays() {
        let value = serde_json::to_value(GatewayStatus {
            addresses: vec![],
            conditions: vec![],
            listeners: vec![],
        })
        .expect("an empty status has nothing that can fail");

        assert_eq!(
            value,
            json!({ "addresses": [], "conditions": [], "listeners": [] }),
            "an absent list and an empty list are different documents to server-side apply"
        );
    }

    #[test]
    fn test_gateway_class_status_camel_cases_supported_features() {
        let value = serde_json::to_value(GatewayClassStatus {
            conditions: vec![condition()],
            supported_features: vec![SupportedFeature {
                name: "HTTPRoute".to_owned(),
            }],
        })
        .expect("a class status is strings and conditions");

        assert_eq!(
            value.get("supportedFeatures"),
            Some(&json!([{ "name": "HTTPRoute" }])),
            "conformance reads supportedFeatures to decide which suites to run"
        );
    }

    #[test]
    fn test_status_patch_includes_the_namespace_for_namespaced_kinds() {
        let patch = status_patch("HTTPRoute", "route", Some("apps"), json!({ "parents": [] }));

        assert_eq!(
            patch,
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": { "name": "route", "namespace": "apps" },
                "status": { "parents": [] },
            }),
            "an apply patch is rejected without apiVersion and kind"
        );
    }

    #[test]
    fn test_status_patch_omits_the_namespace_for_cluster_scoped_kinds() {
        let patch = status_patch("GatewayClass", "praxis", None, json!({ "conditions": [] }));

        assert_eq!(
            patch.get("metadata"),
            Some(&json!({ "name": "praxis" })),
            "a namespace on a cluster-scoped object is rejected by the API server"
        );
    }

    #[test]
    fn test_gateway_address_labels_ips_by_type() {
        let value = serde_json::to_value(GatewayAddress::ip("10.0.0.1")).expect("an address is two strings");

        assert_eq!(
            value,
            json!({ "type": "IPAddress", "value": "10.0.0.1" }),
            "the CRD field is `type`, which is a Rust keyword and so has to be renamed"
        );
    }
}
