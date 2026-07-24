// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Filter conversion from Gateway API filter types to Praxis filter config.

use gateway_api::httproutes::{
    HttpRouteRules, HttpRouteRulesFilters, HttpRouteRulesFiltersRequestHeaderModifier,
    HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersResponseHeaderModifier, HttpRouteRulesFiltersType,
    HttpRouteRulesMatchesPathType,
};
use serde::Serialize;
use tracing::warn;

use super::routing::PraxisFilterEntry;

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
// Filter Conversion
// -----------------------------------------------------------------------------

/// Converts `HTTPRoute` filters to Praxis filter configurations.
///
/// Each rule produces its own filter entries scoped by Praxis conditional
/// filters (`conditions`) derived from the rule's path match. This ensures
/// header modifications and redirects apply only to traffic matching the
/// originating rule.
pub(crate) fn convert_filters(rules: &[HttpRouteRules]) -> Vec<PraxisFilterEntry> {
    let mut filters = Vec::new();
    for rule in rules {
        let has_backends = rule.backend_refs.as_ref().is_some_and(|refs| !refs.is_empty());
        let has_redirect = rule_has_redirect(rule);
        if !has_backends && !has_redirect {
            emit_no_backend_response(rule, &mut filters);
        }
        if rule.filters.is_some() {
            convert_rule_filters(rule, &mut filters);
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
fn convert_rule_filters(rule: &HttpRouteRules, filters: &mut Vec<PraxisFilterEntry>) {
    let Some(rule_filters) = &rule.filters else {
        return;
    };

    let condition = extract_rule_condition(rule);
    let mut header_config = HeaderFilterConfig::default();
    let mut has_header_mods = false;

    for filter in rule_filters {
        has_header_mods |= dispatch_filter(filter, &condition, &mut header_config, filters);
    }

    if has_header_mods {
        emit_conditional_header_filter(&header_config, &condition, filters);
    }
}

/// Dispatches a single filter to the appropriate handler.
///
/// Returns `true` if header config was modified.
fn dispatch_filter(
    filter: &HttpRouteRulesFilters,
    condition: &Option<serde_yaml::Value>,
    header_config: &mut HeaderFilterConfig,
    filters: &mut Vec<PraxisFilterEntry>,
) -> bool {
    match &filter.r#type {
        HttpRouteRulesFiltersType::RequestHeaderModifier => dispatch_request_header(filter, header_config),
        HttpRouteRulesFiltersType::ResponseHeaderModifier => dispatch_response_header(filter, header_config),
        HttpRouteRulesFiltersType::RequestRedirect => {
            if let Some(redirect) = &filter.request_redirect {
                emit_conditional_redirect(redirect, condition, filters);
            }
            false
        },
        other => {
            warn!(?other, "unsupported filter type, ignoring");
            false
        },
    }
}

/// Extracts a Praxis condition from a rule's first path match.
///
/// Returns a YAML value suitable for the `conditions` field of a Praxis
/// filter, or `None` for catch-all rules without path constraints.
fn extract_rule_condition(rule: &HttpRouteRules) -> Option<serde_yaml::Value> {
    let matches = rule.matches.as_ref()?;
    let first = matches.first()?;
    let path = first.path.as_ref()?;
    let value = path.value.as_deref()?;

    let field = match &path.r#type {
        Some(HttpRouteRulesMatchesPathType::PathPrefix | HttpRouteRulesMatchesPathType::Exact) => "path_prefix",
        _ => return None,
    };

    let when = serde_yaml::Mapping::from_iter([(
        serde_yaml::Value::String(field.to_owned()),
        serde_yaml::Value::String(value.to_owned()),
    )]);
    let entry = serde_yaml::Mapping::from_iter([(
        serde_yaml::Value::String("when".to_owned()),
        serde_yaml::Value::Mapping(when),
    )]);

    Some(serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(entry)]))
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
    condition: &Option<serde_yaml::Value>,
    filters: &mut Vec<PraxisFilterEntry>,
) {
    let location = build_redirect_location(redirect);
    let status = u16::try_from(redirect.status_code.unwrap_or(302)).unwrap_or(302);

    let redirect_config = RedirectFilterConfig { status, location };

    match serde_yaml::to_value(&redirect_config) {
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
    condition: &Option<serde_yaml::Value>,
    filters: &mut Vec<PraxisFilterEntry>,
) {
    match serde_yaml::to_value(config) {
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
fn emit_no_backend_response(rule: &HttpRouteRules, filters: &mut Vec<PraxisFilterEntry>) {
    let condition = extract_rule_condition(rule);
    let mut config = serde_yaml::Mapping::new();
    config.insert(
        serde_yaml::Value::String("status".to_owned()),
        serde_yaml::Value::Number(500.into()),
    );
    config.insert(
        serde_yaml::Value::String("body".to_owned()),
        serde_yaml::Value::String("no backends available".to_owned()),
    );
    let config = inject_conditions(serde_yaml::Value::Mapping(config), &condition);
    filters.push(PraxisFilterEntry {
        filter: "static_response".to_owned(),
        config,
    });
}

/// Injects `conditions` into a filter config mapping.
fn inject_conditions(mut config: serde_yaml::Value, condition: &Option<serde_yaml::Value>) -> serde_yaml::Value {
    if let (Some(cond), Some(map)) = (condition, config.as_mapping_mut()) {
        map.insert(serde_yaml::Value::String("conditions".to_owned()), cond.clone());
    }
    config
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

        assert_eq!(filters.len(), 1, "should produce one header filter");
        assert_eq!(filters[0].filter, "headers", "filter name should be headers");

        let config_str = serde_yaml::to_string(&filters[0].config).unwrap();
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

        assert_eq!(filters.len(), 1, "should produce one header filter");
        let config_str = serde_yaml::to_string(&filters[0].config).unwrap();
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

        assert_eq!(filters.len(), 1, "should produce one redirect filter");
        assert_eq!(filters[0].filter, "redirect", "filter name should be redirect");

        let config_str = serde_yaml::to_string(&filters[0].config).unwrap();
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

        assert_eq!(filters.len(), 2, "should produce two filters");
        assert!(
            filters.iter().any(|f| f.filter == "redirect"),
            "should have redirect filter"
        );
        assert!(
            filters.iter().any(|f| f.filter == "headers"),
            "should have headers filter"
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

        assert_eq!(filters.len(), 2, "should produce one filter per rule");

        let first_yaml = serde_yaml::to_string(&filters[0].config).unwrap();
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

        let second_yaml = serde_yaml::to_string(&filters[1].config).unwrap();
        assert!(
            second_yaml.contains("X-Second"),
            "second filter should have X-Second header"
        );
        assert!(
            second_yaml.contains("/add"),
            "second filter should be conditioned on /add"
        );
    }
}
