// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Status-document reconciliation for Gateway API resources.
//!
//! Provides the two primitives every status writer needs before it
//! patches: carry `lastTransitionTime` forward across reconciles, and
//! recognise a computed document that the API server already holds.

use serde_json::{Map, Value};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Status field holding a list of Kubernetes conditions.
const CONDITIONS_KEY: &str = "conditions";

/// Condition field recording when the condition last flipped.
const TRANSITION_TIME_KEY: &str = "lastTransitionTime";

// -----------------------------------------------------------------------------
// Condition Transition Times
// -----------------------------------------------------------------------------

/// Carries `lastTransitionTime` forward from `observed` into `desired`.
///
/// Implements `meta.SetStatusCondition` semantics: a condition keeps the
/// transition time already recorded for as long as its `status` is
/// unchanged, and is restamped only on a genuine flip. Call this on the
/// `status` sub-object of a patch, before deciding whether to write it.
///
/// Without this step every reconcile produces a document that differs
/// only by timestamp, so each write re-triggers the controller's own
/// watch and the loop never settles.
pub fn preserve_condition_times(desired: &mut Value, observed: &Value) {
    if let (Some(desired), Some(observed)) = (desired.as_object_mut(), observed.as_object()) {
        preserve_in_object(desired, observed);
        return;
    }

    if let (Some(desired), Some(observed)) = (desired.as_array_mut(), observed.as_array()) {
        preserve_in_array(desired, observed);
    }
}

/// Returns `true` when every field of `desired` already matches the live
/// object.
///
/// Server-side apply only ever writes the fields the operator sets, so a
/// document whose fields all match the live status would be a no-op
/// patch. Fields the operator does not set are ignored at every depth,
/// and an absent field counts as matching when the desired value is an
/// empty list, which the API server may store by omission.
pub fn is_status_unchanged(desired: &Value, observed: &Value) -> bool {
    if let (Some(desired), Some(observed)) = (desired.as_object(), observed.as_object()) {
        return desired.iter().all(|(key, value)| field_unchanged(observed, key, value));
    }

    if let (Some(desired), Some(observed)) = (desired.as_array(), observed.as_array()) {
        return desired.len() == observed.len()
            && desired
                .iter()
                .zip(observed)
                .all(|(entry, previous)| is_status_unchanged(entry, previous));
    }

    desired == observed
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Recurses into every key of `desired` that `observed` also carries.
fn preserve_in_object(desired: &mut Map<String, Value>, observed: &Map<String, Value>) {
    for (key, value) in desired.iter_mut() {
        let Some(previous) = observed.get(key) else {
            continue;
        };

        if key == CONDITIONS_KEY {
            preserve_condition_list(value, previous);
        } else {
            preserve_condition_times(value, previous);
        }
    }
}

/// Pairs array entries by identity and recurses into each pair.
fn preserve_in_array(desired: &mut [Value], observed: &[Value]) {
    for entry in desired {
        let Some(key) = element_key(entry).cloned() else {
            continue;
        };
        let Some(previous) = observed.iter().find(|other| element_key(other) == Some(&key)) else {
            continue;
        };

        preserve_condition_times(entry, previous);
    }
}

/// Copies transition times onto conditions whose `status` is unchanged.
fn preserve_condition_list(desired: &mut Value, observed: &Value) {
    let (Some(desired), Some(observed)) = (desired.as_array_mut(), observed.as_array()) else {
        return;
    };

    for condition in desired {
        let Some(previous) = matching_condition(observed, condition) else {
            continue;
        };
        let Some(time) = previous.get(TRANSITION_TIME_KEY).cloned() else {
            continue;
        };

        if let Some(object) = condition.as_object_mut() {
            object.insert(TRANSITION_TIME_KEY.to_owned(), time);
        }
    }
}

/// Finds the observed condition sharing a `type` and `status` with
/// `desired`.
fn matching_condition<'a>(observed: &'a [Value], desired: &Value) -> Option<&'a Value> {
    let type_ = desired.get("type")?;
    let status = desired.get("status")?;

    observed
        .iter()
        .find(|other| other.get("type") == Some(type_) && other.get("status") == Some(status))
}

