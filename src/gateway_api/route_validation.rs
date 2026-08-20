// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Detection of `HTTPRoute` rules this operator cannot honour.
//!
//! The Gateway API requires an implementation to refuse configuration it
//! cannot express rather than approximate it. A rule that is silently
//! widened — a regular-expression path collapsed to a catch-all, a
//! header match dropped — sends traffic the author never asked for, so
//! every unsupported construct is surfaced here and the rule is excluded
//! from the generated config.
//!
//! Three categories are refused. Regular-expression matching this
//! operator does not implement; match fields the Praxis route schema
//! has no field for at all — `praxis_core::config::Route` carries only
//! a path match, host, headers and cluster, so a method or
//! query-parameter constraint has nowhere to go, and emitting one
//! anyway would see it dropped during deserialization and the route
//! quietly serve every method; and filters the config generator does
//! not produce, together with the one rewrite the Gateway API itself
//! declares invalid — a `ReplacePrefixMatch` on a rule that matches no
//! prefix.

use std::collections::BTreeMap;

use gateway_api::httproutes::{
    HTTPRoute, HttpRouteRules, HttpRouteRulesFiltersType, HttpRouteRulesFiltersUrlRewritePathType,
    HttpRouteRulesMatches, HttpRouteRulesMatchesHeadersType, HttpRouteRulesMatchesPathType,
    HttpRouteRulesMatchesQueryParamsType,
};

// -----------------------------------------------------------------------------
// RuleRejection
// -----------------------------------------------------------------------------

/// Why a single `HTTPRoute` rule cannot be honoured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleRejection {
    /// A match or filter used a regular expression.
    RegularExpression(&'static str),

    /// A filter type this operator does not implement.
    UnsupportedFilter(String),

    /// A match field the Praxis route schema has no equivalent for.
    UnsupportedMatchField(&'static str),

    /// A `ReplacePrefixMatch` rewrite on a rule that matches no prefix.
    PrefixRewriteWithoutPrefixMatch,
}

impl RuleRejection {
    /// Returns a human-readable explanation for a status message.
    pub fn message(&self) -> String {
        match self {
            Self::RegularExpression(field) => {
                format!("RegularExpression {field} matching is not supported")
            },
            Self::UnsupportedFilter(kind) => format!("filter type {kind} is not supported"),
            Self::UnsupportedMatchField(field) => {
                format!("{field} matching is not supported by the Praxis route schema")
            },
            Self::PrefixRewriteWithoutPrefixMatch => {
                "URLRewrite ReplacePrefixMatch requires a PathPrefix match on the same rule".to_owned()
            },
        }
    }
}

// -----------------------------------------------------------------------------
// RouteValidation
// -----------------------------------------------------------------------------

/// The rules of one `HTTPRoute` that cannot be honoured, keyed by index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteValidation {
    /// Rejected rule indices and the reason for each.
    rejected: BTreeMap<usize, RuleRejection>,

    /// Total number of rules the route declares.
    total: usize,
}

impl RouteValidation {
    /// Returns `true` when the rule at `index` must be excluded from the
    /// generated config.
    pub fn is_rejected(&self, index: usize) -> bool {
        self.rejected.contains_key(&index)
    }

    /// Returns `true` when no rule can be honoured.
    ///
    /// A route declaring no rules at all is not fully rejected; it simply
    /// contributes nothing.
    pub fn is_fully_rejected(&self) -> bool {
        self.total > 0 && self.rejected.len() == self.total
    }

    /// Returns `true` when some, but not all, rules were rejected.
    pub fn is_partially_rejected(&self) -> bool {
        !self.rejected.is_empty() && !self.is_fully_rejected()
    }

    /// Returns a status message naming the first rejection.
    ///
    /// Returns `None` when every rule is supported.
    pub fn message(&self) -> Option<String> {
        let (index, rejection) = self.rejected.iter().next()?;
        Some(format!("rule {index}: {}", rejection.message()))
    }
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

/// Finds every rule of `route` that this operator cannot honour.
pub fn validate_route(route: &HTTPRoute) -> RouteValidation {
    let rules = route.spec.rules.as_deref().unwrap_or(&[]);

    let rejected = rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| reject_rule(rule).map(|reason| (index, reason)))
        .collect();

    RouteValidation {
        rejected,
        total: rules.len(),
    }
}

