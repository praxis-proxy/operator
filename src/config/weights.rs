// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Endpoint weight distribution for backend clusters.
//!
//! A Gateway API `backendRef.weight` applies to a Service, but Praxis
//! weights individual endpoints. Splitting one across the other is the
//! whole job here, and it is arithmetic that has already overflowed
//! once, so it lives apart from the reconciler with its own tests.

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Largest least-common-multiple denominator the split will use.
///
/// Endpoint counts that are mutually coprime make the true LCM grow
/// without bound; capping it trades exactness in extreme fan-outs for
/// arithmetic that cannot run away.
const MAX_LCM_DENOMINATOR: i64 = 1_000_000; // 1e6

/// Largest weight the generated data-plane config can carry.
const MAX_ENDPOINT_WEIGHT: i64 = 2_147_483_647; // i32::MAX

// -----------------------------------------------------------------------------
// Type Aliases
// -----------------------------------------------------------------------------

/// One backend's weight paired with its resolved endpoint addresses.
pub type ResolvedBackend = (i32, Vec<String>);

// -----------------------------------------------------------------------------
// Weight Distribution
// -----------------------------------------------------------------------------

/// Sorts endpoints within each service for deterministic config output.
///
/// Without sorting, endpoint IPs from `EndpointSlice` listings may
/// arrive in arbitrary order across reconciliations. This changes the
/// config YAML (and its SHA-256 hash), triggering unnecessary
/// Deployment rollouts and pod restarts.
pub fn sort_service_endpoints(service_data: &mut [ResolvedBackend]) {
    for (_, eps) in service_data.iter_mut() {
        eps.sort();
    }
}

/// Distributes service-level weights across endpoints.
///
/// For each service with weight `W` and `N` endpoints, assigns
/// `(W * lcm) / N` to each endpoint, where `lcm` is the least
/// common multiple of all endpoint counts. The final weights are
/// reduced by their GCD to minimise the round-robin cycle length,
/// which improves distribution accuracy for small request batches.
///
/// All arithmetic runs in `i64` and saturates. The release profile
/// combines `overflow-checks` with `panic = "abort"`, so an overflow
/// here would kill the operator rather than mis-route a request.
pub fn distribute_service_weights(service_data: &[ResolvedBackend]) -> (Vec<String>, Vec<i32>) {
    let lcm_denominator = endpoint_count_lcm(service_data);
    let mut all_endpoints = Vec::new();
    let mut all_weights = Vec::new();

    for (service_weight, endpoints) in service_data {
        if endpoints.is_empty() {
            continue;
        }

        let count = endpoint_count(endpoints);
        #[expect(clippy::arithmetic_side_effects, reason = "count >= 1 from endpoint_count")]
        let ep_weight = i64::from(*service_weight).saturating_mul(lcm_denominator) / count;
        for ep in endpoints {
            all_endpoints.push(ep.clone());
            all_weights.push(ep_weight);
        }
    }

    reduce_weights_by_gcd(&mut all_weights);
    (all_endpoints, scale_weights_into_range(&all_weights))
}

/// Least common multiple of every non-empty endpoint count.
fn endpoint_count_lcm(service_data: &[ResolvedBackend]) -> i64 {
    service_data
        .iter()
        .filter(|(_, eps)| !eps.is_empty())
        .map(|(_, eps)| endpoint_count(eps))
        .fold(1, lcm)
}

/// Returns an endpoint count as a positive `i64`.
fn endpoint_count(endpoints: &[String]) -> i64 {
    i64::try_from(endpoints.len()).unwrap_or(i64::MAX).max(1)
}

/// Divides all positive weights by their GCD to minimise cycle length.
fn reduce_weights_by_gcd(weights: &mut [i64]) {
    let divisor = weights.iter().copied().filter(|wt| *wt > 0).fold(0, gcd);
    if divisor > 1 {
        for wt in weights.iter_mut() {
            if *wt > 0 {
                #[expect(clippy::arithmetic_side_effects, reason = "divisor > 1 and wt > 0")]
                {
                    *wt /= divisor;
                }
            }
        }
    }
}

