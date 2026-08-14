// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Shared hostname matching for Gateway API route attachment.

use std::collections::BTreeSet;

// -----------------------------------------------------------------------------
// Hostname Matching
// -----------------------------------------------------------------------------

/// Checks if a route hostname matches a listener hostname.
///
/// Wildcard matching per Gateway API spec: `*.example.com` is a suffix
/// match that covers any depth (`foo.example.com` and
/// `foo.bar.example.com`). Matching is bidirectional.
///
/// Comparison is ASCII case-insensitive: DNS names are case-insensitive
/// per RFC 1123 section 2.1 and RFC 4343 section 1.
///
/// ```
/// use praxis_operator::gateway_api::hostname::hostname_matches;
///
/// assert!(hostname_matches("foo.example.com", "*.example.com"));
/// assert!(hostname_matches("Foo.Example.com", "*.example.com"));
///
/// // A bare domain does not match its own wildcard.
/// assert!(!hostname_matches("example.com", "*.example.com"));
///
/// // The separator dot is load-bearing.
/// assert!(!hostname_matches("fooexample.com", "*.example.com"));
/// ```
pub fn hostname_matches(route_host: &str, listener_host: &str) -> bool {
    if route_host.eq_ignore_ascii_case(listener_host) {
        return true;
    }
    wildcard_covers(listener_host, route_host) || wildcard_covers(route_host, listener_host)
}

/// Returns `true` when `wildcard` is a `*.` pattern covering `candidate`.
///
/// The wildcard label must be the whole first label, so only a literal
/// `*.` prefix is honoured; `*foo.com` is not a wildcard. The retained
/// dot separator is what stops `*.example.com` from covering
/// `fooexample.com`.
fn wildcard_covers(wildcard: &str, candidate: &str) -> bool {
    if !wildcard.starts_with("*.") {
        return false;
    }
    let Some(suffix) = wildcard.strip_prefix('*') else {
        return false;
    };

    let Some(start) = candidate.len().checked_sub(suffix.len()) else {
        return false;
    };
    if start == 0 {
        return false;
    }

    candidate
        .get(start..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Computes the intersection of a route hostname with a listener hostname.
///
/// Returns the most specific hostname that satisfies both constraints,
/// or `None` if the hostnames do not intersect. When a wildcard matches
/// an exact hostname, the exact hostname is returned.
///
/// ```
/// use praxis_operator::gateway_api::hostname::hostname_intersection;
///
/// // The more specific side wins.
/// assert_eq!(
///     hostname_intersection("foo.example.com", "*.example.com"),
///     Some("foo.example.com".to_owned()),
/// );
/// assert_eq!(
///     hostname_intersection("bar.other.com", "*.example.com"),
///     None
/// );
/// ```
pub fn hostname_intersection(route_host: &str, listener_host: &str) -> Option<String> {
    if route_host.eq_ignore_ascii_case(listener_host) {
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
/// Results keep route-hostname order, which the generated config depends
/// on.
///
/// ```
/// use praxis_operator::gateway_api::hostname::intersect_hostnames;
///
/// let routes = ["a.example.com".to_owned(), "nope.other.com".to_owned()];
/// let listeners = [Some("*.example.com".to_owned())];
///
/// assert_eq!(
///     intersect_hostnames(&routes, &listeners),
///     vec!["a.example.com"]
/// );
///
/// // An unconstrained listener passes everything through.
/// assert_eq!(intersect_hostnames(&routes, &[None]), routes.to_vec());
/// ```
pub fn intersect_hostnames(route_hostnames: &[String], listener_hostnames: &[Option<String>]) -> Vec<String> {
    let constrained: Vec<_> = listener_hostnames.iter().filter_map(|h| h.as_deref()).collect();
    if constrained.is_empty() || constrained.len() < listener_hostnames.len() {
        return route_hostnames.to_vec();
    }

    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for rh in route_hostnames {
        for lh in &constrained {
            if let Some(intersected) = hostname_intersection(rh, lh) {
                if seen.insert(intersected.to_ascii_lowercase()) {
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

    // -----------------------------------------------------------------------
    // RFC 1123 section 2.1 / RFC 4343 section 1: DNS names are
    // case-insensitive.
    // -----------------------------------------------------------------------

    /// RFC 1123 section 2.1: host names are matched without regard to case.
    #[test]
    fn test_rfc1123_exact_hostname_match_is_case_insensitive() {
        assert!(
            hostname_matches("Example.COM", "example.com"),
            "exact hostnames differing only in case must match per RFC 1123 s2.1"
        );
    }

    /// RFC 4343 section 1: DNS name comparison is case-insensitive.
    #[test]
    fn test_rfc4343_wildcard_hostname_match_is_case_insensitive() {
        assert!(
            hostname_matches("FOO.Example.com", "*.example.COM"),
            "wildcard matching must ignore case per RFC 4343 s1"
        );
    }

    /// RFC 4343 section 1: case-insensitive comparison applies to
    /// intersection as well as matching.
    #[test]
    fn test_rfc4343_intersection_is_case_insensitive() {
        assert_eq!(
            hostname_intersection("Foo.Example.com", "*.example.com"),
            Some("Foo.Example.com".to_owned()),
            "intersection must match case-insensitively and preserve the route's spelling"
        );
    }

    #[test]
    fn test_wildcard_requires_dot_separator() {
        assert!(
            !hostname_matches("fooexample.com", "*.example.com"),
            "wildcard must not match a host that merely ends with the domain text"
        );
    }

    #[test]
    fn test_bare_star_prefix_is_not_a_wildcard() {
        assert!(
            !hostname_matches("bar.foo.com", "*foo.com"),
            "a wildcard label must be a whole label, so *foo.com is not a wildcard"
        );
    }

    #[test]
    fn test_wildcard_does_not_match_its_own_bare_domain_case_insensitively() {
        assert!(
            !hostname_matches("Example.com", "*.EXAMPLE.com"),
            "bare domain must not match its wildcard regardless of case"
        );
    }

    #[test]
    fn test_intersect_dedups_case_variants() {
        let route = &["Foo.Example.com".to_owned(), "foo.example.com".to_owned()];
        let listeners = &[Some("*.example.com".to_owned())];

        assert_eq!(
            intersect_hostnames(route, listeners).len(),
            1,
            "hostnames differing only in case must dedup to a single entry"
        );
    }
}
