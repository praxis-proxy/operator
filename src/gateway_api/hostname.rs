// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Shared hostname matching for Gateway API route attachment.

// -----------------------------------------------------------------------------
// Hostname Matching
// -----------------------------------------------------------------------------

/// Checks if a route hostname matches a listener hostname.
///
/// Wildcard matching per Gateway API spec: `*.example.com` is a suffix
/// match that covers any depth (`foo.example.com` and
/// `foo.bar.example.com`). Matching is bidirectional.
pub(crate) fn hostname_matches(route_host: &str, listener_host: &str) -> bool {
    if route_host == listener_host {
        return true;
    }
    if let Some(suffix) = listener_host.strip_prefix("*")
        && route_host.len() > suffix.len()
        && route_host.ends_with(suffix)
    {
        return true;
    }
    if let Some(suffix) = route_host.strip_prefix("*")
        && listener_host.len() > suffix.len()
        && listener_host.ends_with(suffix)
    {
        return true;
    }
    false
}

/// Computes the intersection of a route hostname with a listener hostname.
///
/// Returns the most specific hostname that satisfies both constraints,
/// or `None` if the hostnames do not intersect. When a wildcard matches
/// an exact hostname, the exact hostname is returned.
///
/// ```ignore
/// assert_eq!(
///     hostname_intersection("foo.example.com", "*.example.com"),
///     Some("foo.example.com".to_owned()),
/// );
/// ```
pub(crate) fn hostname_intersection(route_host: &str, listener_host: &str) -> Option<String> {
    if route_host == listener_host {
        return Some(route_host.to_owned());
    }
    if listener_host.starts_with("*.") && hostname_matches(route_host, listener_host) {
        return Some(route_host.to_owned());
    }
    if route_host.starts_with("*.") && hostname_matches(route_host, listener_host) {
        return Some(listener_host.to_owned());
    }
    None
}

