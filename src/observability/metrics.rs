// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Process-wide reconciliation counters.
//!
//! Exposed in Prometheus text exposition format by the observability
//! server. Counters are plain atomics rather than a metrics crate: the
//! set is small and fixed, and the conventions favour avoiding a
//! dependency where a few atomics do.

use std::{
    fmt,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Controllers that report reconciliation outcomes.
///
/// Indexes into [`Metrics::reconciles`]; kept in step with
/// [`Controller`].
const CONTROLLER_NAMES: [&str; 3] = ["gatewayclass", "gateway", "httproute"];

/// Counters for this process.
///
/// Reconciliation outcomes are recorded from many call sites that have
/// no reason to carry a handle, so the registry is a process global. The
/// type itself stays free of global state, and tests build their own
/// instances.
static GLOBAL: LazyLock<Metrics> = LazyLock::new(Metrics::default);

// -----------------------------------------------------------------------------
// Controller
// -----------------------------------------------------------------------------

/// Returns the process-wide counter registry.
pub(crate) fn global() -> &'static Metrics {
    &GLOBAL
}

/// Which controller a measurement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Controller {
    /// The `GatewayClass` reconciler.
    GatewayClass,

    /// The `Gateway` reconciler.
    Gateway,

    /// The `HTTPRoute` reconciler.
    HttpRoute,
}

impl Controller {
    /// Returns the index of this controller's counters.
    const fn index(self) -> usize {
        match self {
            Self::GatewayClass => 0,
            Self::Gateway => 1,
            Self::HttpRoute => 2,
        }
    }
}

// -----------------------------------------------------------------------------
// Metrics
// -----------------------------------------------------------------------------

/// Counters shared by every controller.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    /// Successful reconciliations, indexed by [`Controller::index`].
    reconciles: [AtomicU64; 3],

    /// Failed reconciliations, indexed by [`Controller::index`].
    errors: [AtomicU64; 3],

    /// Status patches skipped because the live object already matched.
    ///
    /// A rising success count against a flat patch count is what
    /// distinguishes a settled operator from one rewriting identical
    /// status forever.
    status_patches_skipped: AtomicU64,

    /// Status patches actually written.
    status_patches_written: AtomicU64,

    /// Whether this replica currently holds the leadership lease.
    ///
    /// Readiness deliberately does not track this — a standby is healthy
    /// — so leadership is reported here instead, where an operator can
    /// alert on a cluster with no leader or more than one.
    leader: AtomicU64,
}

impl Metrics {
    /// Records a successful reconciliation.
    pub(crate) fn record_reconcile(&self, controller: Controller) {
        Self::bump(&self.reconciles, controller);
    }

    /// Records a failed reconciliation.
    pub(crate) fn record_error(&self, controller: Controller) {
        Self::bump(&self.errors, controller);
    }