/// Scales weights down until each one fits the config's `i32` field.
///
/// Positive weights stay positive so an endpoint is never silently
/// dropped from the load-balancing rotation.
fn scale_weights_into_range(weights: &[i64]) -> Vec<i32> {
    let largest = weights.iter().copied().max().unwrap_or(0);
    let divisor = (largest.saturating_add(MAX_ENDPOINT_WEIGHT - 1) / MAX_ENDPOINT_WEIGHT).max(1);

    weights.iter().map(|wt| scale_weight(*wt, divisor)).collect()
}

/// Scales a single weight into `i32` range.
fn scale_weight(weight: i64, divisor: i64) -> i32 {
    #[expect(clippy::arithmetic_side_effects, reason = "divisor >= 1 from caller")]
    let scaled = weight / divisor;
    let floored = if weight > 0 { scaled.max(1) } else { scaled };
    i32::try_from(floored).unwrap_or(i32::MAX)
}

/// Greatest common divisor (Euclidean algorithm).
fn gcd(mut lhs: i64, mut rhs: i64) -> i64 {
    while rhs != 0 {
        let temp = rhs;
        #[expect(clippy::arithmetic_side_effects, reason = "Euclidean algorithm, rhs != 0")]
        {
            rhs = lhs % rhs;
        }
        lhs = temp;
    }
    lhs.saturating_abs()
}

