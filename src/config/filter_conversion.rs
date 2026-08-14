// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Filter conversion from Gateway API filter types to Praxis filter config.

use gateway_api::httproutes::{
    HttpRouteRules, HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
    HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersResponseHeaderModifier, HttpRouteRulesFiltersType,
    HttpRouteRulesMatches, HttpRouteRulesMatchesPathType,
};
use serde::Serialize;
use tracing::warn;

use super::routing::PraxisFilterEntry;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Redirect status used when the route names none, or names one Praxis
/// cannot accept.
const DEFAULT_REDIRECT_STATUS: u16 = 302;

// -----------------------------------------------------------------------------
// HeaderEntry
// -----------------------------------------------------------------------------

/// Header modification entry.
///
/// Represents a header name-value pair for modification filters.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct HeaderEntry {
    /// Header name.
    name: String,

    /// Header value.
    value: String,
}

// -----------------------------------------------------------------------------
// HeaderFilterConfig
// -----------------------------------------------------------------------------

/// Header filter configuration matching the Praxis `HeaderFilterConfig`.
///
/// Maps to all six fields in the proxy's `deny_unknown_fields` schema:
/// `request_add`, `request_set`, `request_remove`, `response_add`,
/// `response_set`, and `response_remove`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
struct HeaderFilterConfig {
    /// Headers to add to the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_add: Option<Vec<HeaderEntry>>,

    /// Header names to remove from the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_remove: Option<Vec<String>>,

    /// Headers to set on the request (overwrites existing values).
    #[serde(skip_serializing_if = "Option::is_none")]
    request_set: Option<Vec<HeaderEntry>>,

    /// Headers to add to the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_add: Option<Vec<HeaderEntry>>,

    /// Header names to remove from the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_remove: Option<Vec<String>>,

    /// Headers to set on the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_set: Option<Vec<HeaderEntry>>,
}

// -----------------------------------------------------------------------------
// RedirectFilterConfig
// -----------------------------------------------------------------------------

/// Redirect filter configuration matching the Praxis `RedirectConfig`.
///
/// The proxy expects `status` (u16) and `location` (URL template with
/// `${path}` and `${query}` placeholders).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct RedirectFilterConfig {
    /// HTTP redirect status code (301, 302, 307, 308).
    status: u16,

    /// Location URL template with `${path}` and `${query}` placeholders.
    location: String,
}

// -----------------------------------------------------------------------------
// RouteFilters
// -----------------------------------------------------------------------------

/// The filters one Gateway's routes contribute, split by where in the
/// chain they have to run.
///
/// Praxis evaluates a chain in order and the `router` filter sits in the
/// middle of it, so a route filter's position is not a matter of taste.
/// The router picks a cluster from the request's path and `Host` header,
/// reading `ctx.rewritten_path` in preference to the original URI, and
/// rejects with 404 when nothing matches.
///
/// That splits route filters in two. A filter that answers the request
/// itself belongs before the router, because the rule it came from has
/// no backend and therefore no route for the router to find. A filter
/// that changes what the router reads — the path or the `Host` header —
/// belongs after it, because Gateway API selects the rule from the
/// request as it arrived and applies the rule's transformations to what
/// is forwarded upstream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteFilters {
    /// Filters that answer the request without an upstream.
    ///
    /// Run before the router, which would otherwise 404 the traffic
    /// they exist to serve.
    pub terminating: Vec<PraxisFilterEntry>,

    /// Filters that reshape a request the router has already placed.
    ///
    /// Run after the router, so route selection sees the request the
    /// client sent.
    pub transforming: Vec<PraxisFilterEntry>,
}

// -----------------------------------------------------------------------------
// Filter Conversion
// -----------------------------------------------------------------------------

/// Converts `HTTPRoute` filters to Praxis filter configurations.
///
/// Each rule produces its own filter entries scoped by Praxis conditional
/// filters (`conditions`) derived from the rule's path match. This ensures
/// header modifications and redirects apply only to traffic matching the
/// originating rule.
pub fn convert_filters(rules: &[HttpRouteRules]) -> RouteFilters {
    let scoped = scope_rules(rules);
    let mut filters = RouteFilters::default();
    for (index, rule) in rules.iter().enumerate() {
        let condition = rule_conditions(index, &scoped);
        let has_backends = rule.backend_refs.as_ref().is_some_and(|refs| !refs.is_empty());
        let has_redirect = rule_has_redirect(rule);
        if !has_backends && !has_redirect {
            emit_no_backend_response(&condition, &mut filters.terminating);
        }
        if rule.filters.is_some() {
            convert_rule_filters(rule, &condition, &mut filters);
        }
    }
    filters
}