/// Filters route hostnames to only those intersecting listener hostnames.
///
/// For each route hostname, checks every listener hostname for an
/// intersection. Returns the intersected hostnames (most specific form).
/// Deduplicates results.
///
/// When `listener_hostnames` is empty (listeners without hostname
/// constraints), all route hostnames pass through unchanged.
///
/// ```ignore
/// let route = &["non.matching.com".to_owned(), "very.specific.com".to_owned()];
/// let listeners = &[Some("very.specific.com".to_owned())];
/// let result = intersect_hostnames(route, listeners);
/// assert_eq!(result, vec!["very.specific.com"]);
/// ```
pub(crate) fn intersect_hostnames(route_hostnames: &[String], listener_hostnames: &[Option<String>]) -> Vec<String> {
    let constrained: Vec<_> = listener_hostnames.iter().filter_map(|h| h.as_deref()).collect();
    if constrained.is_empty() || constrained.len() < listener_hostnames.len() {
        return route_hostnames.to_vec();
    }

    let mut result = Vec::new();
    for rh in route_hostnames {
        for lh in &constrained {
            if let Some(intersected) = hostname_intersection(rh, lh) {
                if !result.contains(&intersected) {
                    result.push(intersected);
                }
                break;
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
    fn test_hostname_matches_exact() {
        assert!(
            hostname_matches("example.com", "example.com"),
            "exact match should succeed"
        );
    }

    #[test]
    fn test_hostname_matches_wildcard_listener() {
        assert!(
            hostname_matches("foo.example.com", "*.example.com"),
            "subdomain should match wildcard listener"
        );
        assert!(
            !hostname_matches("notexample.com", "*.example.com"),
            "non-subdomain should not match wildcard listener"
        );
    }

    #[test]
    fn test_hostname_matches_wildcard_route() {
        assert!(
            hostname_matches("*.example.com", "foo.example.com"),
            "wildcard route should match subdomain listener"
        );
    }

    #[test]
    fn test_hostname_matches_no_match() {
        assert!(
            !hostname_matches("other.com", "example.com"),
            "different hostnames should not match"
        );
    }

    #[test]
    fn test_hostname_matches_bare_domain_does_not_match_wildcard() {
        assert!(
            !hostname_matches("example.com", "*.example.com"),
            "bare domain should NOT match wildcard per Gateway API spec"
        );
    }

    #[test]
    fn test_hostname_matches_multi_level_subdomain_accepted() {
        assert!(
            hostname_matches("foo.bar.example.com", "*.example.com"),
            "multi-level subdomain should match wildcard (suffix match per Gateway API spec)"
        );
    }

    // -----------------------------------------------------------------------
    // hostname_intersection
    // -----------------------------------------------------------------------

    #[test]
    fn test_intersection_exact_match() {
        assert_eq!(
            hostname_intersection("example.com", "example.com"),
            Some("example.com".to_owned()),
            "identical exact hostnames should intersect"
        );
    }

    #[test]
    fn test_intersection_exact_no_match() {
        assert_eq!(
            hostname_intersection("other.com", "example.com"),
            None,
            "different exact hostnames should not intersect"
        );
    }

    #[test]
    fn test_intersection_route_exact_listener_wildcard() {
        assert_eq!(
            hostname_intersection("foo.example.com", "*.example.com"),
            Some("foo.example.com".to_owned()),
            "exact route matching wildcard listener should return exact"
        );
    }

    #[test]
    fn test_intersection_route_wildcard_listener_exact() {
        assert_eq!(
            hostname_intersection("*.example.com", "foo.example.com"),
            Some("foo.example.com".to_owned()),
            "wildcard route matching exact listener should return exact"
        );
    }

    #[test]
    fn test_intersection_both_wildcards_same_domain() {
        assert_eq!(
            hostname_intersection("*.example.com", "*.example.com"),
            Some("*.example.com".to_owned()),
            "identical wildcards should intersect"
        );
    }

    #[test]
    fn test_intersection_both_wildcards_different_domain() {
        assert_eq!(
            hostname_intersection("*.example.com", "*.other.com"),
            None,
            "different wildcard domains should not intersect"
        );
    }

    #[test]
    fn test_intersection_bare_domain_vs_wildcard() {
        assert_eq!(
            hostname_intersection("example.com", "*.example.com"),
            None,
            "bare domain should not intersect with its own wildcard"
        );
    }

    #[test]
    fn test_intersection_multi_level_subdomain_vs_wildcard() {
        assert_eq!(
            hostname_intersection("foo.bar.example.com", "*.example.com"),
            Some("foo.bar.example.com".to_owned()),
            "multi-level subdomain should intersect with wildcard (suffix match)"
        );
    }

    // -----------------------------------------------------------------------
    // intersect_hostnames
    // -----------------------------------------------------------------------

    #[test]
    fn test_intersect_filters_non_matching() {
        let route = &["non.matching.com".to_owned(), "very.specific.com".to_owned()];
        let listeners = &[Some("very.specific.com".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["very.specific.com".to_owned()],
            "should keep only the matching hostname"
        );
    }

    #[test]
    fn test_intersect_wildcard_listener() {
        let route = &[
            "non.matching.com".to_owned(),
            "foo.wildcard.io".to_owned(),
            "bar.wildcard.io".to_owned(),
        ];
        let listeners = &[Some("*.wildcard.io".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["foo.wildcard.io".to_owned(), "bar.wildcard.io".to_owned()],
            "should keep both matching subdomains"
        );
    }

    #[test]
    fn test_intersect_no_listener_hostname_passes_all() {
        let route = &["a.com".to_owned(), "b.com".to_owned()];
        let listeners = &[None];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["a.com".to_owned(), "b.com".to_owned()],
            "unconstrained listeners should pass all hostnames"
        );
    }

    #[test]
    fn test_intersect_empty_listeners_passes_all() {
        let route = &["a.com".to_owned(), "b.com".to_owned()];
        let listeners: &[Option<String>] = &[];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["a.com".to_owned(), "b.com".to_owned()],
            "empty listener list should pass all hostnames"
        );
    }

    #[test]
    fn test_intersect_multiple_listeners() {
        let route = &[
            "non.matching.com".to_owned(),
            "very.specific.com".to_owned(),
            "foo.wildcard.io".to_owned(),
        ];
        let listeners = &[Some("very.specific.com".to_owned()), Some("*.wildcard.io".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["very.specific.com".to_owned(), "foo.wildcard.io".to_owned()],
            "should match across multiple listener hostnames"
        );
    }

    #[test]
    fn test_intersect_no_matches_returns_empty() {
        let route = &["non.matching.com".to_owned(), "other.com".to_owned()];
        let listeners = &[Some("very.specific.com".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert!(result.is_empty(), "no intersecting hostnames should return empty");
    }

    #[test]
    fn test_intersect_deduplicates() {
        let route = &["foo.example.com".to_owned()];
        let listeners = &[Some("*.example.com".to_owned()), Some("*.example.com".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["foo.example.com".to_owned()],
            "should not produce duplicate hostnames"
        );
    }

    #[test]
    fn test_intersect_wildcard_route_exact_listener() {
        let route = &["*.specific.com".to_owned()];
        let listeners = &[Some("very.specific.com".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["very.specific.com".to_owned()],
            "wildcard route should intersect to exact listener hostname"
        );
    }

    #[test]
    fn test_intersect_mixed_constrained_unconstrained_passes_all() {
        let route = &["example.org".to_owned(), "other.com".to_owned()];
        let listeners = &[None, Some("specific.com".to_owned())];
        let result = intersect_hostnames(route, listeners);
        assert_eq!(
            result,
            vec!["example.org".to_owned(), "other.com".to_owned()],
            "presence of unconstrained listener should pass all route hostnames"
        );
    }
}