/// Least common multiple, capped at [`MAX_LCM_DENOMINATOR`].
fn lcm(lhs: i64, rhs: i64) -> i64 {
    if lhs == 0 || rhs == 0 {
        return 0;
    }

    #[expect(clippy::arithmetic_side_effects, reason = "gcd(lhs,rhs) >= 1 when both non-zero")]
    (lhs / gcd(lhs, rhs))
        .checked_mul(rhs)
        .map_or(MAX_LCM_DENOMINATOR, i64::saturating_abs)
        .min(MAX_LCM_DENOMINATOR)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // -----------------------------------------------------------------------------

    #[test]
    fn test_gcd_basics() {
        assert_eq!(gcd(0, 0), 0, "gcd of zeros is zero");
        assert_eq!(gcd(12, 0), 12, "gcd with zero returns the other operand");
        assert_eq!(gcd(12, 18), 6, "gcd(12, 18) is 6");
        assert_eq!(gcd(17, 5), 1, "coprime operands have gcd 1");
    }

    #[test]
    fn test_gcd_of_extremes_does_not_overflow() {
        assert_eq!(
            gcd(i64::MIN, 0),
            i64::MAX,
            "gcd must saturate rather than overflow on i64::MIN"
        );
    }

    #[test]
    fn test_lcm_basics() {
        assert_eq!(lcm(0, 5), 0, "lcm with zero is zero");
        assert_eq!(lcm(4, 6), 12, "lcm(4, 6) is 12");
        assert_eq!(lcm(lcm(lcm(7, 11), 13), 17), 17_017, "coprime counts multiply out");
    }

    #[test]
    fn test_lcm_is_capped() {
        assert_eq!(
            lcm(MAX_LCM_DENOMINATOR, 999_983),
            MAX_LCM_DENOMINATOR,
            "the denominator must never exceed its ceiling"
        );
    }

    #[test]
    fn test_lcm_of_large_coprimes_does_not_overflow() {
        assert!(
            lcm(i64::MAX, i64::MAX - 1) <= MAX_LCM_DENOMINATOR,
            "an lcm that cannot be represented must fall back to the ceiling"
        );
    }

    #[test]
    fn test_distribute_weights_single_service_is_uniform() {
        let data = [(1, endpoints(&["10.0.0.1:80", "10.0.0.2:80"]))];
        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(eps.len(), 2, "every endpoint should be emitted");
        assert_eq!(weights, vec![1, 1], "a single service splits evenly across its pods");
    }

    #[test]
    fn test_distribute_weights_respects_service_ratio() {
        let data = [(3, endpoints(&["10.0.0.1:80"])), (1, endpoints(&["10.0.1.1:80"]))];
        let (_, weights) = distribute_service_weights(&data);

        assert_eq!(
            weights,
            vec![3, 1],
            "endpoint weights should mirror the backend weights"
        );
    }

    #[test]
    fn test_distribute_weights_normalises_uneven_replica_counts() {
        let data = [
            (1, endpoints(&["10.0.0.1:80", "10.0.0.2:80"])),
            (1, endpoints(&["10.0.1.1:80"])),
        ];
        let (_, weights) = distribute_service_weights(&data);

        assert_eq!(
            weights,
            vec![1, 1, 2],
            "a one-pod service must carry the same total share as a two-pod service"
        );
    }

    #[test]
    fn test_distribute_weights_skips_services_without_endpoints() {
        let data = [(5, endpoints(&[])), (1, endpoints(&["10.0.1.1:80"]))];
        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(
            eps,
            vec!["10.0.1.1:80".to_owned()],
            "an empty service contributes nothing"
        );
        assert_eq!(weights, vec![1], "only the resolved service is weighted");
    }

    #[test]
    fn test_distribute_weights_survives_adversarial_endpoint_counts() {
        let data = [
            (1_000_000, endpoints(&["10.0.0.1:80"; 7])),
            (1_000_000, endpoints(&["10.0.1.1:80"; 11])),
            (1_000_000, endpoints(&["10.0.2.1:80"; 13])),
            (1_000_000, endpoints(&["10.0.3.1:80"; 17])),
        ];

        let (eps, weights) = distribute_service_weights(&data);

        assert_eq!(eps.len(), 48, "every pod of every backend should be emitted");
        assert_eq!(weights.len(), 48, "each endpoint needs a weight");
        assert!(
            weights.iter().all(|wt| *wt > 0),
            "coprime pod counts at the maximum Gateway API weight must not zero out or abort"
        );
    }

    #[test]
    fn test_distribute_weights_saturates_at_the_config_ceiling() {
        let data = [
            (i32::MAX, endpoints(&["10.0.0.1:80"])),
            (1, endpoints(&["10.0.1.1:80"])),
        ];

        let (_, weights) = distribute_service_weights(&data);

        assert!(
            weights.iter().all(|wt| *wt > 0),
            "extreme weights must stay representable instead of overflowing"
        );
    }

    #[test]
    fn test_reduce_weights_by_gcd() {
        let mut weights = vec![4, 8, 12];
        reduce_weights_by_gcd(&mut weights);

        assert_eq!(weights, vec![1, 2, 3], "weights should be reduced by their gcd");
    }

    #[test]
    fn test_reduce_weights_ignores_zero_weights() {
        let mut weights = vec![0, 4, 8];
        reduce_weights_by_gcd(&mut weights);

        assert_eq!(weights, vec![0, 1, 2], "a zero weight must stay zero");
    }

    #[test]
    fn test_scale_weight_keeps_positive_weights_positive() {
        assert_eq!(scale_weight(1, 1_000), 1, "a positive weight never scales to zero");
        assert_eq!(scale_weight(0, 1_000), 0, "a zero weight stays zero");
        assert_eq!(scale_weight(2_000, 1_000), 2, "scaling divides by the divisor");
    }

    #[test]
    fn test_scale_weights_into_range_fits_i32() {
        let weights = [i64::from(i32::MAX) * 4, i64::from(i32::MAX) * 2];
        let scaled = scale_weights_into_range(&weights);

        assert_eq!(scaled.len(), 2, "every weight should be scaled");
        assert!(
            scaled.iter().all(|wt| *wt > 0),
            "scaling must keep every endpoint in the rotation"
        );
    }

    #[test]
    fn test_sort_service_endpoints_is_deterministic() {
        let mut data = [(1, endpoints(&["10.0.0.3:80", "10.0.0.1:80", "10.0.0.2:80"]))];
        sort_service_endpoints(&mut data);

        assert_eq!(
            data[0].1,
            endpoints(&["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]),
            "endpoint order must be stable so the config hash does not churn"
        );
    }

    #[test]
    fn test_endpoint_count_never_returns_zero() {
        assert_eq!(endpoint_count(&[]), 1, "an empty list must not produce a zero divisor");
        assert_eq!(
            endpoint_count(&endpoints(&["a", "b"])),
            2,
            "count should match the list"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds an owned endpoint address list.
    fn endpoints(addrs: &[&str]) -> Vec<String> {
        addrs.iter().map(|addr| (*addr).to_owned()).collect()
    }
}