    /// Records a status patch that was skipped as redundant.
    pub(crate) fn record_status_skipped(&self) {
        self.status_patches_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a status patch that was written.
    pub(crate) fn record_status_written(&self) {
        self.status_patches_written.fetch_add(1, Ordering::Relaxed);
    }

    /// Records whether this replica holds the leadership lease.
    pub(crate) fn set_leader(&self, leading: bool) {
        self.leader.store(u64::from(leading), Ordering::Relaxed);
    }

    /// Increments one controller's slot in a counter array.
    fn bump(counters: &[AtomicU64; 3], controller: Controller) {
        if let Some(counter) = counters.get(controller.index()) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Renders every counter in Prometheus text exposition format.
impl fmt::Display for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_counter(
            f,
            "praxis_operator_reconcile_total",
            "Successful reconciliations by controller.",
            &self.reconciles,
        )?;
        write_counter(
            f,
            "praxis_operator_reconcile_errors_total",
            "Failed reconciliations by controller.",
            &self.errors,
        )?;

        write_scalar(
            f,
            "praxis_operator_status_patches_skipped_total",
            "Status patches skipped as redundant.",
            self.status_patches_skipped.load(Ordering::Relaxed),
        )?;
        write_scalar(
            f,
            "praxis_operator_status_patches_written_total",
            "Status patches written to the API server.",
            self.status_patches_written.load(Ordering::Relaxed),
        )?;

        writeln!(
            f,
            "# HELP praxis_operator_leader Whether this replica holds the leadership lease."
        )?;
        writeln!(f, "# TYPE praxis_operator_leader gauge")?;
        writeln!(f, "praxis_operator_leader {}", self.leader.load(Ordering::Relaxed))
    }
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Writes one counter family labelled by controller.
fn write_counter(f: &mut fmt::Formatter<'_>, name: &str, help: &str, counters: &[AtomicU64; 3]) -> fmt::Result {
    writeln!(f, "# HELP {name} {help}")?;
    writeln!(f, "# TYPE {name} counter")?;

    for (index, controller) in CONTROLLER_NAMES.iter().enumerate() {
        let value = counters.get(index).map_or(0, |c| c.load(Ordering::Relaxed));
        writeln!(f, "{name}{{controller=\"{controller}\"}} {value}")?;
    }

    Ok(())
}

/// Writes one unlabelled counter.
fn write_scalar(f: &mut fmt::Formatter<'_>, name: &str, help: &str, value: u64) -> fmt::Result {
    writeln!(f, "# HELP {name} {help}")?;
    writeln!(f, "# TYPE {name} counter")?;
    writeln!(f, "{name} {value}")
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
    use super::*;

    #[test]
    fn test_reconcile_counts_are_per_controller() {
        let metrics = Metrics::default();
        metrics.record_reconcile(Controller::Gateway);
        metrics.record_reconcile(Controller::Gateway);
        metrics.record_reconcile(Controller::HttpRoute);

        let encoded = metrics.to_string();

        assert!(
            encoded.contains("praxis_operator_reconcile_total{controller=\"gateway\"} 2"),
            "gateway reconciles should be counted separately: {encoded}"
        );
        assert!(
            encoded.contains("praxis_operator_reconcile_total{controller=\"httproute\"} 1"),
            "httproute reconciles should be counted separately: {encoded}"
        );
    }

    #[test]
    fn test_every_controller_is_reported_even_at_zero() {
        let encoded = Metrics::default().to_string();

        for controller in CONTROLLER_NAMES {
            assert!(
                encoded.contains(&format!("{{controller=\"{controller}\"}} 0")),
                "a controller that has not run yet must still report zero, not be absent: {encoded}"
            );
        }
    }

    #[test]
    fn test_errors_are_counted_apart_from_successes() {
        let metrics = Metrics::default();
        metrics.record_error(Controller::GatewayClass);

        let encoded = metrics.to_string();

        assert!(
            encoded.contains("praxis_operator_reconcile_errors_total{controller=\"gatewayclass\"} 1"),
            "errors should be counted: {encoded}"
        );
        assert!(
            encoded.contains("praxis_operator_reconcile_total{controller=\"gatewayclass\"} 0"),
            "an error must not also count as a success: {encoded}"
        );
    }

    #[test]
    fn test_status_patch_counters_track_both_outcomes() {
        let metrics = Metrics::default();
        metrics.record_status_skipped();
        metrics.record_status_skipped();
        metrics.record_status_written();

        let encoded = metrics.to_string();

        assert!(
            encoded.contains("praxis_operator_status_patches_skipped_total 2"),
            "skipped patches should be counted: {encoded}"
        );
        assert!(
            encoded.contains("praxis_operator_status_patches_written_total 1"),
            "written patches should be counted: {encoded}"
        );
    }

    #[test]
    fn test_encoding_declares_help_and_type_for_every_family() {
        let encoded = Metrics::default().to_string();

        assert_eq!(
            encoded.matches("# HELP ").count(),
            5,
            "every counter family needs a HELP line to be a valid exposition: {encoded}"
        );
        assert_eq!(
            encoded.matches("# TYPE ").count(),
            5,
            "every counter family needs a TYPE line: {encoded}"
        );
    }

    #[test]
    fn test_leadership_is_reported_as_a_gauge() {
        let metrics = Metrics::default();

        assert!(
            metrics.to_string().contains("praxis_operator_leader 0"),
            "a standby must report zero, not omit the gauge, or a cluster with no leader is \
             indistinguishable from one that never reported"
        );

        metrics.set_leader(true);
        assert!(
            metrics.to_string().contains("praxis_operator_leader 1"),
            "the holder should report one"
        );
    }

    #[test]
    fn test_controller_indices_are_distinct() {
        let indices = [
            Controller::GatewayClass.index(),
            Controller::Gateway.index(),
            Controller::HttpRoute.index(),
        ];

        assert_eq!(
            indices,
            [0, 1, 2],
            "controllers must map to distinct slots or their counters collide"
        );
    }
}