/// Returns the field identifying an entry within a status array.
///
/// Listener statuses are keyed by `name`, route parent statuses by
/// `parentRef`.
fn element_key(value: &Value) -> Option<&Value> {
    value.get("name").or_else(|| value.get("parentRef"))
}

/// Returns `true` when the live object already satisfies one desired
/// field.
fn field_unchanged(observed: &Map<String, Value>, key: &str, desired: &Value) -> bool {
    observed.get(key).map_or_else(
        || is_empty_list(desired),
        |previous| is_status_unchanged(desired, previous),
    )
}

/// Returns `true` for an empty list, which the API server omits rather
/// than stores.
fn is_empty_list(value: &Value) -> bool {
    value.as_array().is_some_and(Vec::is_empty)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_preserve_carries_time_forward_when_status_unchanged() {
        let mut desired = json!({
            "conditions": [{
                "type": "Accepted", "status": "True", "reason": "Accepted",
                "message": "ok", "observedGeneration": 1,
                "lastTransitionTime": "2026-08-08T00:00:10Z",
            }],
        });
        let observed = json!({
            "conditions": [{
                "type": "Accepted", "status": "True", "reason": "Accepted",
                "message": "ok", "observedGeneration": 1,
                "lastTransitionTime": "2026-08-08T00:00:00Z",
            }],
        });

        preserve_condition_times(&mut desired, &observed);

        assert_eq!(
            desired, observed,
            "unchanged condition status should reuse the observed transition time"
        );
    }

    #[test]
    fn test_preserve_restamps_when_status_flips() {
        let mut desired = json!({
            "conditions": [{
                "type": "Accepted", "status": "False",
                "lastTransitionTime": "2026-08-08T00:00:10Z",
            }],
        });
        let observed = json!({
            "conditions": [{
                "type": "Accepted", "status": "True",
                "lastTransitionTime": "2026-08-08T00:00:00Z",
            }],
        });

        preserve_condition_times(&mut desired, &observed);

        assert_eq!(
            desired["conditions"][0]["lastTransitionTime"], "2026-08-08T00:00:10Z",
            "a status flip must keep the freshly stamped transition time"
        );
    }

    #[test]
    fn test_preserve_recurses_into_listener_statuses_by_name() {
        let mut desired = json!({
            "listeners": [{
                "name": "http",
                "conditions": [{
                    "type": "Programmed", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:10Z",
                }],
            }],
        });
        let observed = json!({
            "listeners": [{
                "name": "http",
                "conditions": [{
                    "type": "Programmed", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:00Z",
                }],
            }],
        });

        preserve_condition_times(&mut desired, &observed);

        assert_eq!(
            desired["listeners"][0]["conditions"][0]["lastTransitionTime"], "2026-08-08T00:00:00Z",
            "nested listener conditions should carry their transition time forward"
        );
    }

    #[test]
    fn test_preserve_recurses_into_route_parents_by_parent_ref() {
        let parent_ref = json!({ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": "gw" });
        let mut desired = json!({
            "parents": [{
                "parentRef": parent_ref,
                "conditions": [{
                    "type": "Accepted", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:10Z",
                }],
            }],
        });
        let observed = json!({
            "parents": [{
                "parentRef": parent_ref,
                "conditions": [{
                    "type": "Accepted", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:00Z",
                }],
            }],
        });

        preserve_condition_times(&mut desired, &observed);

        assert_eq!(
            desired["parents"][0]["conditions"][0]["lastTransitionTime"], "2026-08-08T00:00:00Z",
            "route parent conditions should be paired by parentRef"
        );
    }

    #[test]
    fn test_preserve_ignores_listeners_with_a_different_name() {
        let mut desired = json!({
            "listeners": [{
                "name": "https",
                "conditions": [{
                    "type": "Programmed", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:10Z",
                }],
            }],
        });
        let observed = json!({
            "listeners": [{
                "name": "http",
                "conditions": [{
                    "type": "Programmed", "status": "True",
                    "lastTransitionTime": "2026-08-08T00:00:00Z",
                }],
            }],
        });

        preserve_condition_times(&mut desired, &observed);

        assert_eq!(
            desired["listeners"][0]["conditions"][0]["lastTransitionTime"], "2026-08-08T00:00:10Z",
            "an unmatched listener name must not inherit another listener's time"
        );
    }

    #[test]
    fn test_unchanged_detects_identical_status() {
        let status = json!({ "conditions": [{ "type": "Accepted", "status": "True" }] });

        assert!(
            is_status_unchanged(&status, &status),
            "an identical document should not be patched"
        );
    }

    #[test]
    fn test_unchanged_ignores_fields_the_operator_does_not_write() {
        let desired = json!({ "conditions": [] });
        let observed = json!({ "conditions": [], "attachedListenerSets": 3 });

        assert!(
            is_status_unchanged(&desired, &observed),
            "fields owned by other managers should not force a write"
        );
    }

    #[test]
    fn test_unchanged_treats_absent_field_as_empty_list() {
        let desired = json!({ "addresses": [], "conditions": [] });
        let observed = json!({ "conditions": [] });

        assert!(
            is_status_unchanged(&desired, &observed),
            "an empty list the server omits should not force a write"
        );
    }

    #[test]
    fn test_unchanged_rejects_absent_non_empty_field() {
        let desired = json!({ "addresses": [{ "type": "IPAddress", "value": "10.0.0.1" }] });
        let observed = json!({ "conditions": [] });

        assert!(
            !is_status_unchanged(&desired, &observed),
            "a missing non-empty field must be written"
        );
    }

    #[test]
    fn test_unchanged_tolerates_an_absent_nested_empty_list() {
        let desired = json!({ "listeners": [{ "name": "http", "attachedRoutes": 0, "supportedKinds": [] }] });
        let observed = json!({ "listeners": [{ "name": "http", "attachedRoutes": 0 }] });

        assert!(
            is_status_unchanged(&desired, &observed),
            "an empty list omitted inside a listener entry should not force a write"
        );
    }

    #[test]
    fn test_unchanged_ignores_nested_fields_the_operator_does_not_write() {
        let desired = json!({ "listeners": [{ "name": "http", "attachedRoutes": 1 }] });
        let observed = json!({ "listeners": [{ "name": "http", "attachedRoutes": 1, "extra": true }] });

        assert!(
            is_status_unchanged(&desired, &observed),
            "a field added by another manager inside a listener should not force a write"
        );
    }

    #[test]
    fn test_unchanged_rejects_a_shorter_observed_list() {
        let desired = json!({ "conditions": [{ "type": "Accepted" }, { "type": "Programmed" }] });
        let observed = json!({ "conditions": [{ "type": "Accepted" }] });

        assert!(
            !is_status_unchanged(&desired, &observed),
            "a missing condition must be written"
        );
    }

    #[test]
    fn test_unchanged_rejects_missing_status() {
        let desired = json!({ "conditions": [] });

        assert!(
            !is_status_unchanged(&desired, &Value::Null),
            "a resource with no status yet must be written"
        );
    }

    #[test]
    fn test_unchanged_rejects_differing_condition_message() {
        let desired = json!({ "conditions": [{ "type": "Accepted", "status": "True", "message": "new" }] });
        let observed = json!({ "conditions": [{ "type": "Accepted", "status": "True", "message": "old" }] });

        assert!(
            !is_status_unchanged(&desired, &observed),
            "a changed condition message must be written"
        );
    }
}