/// Checks if a rule has a `RequestRedirect` filter.
fn rule_has_redirect(rule: &HttpRouteRules) -> bool {
    rule.filters.as_ref().is_some_and(|fs| {
        fs.iter()
            .any(|f| f.r#type == HttpRouteRulesFiltersType::RequestRedirect)
    })
}

/// Converts filters from a single rule into conditional filter entries.
fn convert_rule_filters(rule: &HttpRouteRules, condition: &Option<yaml_serde::Value>, filters: &mut RouteFilters) {
    let Some(rule_filters) = &rule.filters else {
        return;
    };

    let mut header_config = HeaderFilterConfig::default();
    let mut has_header_mods = false;

    for filter in rule_filters {
        has_header_mods |= dispatch_filter(filter, condition, &mut header_config, filters);
    }

    if has_header_mods {
        emit_conditional_header_filter(&header_config, condition, &mut filters.transforming);
    }
}

/// Dispatches a single filter to the appropriate handler.
///
/// Returns `true` if header config was modified.
fn dispatch_filter(
    filter: &HttpRouteRulesFilters,
    condition: &Option<yaml_serde::Value>,
    header_config: &mut HeaderFilterConfig,
    filters: &mut RouteFilters,
) -> bool {
    match &filter.r#type {
        HttpRouteRulesFiltersType::RequestHeaderModifier => dispatch_request_header(filter, header_config),
        HttpRouteRulesFiltersType::ResponseHeaderModifier => dispatch_response_header(filter, header_config),
        HttpRouteRulesFiltersType::RequestRedirect => {
            if let Some(redirect) = &filter.request_redirect {
                emit_conditional_redirect(redirect, condition, &mut filters.terminating);
            }
            false
        },
        other => {
            warn!(?other, "unsupported filter type, ignoring");
            false
        },
    }
}

type Predicate = yaml_serde::Mapping;

/// How a rule ranks against a sibling when both could match a request.
///
/// Gateway API resolves an overlap by path specificity first — an
/// exact match over any prefix, then the longer prefix — and breaks
/// the remaining tie on the number of header matches. Field order here
/// is the comparison order the derived `Ord` uses, so it has to stay in
/// that sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Precedence {
    /// An exact path match outranks every prefix match.
    exact_path: bool,

    /// A longer prefix outranks a shorter one.
    path_len: usize,

    /// More header matches break a path tie.
    headers: usize,
}

/// A rule's predicate paired with the rank it holds among its siblings.
struct ScopedRule {
    /// The traffic this rule claims.
    predicate: Predicate,

    /// Where the rule sits in the overlap ordering.
    precedence: Precedence,
}

/// Describes every rule by the traffic it claims and its rank.
fn scope_rules(rules: &[HttpRouteRules]) -> Vec<ScopedRule> {
    rules.iter().map(scope_rule).collect()
}

/// Describes one rule by the traffic it claims and its rank.
///
/// Only the first match is read. A rule listing several matches claims
/// the union of them, which a single `ConditionMatch` cannot express.
fn scope_rule(rule: &HttpRouteRules) -> ScopedRule {
    let first = rule.matches.as_deref().and_then(<[_]>::first);
    ScopedRule {
        predicate: first.map(rule_predicate).unwrap_or_default(),
        precedence: first.map(rule_precedence).unwrap_or_default(),
    }
}

/// Builds the predicate matching one rule's traffic.
fn rule_predicate(m: &HttpRouteRulesMatches) -> Predicate {
    let mut predicate = Predicate::new();
    insert_path_predicate(m, &mut predicate);
    insert_header_predicate(m, &mut predicate);
    predicate
}