/// Returns the reason a rule cannot be honoured, if any.
fn reject_rule(rule: &HttpRouteRules) -> Option<RuleRejection> {
    rule.matches
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find_map(reject_match)
        .or_else(|| reject_filters(rule))
        .or_else(|| reject_rewrites(rule))
}

/// Returns the reason a match cannot be honoured, if any.
fn reject_match(rule_match: &HttpRouteRulesMatches) -> Option<RuleRejection> {
    if has_regex_path(rule_match) {
        return Some(RuleRejection::RegularExpression("path"));
    }
    if has_regex_header(rule_match) {
        return Some(RuleRejection::RegularExpression("header"));
    }
    if has_regex_query_param(rule_match) {
        return Some(RuleRejection::RegularExpression("query parameter"));
    }
    if rule_match.method.is_some() {
        return Some(RuleRejection::UnsupportedMatchField("method"));
    }
    if rule_match
        .query_params
        .as_deref()
        .is_some_and(|query| !query.is_empty())
    {
        return Some(RuleRejection::UnsupportedMatchField("query parameter"));
    }
    None
}

/// Returns `true` when the match uses a regular-expression path.
fn has_regex_path(rule_match: &HttpRouteRulesMatches) -> bool {
    rule_match
        .path
        .as_ref()
        .and_then(|path| path.r#type.as_ref())
        .is_some_and(|match_type| *match_type == HttpRouteRulesMatchesPathType::RegularExpression)
}

/// Returns `true` when any header match uses a regular expression.
fn has_regex_header(rule_match: &HttpRouteRulesMatches) -> bool {
    rule_match.headers.as_deref().unwrap_or(&[]).iter().any(|header| {
        header
            .r#type
            .as_ref()
            .is_some_and(|match_type| *match_type == HttpRouteRulesMatchesHeadersType::RegularExpression)
    })
}

/// Returns `true` when any query-parameter match uses a regular
/// expression.
fn has_regex_query_param(rule_match: &HttpRouteRulesMatches) -> bool {
    rule_match.query_params.as_deref().unwrap_or(&[]).iter().any(|query| {
        query
            .r#type
            .as_ref()
            .is_some_and(|match_type| *match_type == HttpRouteRulesMatchesQueryParamsType::RegularExpression)
    })
}

/// Returns the reason a rule's filters cannot be honoured, if any.
fn reject_filters(rule: &HttpRouteRules) -> Option<RuleRejection> {
    rule.filters
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|filter| !is_supported_filter(&filter.r#type))
        .map(|filter| RuleRejection::UnsupportedFilter(format!("{:?}", filter.r#type)))
}

/// Returns `true` for filter types the config generator implements.
fn is_supported_filter(kind: &HttpRouteRulesFiltersType) -> bool {
    matches!(
        kind,
        HttpRouteRulesFiltersType::RequestHeaderModifier
            | HttpRouteRulesFiltersType::ResponseHeaderModifier
            | HttpRouteRulesFiltersType::RequestRedirect
            | HttpRouteRulesFiltersType::UrlRewrite
    )
}

/// Returns the reason a rule's rewrites cannot be honoured, if any.
///
/// `ReplacePrefixMatch` names no prefix of its own: it replaces
/// whatever the rule matched on. The Gateway API requires an
/// implementation to refuse the rule outright when there is no
/// `PathPrefix` match to take that from, rather than guess at one.
fn reject_rewrites(rule: &HttpRouteRules) -> Option<RuleRejection> {
    let prefixed = matches!(
        rule.matches
            .as_deref()
            .and_then(<[_]>::first)
            .and_then(|rule_match| rule_match.path.as_ref())
            .and_then(|path| path.r#type.as_ref()),
        Some(HttpRouteRulesMatchesPathType::PathPrefix)
    );
    if prefixed {
        return None;
    }

    rule.filters
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|filter| filter.url_rewrite.as_ref())
        .find(|rewrite| {
            rewrite
                .path
                .as_ref()
                .is_some_and(|path| path.r#type == HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch)
        })
        .map(|_| RuleRejection::PrefixRewriteWithoutPrefixMatch)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::default_trait_access, reason = "tests")]
mod tests {
    use gateway_api::httproutes::{
        HttpRouteRulesFilters, HttpRouteRulesFiltersUrlRewrite, HttpRouteRulesFiltersUrlRewritePath,
        HttpRouteRulesMatchesHeaders, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesQueryParams, HttpRouteSpec,
    };

    use super::*;

    #[test]
    fn test_plain_route_is_fully_supported() {
        let route = route_with(vec![rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/api")]);
        let validation = validate_route(&route);

        assert!(
            !validation.is_rejected(0),
            "a prefix path match is supported and must not be rejected"
        );
        assert!(
            validation.message().is_none(),
            "a fully supported route needs no status message"
        );
    }

    #[test]
    fn test_regex_path_is_rejected() {
        let route = route_with(vec![rule_with_path(
            HttpRouteRulesMatchesPathType::RegularExpression,
            "/api/.*",
        )]);
        let validation = validate_route(&route);

        assert!(
            validation.is_rejected(0),
            "a RegularExpression path must be rejected, never widened to a catch-all"
        );
        assert!(
            validation.is_fully_rejected(),
            "a single-rule route whose only rule is rejected is fully rejected"
        );
    }

    #[test]
    fn test_regex_header_is_rejected() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        set_header_match(&mut rule, HttpRouteRulesMatchesHeadersType::RegularExpression);
        let validation = validate_route(&route_with(vec![rule]));

        assert!(
            validation.is_rejected(0),
            "a RegularExpression header match must be rejected, not dropped"
        );
    }

    #[test]
    fn test_regex_query_param_is_rejected() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        set_query_match(&mut rule, HttpRouteRulesMatchesQueryParamsType::RegularExpression);
        let validation = validate_route(&route_with(vec![rule]));

        assert!(
            validation.is_rejected(0),
            "a RegularExpression query parameter match must be rejected"
        );
    }

    #[test]
    fn test_exact_header_match_is_supported() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        set_header_match(&mut rule, HttpRouteRulesMatchesHeadersType::Exact);

        assert!(
            !validate_route(&route_with(vec![rule])).is_rejected(0),
            "an Exact header match is supported"
        );
    }

    #[test]
    fn test_unsupported_filter_is_rejected() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        rule.filters = Some(vec![HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::RequestMirror,
            ..Default::default()
        }]);
        let validation = validate_route(&route_with(vec![rule]));

        assert!(
            validation.is_rejected(0),
            "RequestMirror is not implemented and must be rejected rather than ignored"
        );
        assert!(
            validation.message().is_some_and(|msg| msg.contains("not supported")),
            "the rejection message should name the unsupported construct"
        );
    }

    #[test]
    fn test_host_rewrite_is_accepted() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        rule.filters = Some(vec![url_rewrite(HttpRouteRulesFiltersUrlRewrite {
            hostname: Some("one.example.org".to_owned()),
            path: None,
        })]);

        assert!(
            !validate_route(&route_with(vec![rule])).is_rejected(0),
            "a hostname rewrite needs no path match and is served by setting the Host header"
        );
    }

    #[test]
    fn test_full_path_rewrite_is_accepted_without_a_prefix_match() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        rule.filters = Some(vec![url_rewrite(HttpRouteRulesFiltersUrlRewrite {
            hostname: None,
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplaceFullPath,
                replace_full_path: Some("/one".to_owned()),
                replace_prefix_match: None,
            }),
        })]);

        assert!(
            !validate_route(&route_with(vec![rule])).is_rejected(0),
            "ReplaceFullPath discards the whole path, so it does not care what the rule matched on"
        );
    }

    #[test]
    fn test_prefix_rewrite_without_a_prefix_match_is_rejected() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        rule.filters = Some(vec![url_rewrite(HttpRouteRulesFiltersUrlRewrite {
            hostname: None,
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch,
                replace_full_path: None,
                replace_prefix_match: Some("/one".to_owned()),
            }),
        })]);
        let validation = validate_route(&route_with(vec![rule]));

        assert!(
            validation.is_rejected(0),
            "ReplacePrefixMatch replaces what the rule matched on, and an Exact match leaves it \
             nothing to replace — the Gateway API requires refusing the rule, not guessing"
        );
        assert!(
            validation.message().is_some_and(|msg| msg.contains("PathPrefix")),
            "the message should say what the rule is missing"
        );
    }

    #[test]
    fn test_prefix_rewrite_with_a_prefix_match_is_accepted() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/api");
        rule.filters = Some(vec![url_rewrite(HttpRouteRulesFiltersUrlRewrite {
            hostname: None,
            path: Some(HttpRouteRulesFiltersUrlRewritePath {
                r#type: HttpRouteRulesFiltersUrlRewritePathType::ReplacePrefixMatch,
                replace_full_path: None,
                replace_prefix_match: Some("/one".to_owned()),
            }),
        })]);

        assert!(
            !validate_route(&route_with(vec![rule])).is_rejected(0),
            "the prefix the rewrite replaces is right there on the rule"
        );
    }

    #[test]
    fn test_supported_filters_are_accepted() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/api");
        rule.filters = Some(vec![
            HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
                ..Default::default()
            },
            HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::RequestRedirect,
                ..Default::default()
            },
        ]);

        assert!(
            !validate_route(&route_with(vec![rule])).is_rejected(0),
            "header modification and redirect are implemented"
        );
    }

    #[test]
    fn test_partial_rejection_is_distinguished_from_full() {
        let route = route_with(vec![
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/ok"),
            rule_with_path(HttpRouteRulesMatchesPathType::RegularExpression, "/bad/.*"),
        ]);
        let validation = validate_route(&route);

        assert!(
            validation.is_partially_rejected(),
            "one bad rule out of two is a partial rejection"
        );
        assert!(
            !validation.is_fully_rejected(),
            "a route with a surviving rule is not fully rejected"
        );
        assert!(!validation.is_rejected(0), "the supported rule must survive");
        assert!(validation.is_rejected(1), "the regex rule must be excluded");
    }

    #[test]
    fn test_route_without_rules_is_not_rejected() {
        let route = HTTPRoute {
            metadata: Default::default(),
            spec: HttpRouteSpec {
                rules: None,
                ..Default::default()
            },
            status: None,
        };
        let validation = validate_route(&route);

        assert!(
            !validation.is_fully_rejected(),
            "a route declaring no rules contributes nothing but is not itself invalid"
        );
        assert!(
            !validation.is_partially_rejected(),
            "a route declaring no rules has nothing to partially reject"
        );
    }

    #[test]
    fn test_message_names_the_offending_rule_index() {
        let route = route_with(vec![
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/ok"),
            rule_with_path(HttpRouteRulesMatchesPathType::RegularExpression, "/bad/.*"),
        ]);

        assert!(
            validate_route(&route)
                .message()
                .is_some_and(|msg| msg.starts_with("rule 1:")),
            "the message should identify which rule was rejected"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds an `HTTPRoute` carrying the given rules.
    fn route_with(rules: Vec<HttpRouteRules>) -> HTTPRoute {
        HTTPRoute {
            metadata: Default::default(),
            spec: HttpRouteSpec {
                rules: Some(rules),
                ..Default::default()
            },
            status: None,
        }
    }

    /// Wraps a `URLRewrite` body in a rule filter.
    fn url_rewrite(rewrite: HttpRouteRulesFiltersUrlRewrite) -> HttpRouteRulesFilters {
        HttpRouteRulesFilters {
            r#type: HttpRouteRulesFiltersType::UrlRewrite,
            url_rewrite: Some(rewrite),
            ..Default::default()
        }
    }

    /// Builds a rule with a single path match of the given type.
    fn rule_with_path(kind: HttpRouteRulesMatchesPathType, value: &str) -> HttpRouteRules {
        HttpRouteRules {
            matches: Some(vec![HttpRouteRulesMatches {
                path: Some(HttpRouteRulesMatchesPath {
                    r#type: Some(kind),
                    value: Some(value.to_owned()),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// Adds a header match of the given type to a rule's first match.
    fn set_header_match(rule: &mut HttpRouteRules, kind: HttpRouteRulesMatchesHeadersType) {
        let matches = rule.matches.as_mut().expect("rule should declare matches");
        let first = matches.first_mut().expect("rule should declare one match");
        first.headers = Some(vec![HttpRouteRulesMatchesHeaders {
            name: "x-test".to_owned(),
            value: "value".to_owned(),
            r#type: Some(kind),
        }]);
    }

    /// Adds a query-parameter match of the given type to a rule's first
    /// match.
    fn set_query_match(rule: &mut HttpRouteRules, kind: HttpRouteRulesMatchesQueryParamsType) {
        let matches = rule.matches.as_mut().expect("rule should declare matches");
        let first = matches.first_mut().expect("rule should declare one match");
        first.query_params = Some(vec![HttpRouteRulesMatchesQueryParams {
            name: "q".to_owned(),
            value: "value".to_owned(),
            r#type: Some(kind),
        }]);
    }
}
