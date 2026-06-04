// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Condition builders for Gateway API status updates.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

// -----------------------------------------------------------------------------
// Condition Builders
// -----------------------------------------------------------------------------

/// Creates a Condition with the given type, status, reason, message, and
/// observedGeneration.
///
/// Sets `last_transition_time` to the current UTC timestamp.
pub(crate) fn make_condition(type_: &str, status: &str, reason: &str, message: &str, generation: i64) -> Condition {
    Condition {
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
        message: message.to_owned(),
        observed_generation: Some(generation),
        reason: reason.to_owned(),
        status: status.to_owned(),
        type_: type_.to_owned(),
    }
}

/// Returns an Accepted: True condition.
pub(crate) fn accepted(generation: i64, message: &str) -> Condition {
    make_condition("Accepted", "True", "Accepted", message, generation)
}

/// Returns an `Accepted: False` condition.
pub(crate) fn not_accepted(generation: i64, reason: &str, message: &str) -> Condition {
    make_condition("Accepted", "False", reason, message, generation)
}

/// Returns a `Programmed: True` condition.
pub(crate) fn programmed(generation: i64, message: &str) -> Condition {
    make_condition("Programmed", "True", "Programmed", message, generation)
}

/// Returns a `Programmed: False` condition.
pub(crate) fn not_programmed(generation: i64, reason: &str, message: &str) -> Condition {
    make_condition("Programmed", "False", reason, message, generation)
}

/// Returns a `ResolvedRefs: True` condition.
pub(crate) fn resolved_refs(generation: i64, message: &str) -> Condition {
    make_condition("ResolvedRefs", "True", "ResolvedRefs", message, generation)
}

/// Returns a `ResolvedRefs: False` condition.
pub(crate) fn unresolved_refs(generation: i64, reason: &str, message: &str) -> Condition {
    make_condition("ResolvedRefs", "False", reason, message, generation)
}

/// Returns a Conflicted: False condition indicating no conflicts.
pub(crate) fn no_conflicts(generation: i64) -> Condition {
    make_condition(
        "Conflicted",
        "False",
        "NoConflicts",
        "no conflicts detected",
        generation,
    )
}

/// Returns a `Conflicted: True` condition.
#[allow(dead_code, reason = "available for all condition variants")]
pub(crate) fn conflicted(generation: i64, reason: &str, message: &str) -> Condition {
    make_condition("Conflicted", "True", reason, message, generation)
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
    use super::*;

    #[test]
    fn test_make_condition() {
        let cond = make_condition("TestType", "TestStatus", "TestReason", "test message", 42);
        assert_eq!(cond.type_, "TestType");
        assert_eq!(cond.status, "TestStatus");
        assert_eq!(cond.reason, "TestReason");
        assert_eq!(cond.message, "test message");
        assert_eq!(cond.observed_generation, Some(42));
    }

    #[test]
    fn test_accepted() {
        let cond = accepted(10, "accepted message");
        assert_eq!(cond.type_, "Accepted");
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason, "Accepted");
        assert_eq!(cond.message, "accepted message");
        assert_eq!(cond.observed_generation, Some(10));
    }

    #[test]
    fn test_not_accepted() {
        let cond = not_accepted(11, "InvalidConfig", "config is invalid");
        assert_eq!(cond.type_, "Accepted");
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "InvalidConfig");
        assert_eq!(cond.message, "config is invalid");
        assert_eq!(cond.observed_generation, Some(11));
    }

    #[test]
    fn test_programmed() {
        let cond = programmed(12, "programmed successfully");
        assert_eq!(cond.type_, "Programmed");
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason, "Programmed");
        assert_eq!(cond.message, "programmed successfully");
        assert_eq!(cond.observed_generation, Some(12));
    }

    #[test]
    fn test_not_programmed() {
        let cond = not_programmed(13, "DeploymentFailed", "deployment not ready");
        assert_eq!(cond.type_, "Programmed");
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "DeploymentFailed");
        assert_eq!(cond.message, "deployment not ready");
        assert_eq!(cond.observed_generation, Some(13));
    }

    #[test]
    fn test_resolved_refs() {
        let cond = resolved_refs(14, "all refs resolved");
        assert_eq!(cond.type_, "ResolvedRefs");
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason, "ResolvedRefs");
        assert_eq!(cond.message, "all refs resolved");
        assert_eq!(cond.observed_generation, Some(14));
    }

    #[test]
    fn test_unresolved_refs() {
        let cond = unresolved_refs(15, "BackendNotFound", "backend not found");
        assert_eq!(cond.type_, "ResolvedRefs");
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "BackendNotFound");
        assert_eq!(cond.message, "backend not found");
        assert_eq!(cond.observed_generation, Some(15));
    }

    #[test]
    fn test_no_conflicts() {
        let cond = no_conflicts(16);
        assert_eq!(cond.type_, "Conflicted");
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "NoConflicts");
        assert_eq!(cond.message, "no conflicts detected");
        assert_eq!(cond.observed_generation, Some(16));
    }

    #[test]
    fn test_conflicted() {
        let cond = conflicted(17, "ListenerConflict", "port conflict on listener");
        assert_eq!(cond.type_, "Conflicted");
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason, "ListenerConflict");
        assert_eq!(cond.message, "port conflict on listener");
        assert_eq!(cond.observed_generation, Some(17));
    }
}