/// Ranks one rule's match against its siblings.
fn rule_precedence(m: &HttpRouteRulesMatches) -> Precedence {
    let path = m.path.as_ref();
    Precedence {
        exact_path: matches!(
            path.and_then(|p| p.r#type.as_ref()),
            Some(HttpRouteRulesMatchesPathType::Exact)
        ),
        path_len: path.and_then(|p| p.value.as_deref()).map_or(0, str::len),
        headers: m.headers.as_deref().map_or(0, <[_]>::len),
    }
}

/// Builds the Praxis `conditions` list scoping one rule's filters.
///
/// The rule's own predicate becomes a `when`, and the predicate of
/// every sibling that outranks it becomes an `unless`. The `unless`
/// clauses are what keep a chain-level filter inside its own rule:
/// Gateway API hands overlapping traffic to the higher-ranked rule, so
/// the loser's filters must not touch it. Without them a rule that
/// declares no `matches` at all — which claims every request — would
/// rewrite or re-header the traffic of every other rule on the
/// listener.
///
/// A sibling whose traffic cannot overlap is still listed. Its
/// predicate then only ever fails against requests that already fail
/// this rule's own `when`, so the extra clause costs a comparison and
/// changes nothing.
fn rule_conditions(index: usize, scoped: &[ScopedRule]) -> Option<yaml_serde::Value> {
    let own = scoped.get(index)?;

    let mut conditions = Vec::new();
    if !own.predicate.is_empty() {
        conditions.push(condition_entry("when", &own.predicate));
    }
    for predicate in outranking_predicates(own, scoped) {
        conditions.push(condition_entry("unless", predicate));
    }

    (!conditions.is_empty()).then(|| yaml_serde::Value::Sequence(conditions))
}

/// Collects the distinct predicates of the rules that outrank `own`.
fn outranking_predicates<'a>(own: &ScopedRule, scoped: &'a [ScopedRule]) -> Vec<&'a Predicate> {
    let mut collected: Vec<&Predicate> = Vec::new();
    for other in scoped {
        let outranks = other.precedence > own.precedence && !other.predicate.is_empty();
        if outranks && !collected.contains(&&other.predicate) {
            collected.push(&other.predicate);
        }
    }
    collected
}

/// Wraps a predicate in a `when` or `unless` condition entry.
fn condition_entry(keyword: &str, predicate: &Predicate) -> yaml_serde::Value {
    yaml_serde::Value::Mapping(yaml_serde::Mapping::from_iter([(
        yaml_serde::Value::String(keyword.to_owned()),
        yaml_serde::Value::Mapping(predicate.clone()),
    )]))
}

/// Adds the path constraint to a filter predicate.
///
/// An `Exact` match uses the Praxis `path` field and a `PathPrefix`
/// match uses `path_prefix`. Collapsing both onto `path_prefix`, as this
/// did before, made a filter scoped to exactly `/foo` fire on `/foo/bar`
/// as well.
fn insert_path_predicate(m: &HttpRouteRulesMatches, predicate: &mut yaml_serde::Mapping) {
    let Some(path) = m.path.as_ref() else { return };
    let Some(value) = path.value.as_deref() else { return };

    let field = match &path.r#type {
        Some(HttpRouteRulesMatchesPathType::Exact) => "path",
        Some(HttpRouteRulesMatchesPathType::PathPrefix) => "path_prefix",
        _ => return,
    };

    predicate.insert(
        yaml_serde::Value::String(field.to_owned()),
        yaml_serde::Value::String(value.to_owned()),
    );
}

/// Adds the rule's header constraints to a filter predicate.
///
/// Narrows the filter to the traffic its own rule matches. Without it a
/// header modifier written for one route also fires for any other route
/// sharing its path on the same listener.
fn insert_header_predicate(m: &HttpRouteRulesMatches, predicate: &mut yaml_serde::Mapping) {
    let Some(headers) = m.headers.as_deref().filter(|h| !h.is_empty()) else {
        return;
    };

    let mut mapping = yaml_serde::Mapping::new();
    for header in headers {
        mapping.insert(
            yaml_serde::Value::String(header.name.clone()),
            yaml_serde::Value::String(header.value.clone()),
        );
    }

    predicate.insert(
        yaml_serde::Value::String("headers".to_owned()),
        yaml_serde::Value::Mapping(mapping),
    );
}

/// Dispatches a request header modifier filter.
fn dispatch_request_header(filter: &HttpRouteRulesFilters, config: &mut HeaderFilterConfig) -> bool {
    filter
        .request_header_modifier
        .as_ref()
        .is_some_and(|m| process_request_header_modifier(m, config))
}

/// Dispatches a response header modifier filter.
fn dispatch_response_header(filter: &HttpRouteRulesFilters, config: &mut HeaderFilterConfig) -> bool {
    filter
        .response_header_modifier
        .as_ref()
        .is_some_and(|m| process_response_header_modifier(m, config))
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Processes a request header modifier into the accumulated header config.
///
/// Maps Gateway API `add` to `request_add`, `set` to `request_set`,
/// and `remove` to `request_remove`.
///
/// Returns `true` if any modifications were applied.
fn process_request_header_modifier(
    modifier: &HttpRouteRulesFiltersRequestHeaderModifier,
    config: &mut HeaderFilterConfig,
) -> bool {
    let mut modified = false;
    modified |= collect_request_add(&modifier.add, config);
    modified |= collect_request_set(&modifier.set, config);
    modified |= collect_request_remove(&modifier.remove, config);
    modified
}

/// Collects `add` headers into `request_add`.
fn collect_request_add(
    add: &Option<Vec<gateway_api::httproutes::HttpRouteRulesFiltersRequestHeaderModifierAdd>>,
    config: &mut HeaderFilterConfig,
) -> bool {
    let Some(headers) = add else { return false };
    let entries = to_header_entries(headers.iter().map(|h| (&h.name, &h.value)));
    config.request_add.get_or_insert_with(Vec::new).extend(entries);
    true
}

/// Maps `set` headers to `request_set` (overwrite semantics).
fn collect_request_set(
    set: &Option<Vec<gateway_api::httproutes::HttpRouteRulesFiltersRequestHeaderModifierSet>>,
    config: &mut HeaderFilterConfig,
) -> bool {
    let Some(headers) = set else { return false };
    let entries = to_header_entries(headers.iter().map(|h| (&h.name, &h.value)));
    config.request_set.get_or_insert_with(Vec::new).extend(entries);
    true
}

/// Maps `remove` headers to `request_remove`.
fn collect_request_remove(remove: &Option<Vec<String>>, config: &mut HeaderFilterConfig) -> bool {
    let Some(headers) = remove else { return false };
    config
        .request_remove
        .get_or_insert_with(Vec::new)
        .extend(headers.iter().cloned());
    true
}

/// Processes a response header modifier into the accumulated header config.
///
/// Returns `true` if any modifications were applied.
fn process_response_header_modifier(
    modifier: &HttpRouteRulesFiltersResponseHeaderModifier,
    config: &mut HeaderFilterConfig,
) -> bool {
    let mut modified = false;

    if let Some(add_headers) = &modifier.add {
        let entries = to_header_entries(add_headers.iter().map(|h| (&h.name, &h.value)));
        config.response_add.get_or_insert_with(Vec::new).extend(entries);
        modified = true;
    }
    if let Some(set_headers) = &modifier.set {
        let entries = to_header_entries(set_headers.iter().map(|h| (&h.name, &h.value)));
        config.response_set.get_or_insert_with(Vec::new).extend(entries);
        modified = true;
    }
    if let Some(remove_headers) = &modifier.remove {
        config
            .response_remove
            .get_or_insert_with(Vec::new)
            .extend(remove_headers.iter().cloned());
        modified = true;
    }

    modified
}

/// Converts name-value pairs into [`HeaderEntry`] values.
fn to_header_entries<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> Vec<HeaderEntry> {
    pairs
        .map(|(name, value)| HeaderEntry {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Emits a conditional redirect filter entry.
///
/// Builds a Praxis `location` URL template from Gateway API redirect
/// fields (scheme, hostname, port) with `${path}${query}` placeholders.
fn emit_conditional_redirect(
    redirect: &gateway_api::httproutes::HttpRouteRulesFiltersRequestRedirect,
    condition: &Option<yaml_serde::Value>,
    filters: &mut Vec<PraxisFilterEntry>,
) {
    let location = build_redirect_location(redirect);
    let status = redirect_status(redirect.status_code);

    let redirect_config = RedirectFilterConfig { status, location };

    match yaml_serde::to_value(&redirect_config) {
        Ok(config) => {
            let config = inject_conditions(config, condition);
            filters.push(PraxisFilterEntry {
                filter: "redirect".to_owned(),
                config,
            });
        },
        Err(err) => warn!(%err, "failed to serialize redirect filter config"),
    }
}

/// Maps a Gateway API redirect status onto one Praxis accepts.
///
/// Praxis deserializes this field through a `TryFrom<u16>` limited to
/// 301, 302, 307 and 308, and its filter config denies unknown values,
/// so an out-of-range status does not degrade one redirect — it fails
/// the whole document and leaves that Gateway's data plane without a
/// config. Anything unrecognised falls back to the Gateway API default
/// rather than being passed through.
fn redirect_status(status_code: Option<i64>) -> u16 {
    match status_code {
        Some(301) => 301,
        Some(307) => 307,
        Some(308) => 308,
        Some(302) | None => DEFAULT_REDIRECT_STATUS,
        Some(other) => {
            warn!(status = other, "unsupported redirect status, falling back to 302");
            DEFAULT_REDIRECT_STATUS
        },
    }
}

/// Builds a redirect location URL template from Gateway API fields.
fn build_redirect_location(redirect: &gateway_api::httproutes::HttpRouteRulesFiltersRequestRedirect) -> String {
    let scheme = redirect.scheme.as_ref().map(|s| match s {
        HttpRouteRulesFiltersRequestRedirectScheme::Http => "http",
        HttpRouteRulesFiltersRequestRedirectScheme::Https => "https",
    });
    let hostname = redirect.hostname.as_deref().unwrap_or("${host}");

    match (scheme, redirect.port) {
        (Some(s), Some(p)) => format!("{s}://{hostname}:{p}${{path}}${{query}}"),
        (Some(s), None) => format!("{s}://{hostname}${{path}}${{query}}"),
        (None, Some(p)) => format!("${{scheme}}://{hostname}:{p}${{path}}${{query}}"),
        (None, None) => format!("${{scheme}}://{hostname}${{path}}${{query}}"),
    }
}

/// Emits a conditional header filter entry.
fn emit_conditional_header_filter(
    config: &HeaderFilterConfig,
    condition: &Option<yaml_serde::Value>,
    filters: &mut Vec<PraxisFilterEntry>,
) {
    match yaml_serde::to_value(config) {
        Ok(config) => {
            let config = inject_conditions(config, condition);
            filters.push(PraxisFilterEntry {
                filter: "headers".to_owned(),
                config,
            });
        },
        Err(err) => warn!(%err, "failed to serialize header filter config"),
    }
}

/// Emits a `static_response` filter returning 500 for rules with no backends.
fn emit_no_backend_response(condition: &Option<yaml_serde::Value>, filters: &mut Vec<PraxisFilterEntry>) {
    let mut config = yaml_serde::Mapping::new();
    config.insert(
        yaml_serde::Value::String("status".to_owned()),
        yaml_serde::Value::Number(500.into()),
    );
    config.insert(
        yaml_serde::Value::String("body".to_owned()),
        yaml_serde::Value::String("no backends available".to_owned()),
    );
    let config = inject_conditions(yaml_serde::Value::Mapping(config), condition);
    filters.push(PraxisFilterEntry {
        filter: "static_response".to_owned(),
        config,
    });
}

/// Injects `conditions` into a filter config mapping.
fn inject_conditions(mut config: yaml_serde::Value, condition: &Option<yaml_serde::Value>) -> yaml_serde::Value {
    if let (Some(cond), Some(map)) = (condition, config.as_mapping_mut()) {
        map.insert(yaml_serde::Value::String("conditions".to_owned()), cond.clone());
    }
    config
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::too_many_lines, reason = "tests")]
mod tests {
    use gateway_api::httproutes::{HttpRouteRules, HttpRouteRulesBackendRefs};

    use super::*;

    fn dummy_backend_refs() -> Vec<HttpRouteRulesBackendRefs> {
        vec![HttpRouteRulesBackendRefs {
            name: "dummy".to_owned(),
            port: Some(80),
            ..Default::default()
        }]
    }

    #[test]
    fn test_convert_filters_request_header_modifier() {
        use gateway_api::httproutes::{
            HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
            HttpRouteRulesFiltersRequestHeaderModifierAdd, HttpRouteRulesFiltersRequestHeaderModifierSet,
        };

        let rules = vec![HttpRouteRules {
            backend_refs: Some(dummy_backend_refs()),
            filters: Some(vec![HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
                request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                    add: Some(vec![HttpRouteRulesFiltersRequestHeaderModifierAdd {
                        name: "X-Custom".to_owned(),
                        value: "custom-value".to_owned(),
                    }]),
                    set: Some(vec![HttpRouteRulesFiltersRequestHeaderModifierSet {
                        name: "X-Override".to_owned(),
                        value: "override-value".to_owned(),
                    }]),
                    remove: Some(vec!["X-Remove".to_owned()]),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        let filters = convert_filters(&rules);

        let transforming = &filters.transforming;
        assert_eq!(transforming.len(), 1, "should produce one header filter");
        assert_eq!(transforming[0].filter, "headers", "filter name should be headers");
        assert!(
            filters.terminating.is_empty(),
            "a header modifier does not answer the request, so nothing runs before the router"
        );

        let config_str = yaml_serde::to_string(&transforming[0].config).unwrap();
        assert!(
            config_str.contains("request_add"),
            "should have request_add for added headers"
        );
        assert!(config_str.contains("X-Custom"), "should contain added header");
        assert!(
            config_str.contains("request_set"),
            "set headers should map to request_set"
        );
        assert!(config_str.contains("X-Override"), "should contain set header");
        assert!(
            config_str.contains("request_remove"),
            "remove should map to request_remove"
        );
        assert!(config_str.contains("X-Remove"), "should contain removed header name");
    }

    #[test]
    fn test_convert_filters_response_header_modifier() {
        use gateway_api::httproutes::{
            HttpRouteRulesFilters, HttpRouteRulesFiltersResponseHeaderModifier,
            HttpRouteRulesFiltersResponseHeaderModifierAdd,
        };

        let rules = vec![HttpRouteRules {
            backend_refs: Some(dummy_backend_refs()),
            filters: Some(vec![HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::ResponseHeaderModifier,
                response_header_modifier: Some(HttpRouteRulesFiltersResponseHeaderModifier {
                    add: Some(vec![HttpRouteRulesFiltersResponseHeaderModifierAdd {
                        name: "X-Response".to_owned(),
                        value: "response-value".to_owned(),
                    }]),
                    remove: Some(vec!["X-Remove-Response".to_owned()]),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        let filters = convert_filters(&rules);

        assert_eq!(filters.transforming.len(), 1, "should produce one header filter");
        let config_str = yaml_serde::to_string(&filters.transforming[0].config).unwrap();
        assert!(config_str.contains("X-Response"), "should contain response header");
        assert!(
            config_str.contains("X-Remove-Response"),
            "should contain removed response header"
        );
    }

    #[test]
    fn test_convert_filters_request_redirect() {
        use gateway_api::httproutes::{HttpRouteRulesFilters, HttpRouteRulesFiltersRequestRedirect};

        let rules = vec![HttpRouteRules {
            backend_refs: Some(dummy_backend_refs()),
            filters: Some(vec![HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::RequestRedirect,
                request_redirect: Some(HttpRouteRulesFiltersRequestRedirect {
                    status_code: Some(302),
                    scheme: Some(HttpRouteRulesFiltersRequestRedirectScheme::Https),
                    hostname: Some("example.com".to_owned()),
                    port: Some(443),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        let filters = convert_filters(&rules);

        let terminating = &filters.terminating;
        assert_eq!(terminating.len(), 1, "should produce one redirect filter");
        assert_eq!(terminating[0].filter, "redirect", "filter name should be redirect");
        assert!(
            filters.transforming.is_empty(),
            "a redirect answers the request itself and never reaches an upstream"
        );

        let config_str = yaml_serde::to_string(&terminating[0].config).unwrap();
        assert!(config_str.contains("302"), "should contain status code");
        assert!(
            config_str.contains("https://example.com:443"),
            "location should contain scheme, hostname, and port"
        );
    }

    #[test]
    fn test_convert_filters_mixed() {
        use gateway_api::httproutes::{
            HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
            HttpRouteRulesFiltersRequestHeaderModifierAdd, HttpRouteRulesFiltersRequestRedirect,
        };

        let rules = vec![HttpRouteRules {
            backend_refs: Some(dummy_backend_refs()),
            filters: Some(vec![
                HttpRouteRulesFilters {
                    r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
                    request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                        add: Some(vec![HttpRouteRulesFiltersRequestHeaderModifierAdd {
                            name: "X-Header".to_owned(),
                            value: "value".to_owned(),
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                HttpRouteRulesFilters {
                    r#type: HttpRouteRulesFiltersType::RequestRedirect,
                    request_redirect: Some(HttpRouteRulesFiltersRequestRedirect {
                        status_code: Some(301),
                        scheme: Some(HttpRouteRulesFiltersRequestRedirectScheme::Https),
                        hostname: Some("example.com".to_owned()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }];

        let filters = convert_filters(&rules);

        assert!(
            filters.terminating.iter().any(|f| f.filter == "redirect"),
            "the redirect answers the request, so it runs before the router"
        );
        assert!(
            filters.transforming.iter().any(|f| f.filter == "headers"),
            "the header modifier reshapes a routed request, so it runs after the router"
        );
    }

    #[test]
    fn test_convert_filters_per_rule_conditions() {
        use gateway_api::httproutes::{
            HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
            HttpRouteRulesFiltersRequestHeaderModifierAdd, HttpRouteRulesMatches, HttpRouteRulesMatchesPath,
        };

        let rules = vec![
            HttpRouteRules {
                backend_refs: Some(dummy_backend_refs()),
                matches: Some(vec![HttpRouteRulesMatches {
                    path: Some(HttpRouteRulesMatchesPath {
                        r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/set".to_owned()),
                    }),
                    ..Default::default()
                }]),
                filters: Some(vec![HttpRouteRulesFilters {
                    r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
                    request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                        add: Some(vec![HttpRouteRulesFiltersRequestHeaderModifierAdd {
                            name: "X-First".to_owned(),
                            value: "first".to_owned(),
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            HttpRouteRules {
                backend_refs: Some(dummy_backend_refs()),
                matches: Some(vec![HttpRouteRulesMatches {
                    path: Some(HttpRouteRulesMatchesPath {
                        r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
                        value: Some("/add".to_owned()),
                    }),
                    ..Default::default()
                }]),
                filters: Some(vec![HttpRouteRulesFilters {
                    r#type: HttpRouteRulesFiltersType::RequestHeaderModifier,
                    request_header_modifier: Some(HttpRouteRulesFiltersRequestHeaderModifier {
                        add: Some(vec![HttpRouteRulesFiltersRequestHeaderModifierAdd {
                            name: "X-Second".to_owned(),
                            value: "second".to_owned(),
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ];

        let filters = convert_filters(&rules);

        let transforming = &filters.transforming;
        assert_eq!(transforming.len(), 2, "should produce one filter per rule");

        let first_yaml = yaml_serde::to_string(&transforming[0].config).unwrap();
        assert!(
            first_yaml.contains("X-First"),
            "first filter should have X-First header"
        );
        assert!(
            first_yaml.contains("path_prefix"),
            "first filter should have path_prefix condition"
        );
        assert!(
            first_yaml.contains("/set"),
            "first filter should be conditioned on /set"
        );

        let second_yaml = yaml_serde::to_string(&transforming[1].config).unwrap();
        assert!(
            second_yaml.contains("X-Second"),
            "second filter should have X-Second header"
        );
        assert!(
            second_yaml.contains("/add"),
            "second filter should be conditioned on /add"
        );
    }

    // -----------------------------------------------------------------------
    // Condition Scoping
    // -----------------------------------------------------------------------

    #[test]
    fn test_exact_path_scopes_on_path_not_prefix() {
        let rule = rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/foo");
        let cond = lone_rule_condition(&rule).expect("an exact path should produce a condition");
        let when = &cond[0]["when"];

        assert_eq!(
            when["path"],
            yaml_serde::Value::String("/foo".to_owned()),
            "an Exact match must scope on the Praxis path field"
        );
        assert!(
            when.get("path_prefix").is_none(),
            "using path_prefix for an Exact match would fire the filter on /foo/bar too"
        );
    }

    #[test]
    fn test_prefix_path_scopes_on_path_prefix() {
        let rule = rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/api");
        let cond = lone_rule_condition(&rule).expect("a prefix path should produce a condition");

        assert_eq!(
            cond[0]["when"]["path_prefix"],
            yaml_serde::Value::String("/api".to_owned()),
            "a PathPrefix match must scope on path_prefix"
        );
    }

    #[test]
    fn test_header_constraints_narrow_the_condition() {
        let mut rule = rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/api");
        if let Some(matches) = rule.matches.as_mut()
            && let Some(first) = matches.first_mut()
        {
            first.headers = Some(vec![gateway_api::httproutes::HttpRouteRulesMatchesHeaders {
                name: "x-tenant".to_owned(),
                value: "acme".to_owned(),
                r#type: None,
            }]);
        }

        let cond = lone_rule_condition(&rule).expect("condition expected");

        assert_eq!(
            cond[0]["when"]["headers"]["x-tenant"],
            yaml_serde::Value::String("acme".to_owned()),
            "a rule's header match must scope its filters, or the filter fires for other routes \
             sharing the same path on this listener"
        );
    }

    #[test]
    fn test_header_only_rule_still_produces_a_condition() {
        let rule = HttpRouteRules {
            matches: Some(vec![HttpRouteRulesMatches {
                headers: Some(vec![gateway_api::httproutes::HttpRouteRulesMatchesHeaders {
                    name: "x-canary".to_owned(),
                    value: "true".to_owned(),
                    r#type: None,
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let cond = lone_rule_condition(&rule).expect("a header-only rule should still be scoped");

        assert!(
            cond[0]["when"].get("headers").is_some(),
            "a rule constrained only by headers must not produce an unscoped filter"
        );
    }

    #[test]
    fn test_unconstrained_rule_has_no_condition() {
        let rule = HttpRouteRules {
            matches: Some(vec![HttpRouteRulesMatches::default()]),
            ..Default::default()
        };

        assert!(
            lone_rule_condition(&rule).is_none(),
            "a rule with nothing to match on cannot be scoped and must stay unconditional"
        );
    }

    #[test]
    fn test_catch_all_rule_is_scoped_away_from_its_siblings() {
        let rules = vec![
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one"),
            HttpRouteRules::default(),
        ];
        let scoped = scope_rules(&rules);

        let cond = rule_conditions(1, &scoped).expect("a catch-all rule beside a narrower one must be scoped");

        assert_eq!(
            cond[0]["unless"]["path_prefix"],
            yaml_serde::Value::String("/one".to_owned()),
            "Gateway API gives /one traffic to the narrower rule, so the catch-all rule's filters \
             must skip it — without this the catch-all rewrites every request on the listener"
        );
        assert!(
            cond[0].get("when").is_none(),
            "a catch-all rule claims no traffic of its own to gate on"
        );
    }

    #[test]
    fn test_a_narrower_rule_is_not_scoped_against_a_broader_one() {
        let rules = vec![
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one"),
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one/two"),
        ];
        let scoped = scope_rules(&rules);

        let cond = rule_conditions(1, &scoped).expect("a rule with a path match is always scoped");

        assert_eq!(
            cond.as_sequence().map(Vec::len),
            Some(1),
            "the longer prefix wins the overlap, so it needs no unless clause against the shorter one"
        );
    }

    #[test]
    fn test_an_exact_match_outranks_a_longer_prefix() {
        let rules = vec![
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one/two/three"),
            rule_with_path(HttpRouteRulesMatchesPathType::Exact, "/one"),
        ];
        let scoped = scope_rules(&rules);

        let cond = rule_conditions(0, &scoped).expect("a rule with a path match is always scoped");

        assert_eq!(
            cond[1]["unless"]["path"],
            yaml_serde::Value::String("/one".to_owned()),
            "Gateway API ranks an exact match above any prefix, however long"
        );
    }

    #[test]
    fn test_identical_predicates_are_listed_once() {
        let rules = vec![
            HttpRouteRules::default(),
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one"),
            rule_with_path(HttpRouteRulesMatchesPathType::PathPrefix, "/one"),
        ];
        let scoped = scope_rules(&rules);

        let cond = rule_conditions(0, &scoped).expect("a catch-all rule beside narrower ones must be scoped");

        assert_eq!(
            cond.as_sequence().map(Vec::len),
            Some(1),
            "two siblings claiming the same traffic need one unless clause, not two"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Scopes a rule that has no siblings to be scoped against.
    fn lone_rule_condition(rule: &HttpRouteRules) -> Option<yaml_serde::Value> {
        let rules = [rule.clone()];
        rule_conditions(0, &scope_rules(&rules))
    }

    /// Builds a rule with a single path match of the given type.
    fn rule_with_path(kind: HttpRouteRulesMatchesPathType, value: &str) -> HttpRouteRules {
        HttpRouteRules {
            matches: Some(vec![HttpRouteRulesMatches {
                path: Some(gateway_api::httproutes::HttpRouteRulesMatchesPath {
                    r#type: Some(kind),
                    value: Some(value.to_owned()),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Redirect Status
    // -----------------------------------------------------------------------

    #[test]
    fn test_supported_redirect_statuses_pass_through() {
        for code in [301_i64, 302, 307, 308] {
            assert_eq!(
                i64::from(redirect_status(Some(code))),
                code,
                "Praxis accepts {code} and it should reach the config unchanged"
            );
        }
    }

    #[test]
    fn test_unsupported_redirect_status_falls_back() {
        for code in [200_i64, 303, 399, -1, 99999] {
            assert_eq!(
                redirect_status(Some(code)),
                DEFAULT_REDIRECT_STATUS,
                "Praxis denies unknown redirect statuses and rejects the whole document, so {code} \
                 must never be emitted"
            );
        }
    }

    #[test]
    fn test_absent_redirect_status_uses_the_spec_default() {
        assert_eq!(
            redirect_status(None),
            302,
            "the Gateway API default for an unspecified redirect status is 302"
        );
    }
}
