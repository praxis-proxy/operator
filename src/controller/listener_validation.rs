// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Per-listener validation feeding `ResolvedRefs`.
//!
//! A listener can fail for reasons unrelated to the routes attached to
//! it: it may allow a route kind this operator does not serve, or name
//! a TLS Secret that is missing, cross-namespace without a grant, or
//! not actually a certificate. Each check reports the specific Gateway
//! API reason rather than a generic failure.

use std::collections::BTreeMap;

use gateway_api::{
    gateways::{GatewayListeners, GatewayListenersAllowedRoutesKinds, GatewayListenersTlsCertificateRefs},
    referencegrants::ReferenceGrant,
};
use k8s_openapi::{ByteString, api::core::v1::Secret, apimachinery::pkg::apis::meta::v1::Condition};
use kube::Api;

use crate::{
    context::Context,
    gateway_api::{conditions, reference_grant, status_types::RouteGroupKind},
};

// -----------------------------------------------------------------------------
// Listener Validation
// -----------------------------------------------------------------------------

/// Determines `supportedKinds` and `ResolvedRefs` for a listener.
///
/// Checks `allowedRoutes.kinds` for unsupported route kinds and validates
/// TLS certificate refs (group, kind, existence, format).
pub(super) async fn listener_resolved_refs(
    listener: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    ctx: &Context,
) -> (Vec<RouteGroupKind>, Condition) {
    let (supported, kinds_invalid) = validate_route_kinds(listener);

    if kinds_invalid {
        return (
            supported,
            conditions::unresolved_refs(generation, "InvalidRouteKinds", "unsupported route kinds specified"),
        );
    }

    if let Some(condition) = validate_tls_cert_refs(listener, generation, gateway_ns, ctx).await {
        return (supported, condition);
    }

    (supported, conditions::resolved_refs(generation, "all refs resolved"))
}

/// Validates the configured `allowedRoutes.kinds` on a listener.
///
/// Returns `(supported_kinds_json, has_invalid_kinds)`.
fn validate_route_kinds(listener: &GatewayListeners) -> (Vec<RouteGroupKind>, bool) {
    let configured = listener.allowed_routes.as_ref().and_then(|ar| ar.kinds.as_ref());
    let Some(kinds) = configured else {
        return (httproute_supported_kinds(), false);
    };

    let has_httproute = kinds.iter().any(is_httproute_kind);
    let has_unsupported = kinds.iter().any(|k| !is_httproute_kind(k));
    let supported = if has_httproute {
        httproute_supported_kinds()
    } else {
        Vec::new()
    };
    (supported, has_unsupported)
}

/// Returns the default `supportedKinds` JSON for `HTTPRoute`.
fn httproute_supported_kinds() -> Vec<RouteGroupKind> {
    vec![RouteGroupKind::httproute()]
}

/// Checks whether a route kind ref is `HTTPRoute` in the Gateway API group.
fn is_httproute_kind(k: &GatewayListenersAllowedRoutesKinds) -> bool {
    let group = k.group.as_deref().unwrap_or("gateway.networking.k8s.io");
    group == "gateway.networking.k8s.io" && k.kind == "HTTPRoute"
}

/// Validates TLS certificate refs on a listener.
///
/// Returns `Some(condition)` on the first validation failure, `None` when
/// all refs are valid.
async fn validate_tls_cert_refs(
    listener: &GatewayListeners,
    generation: i64,
    gateway_ns: &str,
    ctx: &Context,
) -> Option<Condition> {
    let cert_refs = listener.tls.as_ref()?.certificate_refs.as_ref()?;

    for cert_ref in cert_refs {
        if !is_secret_cert_ref(cert_ref) {
            return Some(conditions::unresolved_refs(
                generation,
                "InvalidCertificateRef",
                "unsupported certificate ref",
            ));
        }
        let secret_ns = cert_ref.namespace.as_deref().unwrap_or(gateway_ns);
        if let Some(c) = check_cross_ns_grant(ctx, generation, gateway_ns, secret_ns, &cert_ref.name) {
            return Some(c);
        }
        if let Some(c) = check_secret_contents(&ctx.client, generation, secret_ns, &cert_ref.name).await {
            return Some(c);
        }
    }
    None
}

/// Returns `true` when the cert ref points to a core `Secret`.
fn is_secret_cert_ref(cert_ref: &GatewayListenersTlsCertificateRefs) -> bool {
    let group = cert_ref.group.as_deref().unwrap_or("");
    let kind = cert_ref.kind.as_deref().unwrap_or("Secret");
    group.is_empty() && kind == "Secret"
}

/// Checks cross-namespace `ReferenceGrant` authorization for a TLS secret.
///
/// Returns `Some(condition)` when the reference is denied, `None` when
/// allowed or same-namespace.
fn check_cross_ns_grant(
    ctx: &Context,
    generation: i64,
    gateway_ns: &str,
    secret_ns: &str,
    secret_name: &str,
) -> Option<Condition> {
    if secret_ns == gateway_ns {
        return None;
    }

    let grants = ctx.stores.grants_in(secret_ns);

    if is_secret_ref_granted(gateway_ns, secret_ns, secret_name, &grants) {
        return None;
    }

    Some(conditions::unresolved_refs(
        generation,
        "RefNotPermitted",
        "cross-namespace secret reference requires a valid ReferenceGrant",
    ))
}

/// Checks whether a Gateway-to-Secret cross-namespace ref is allowed.
fn is_secret_ref_granted(gateway_ns: &str, secret_ns: &str, secret_name: &str, grants: &[ReferenceGrant]) -> bool {
    reference_grant::is_reference_allowed(
        gateway_ns,
        "gateway.networking.k8s.io",
        "Gateway",
        secret_ns,
        "",
        "Secret",
        Some(secret_name),
        grants,
    )
}

/// Validates that a TLS Secret exists and contains valid PEM data.
///
/// Returns `Some(condition)` on failure, `None` when the secret is valid.
async fn check_secret_contents(
    client: &kube::Client,
    generation: i64,
    secret_ns: &str,
    secret_name: &str,
) -> Option<Condition> {
    let secret_api = Api::<Secret>::namespaced(client.clone(), secret_ns);

    let Ok(secret) = secret_api.get(secret_name).await else {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "secret not found",
        ));
    };
    validate_tls_secret_data(secret.data.as_ref(), generation)
}

/// Validates that a Secret's data contains well-formed TLS PEM entries.
fn validate_tls_secret_data(data: Option<&BTreeMap<String, ByteString>>, generation: i64) -> Option<Condition> {
    let has_keys = data.is_some_and(|d| d.contains_key("tls.crt") && d.contains_key("tls.key"));
    if !has_keys {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "malformed secret",
        ));
    }

    let is_pem = data.is_some_and(|d| is_pem_entry(d, "tls.crt") && is_pem_entry(d, "tls.key"));
    if !is_pem {
        return Some(conditions::unresolved_refs(
            generation,
            "InvalidCertificateRef",
            "invalid PEM data",
        ));
    }

    None
}

/// Checks whether a Secret data entry starts with a PEM header.
fn is_pem_entry(data: &BTreeMap<String, ByteString>, key: &str) -> bool {
    data.get(key)
        .is_some_and(|v| String::from_utf8_lossy(&v.0).starts_with("-----BEGIN "))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use gateway_api::{
        gateways::{GatewayListenersAllowedRoutes, GatewayListenersAllowedRoutesKinds, GatewayListenersTls},
        referencegrants::{ReferenceGrantFrom, ReferenceGrantSpec, ReferenceGrantTo},
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::{controller::fixtures::secret_data, testing};

    #[test]
    fn test_validate_route_kinds_defaults_to_httproute() {
        let (supported, invalid) = validate_route_kinds(&http_listener());

        assert_eq!(supported.len(), 1, "HTTPRoute is supported by default");
        assert!(!invalid, "an unspecified kind list is never invalid");
    }

    #[test]
    fn test_validate_route_kinds_flags_unsupported_kinds() {
        let mut l = http_listener();
        l.allowed_routes = Some(GatewayListenersAllowedRoutes {
            kinds: Some(vec![GatewayListenersAllowedRoutesKinds {
                group: None,
                kind: "TCPRoute".to_owned(),
            }]),
            ..Default::default()
        });

        let (supported, invalid) = validate_route_kinds(&l);

        assert!(supported.is_empty(), "an unsupported-only list supports nothing");
        assert!(invalid, "TCPRoute is not implemented and must be reported");
    }

    #[test]
    fn test_is_secret_cert_ref_accepts_core_secret() {
        assert!(
            is_secret_cert_ref(&GatewayListenersTlsCertificateRefs {
                name: "cert".to_owned(),
                ..Default::default()
            }),
            "an unqualified certificateRef defaults to a core Secret"
        );
    }

    #[test]
    fn test_is_secret_cert_ref_rejects_other_kinds() {
        assert!(
            !is_secret_cert_ref(&GatewayListenersTlsCertificateRefs {
                name: "cert".to_owned(),
                kind: Some("ConfigMap".to_owned()),
                ..Default::default()
            }),
            "only Secrets can carry TLS material"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_accepts_pem() {
        let data = secret_data("-----BEGIN CERTIFICATE-----", "-----BEGIN PRIVATE KEY-----");

        assert!(
            validate_tls_secret_data(Some(&data), 1).is_none(),
            "a well-formed TLS secret produces no failure condition"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_rejects_missing_keys() {
        let condition = validate_tls_secret_data(None, 1);

        assert_eq!(
            condition.map(|c| c.message),
            Some("malformed secret".to_owned()),
            "a secret without tls.crt and tls.key is malformed"
        );
    }

    #[test]
    fn test_validate_tls_secret_data_rejects_non_pem() {
        let data = secret_data("not a certificate", "not a key");
        let condition = validate_tls_secret_data(Some(&data), 1);

        assert_eq!(
            condition.map(|c| c.message),
            Some("invalid PEM data".to_owned()),
            "non-PEM contents must be reported"
        );
    }

    #[test]
    fn test_is_pem_entry() {
        let data = secret_data("-----BEGIN CERTIFICATE-----", "garbage");

        assert!(is_pem_entry(&data, "tls.crt"), "a PEM header should be recognised");
        assert!(!is_pem_entry(&data, "tls.key"), "non-PEM data should be rejected");
        assert!(!is_pem_entry(&data, "missing"), "an absent key is not PEM");
    }

    // -----------------------------------------------------------------------
    // Resolved Refs
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_a_plain_http_listener_resolves() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());

        let (kinds, condition) = listener_resolved_refs(&http_listener(), 1, "infra", &ctx).await;

        assert_eq!(
            kinds,
            vec![RouteGroupKind::httproute()],
            "a listener that names no kinds serves the one kind this operator implements"
        );
        assert_eq!(condition.status, "True", "there is nothing for it to fail to resolve");
    }

    #[tokio::test]
    async fn test_an_unsupported_route_kind_is_named_as_such() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());
        let mut listener = http_listener();
        listener.allowed_routes = Some(allowed_kinds(&["TCPRoute"]));

        let (kinds, condition) = listener_resolved_refs(&listener, 1, "infra", &ctx).await;

        assert_eq!(condition.reason, "InvalidRouteKinds", "the reason names the problem");
        assert!(
            kinds.is_empty(),
            "advertising HTTPRoute on a listener that asked only for TCPRoute would invite routes \
             it will not serve"
        );
    }

    #[tokio::test]
    async fn test_a_missing_tls_secret_is_an_invalid_certificate_ref() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());

        let (_, condition) = listener_resolved_refs(&tls_listener("infra"), 1, "infra", &ctx).await;

        assert_eq!(
            (condition.status.as_str(), condition.reason.as_str()),
            ("False", "InvalidCertificateRef"),
            "a listener whose certificate does not exist cannot terminate TLS, and saying so is \
             the only way an author learns why"
        );
    }

    #[tokio::test]
    async fn test_a_well_formed_tls_secret_resolves() {
        let (ctx, _) = testing::fake_context(vec![secret_response()], testing::Cached::default());

        let (_, condition) = listener_resolved_refs(&tls_listener("infra"), 1, "infra", &ctx).await;

        assert_eq!(
            condition.status, "True",
            "a PEM certificate in the same namespace resolves"
        );
    }

    #[tokio::test]
    async fn test_a_cross_namespace_secret_needs_a_grant() {
        let (ctx, journal) = testing::fake_context(vec![secret_response()], testing::Cached::default());

        let (_, condition) = listener_resolved_refs(&tls_listener("certs"), 1, "infra", &ctx).await;

        assert_eq!(
            (condition.status.as_str(), condition.reason.as_str()),
            ("False", "RefNotPermitted"),
            "reading a Secret across a namespace boundary without a grant is exactly what a \
             ReferenceGrant exists to prevent"
        );
        assert!(
            journal.requests().is_empty(),
            "the refusal has to come before the read, or the operator has already done the thing \
             the grant was meant to authorize"
        );
    }

    #[tokio::test]
    async fn test_a_grant_admits_the_cross_namespace_secret() {
        let (ctx, _) = testing::fake_context(
            vec![secret_response()],
            testing::Cached {
                grants: vec![secret_grant()],
                ..Default::default()
            },
        );

        let (_, condition) = listener_resolved_refs(&tls_listener("certs"), 1, "infra", &ctx).await;

        assert_eq!(condition.status, "True", "the grant is what makes the reference legal");
    }

    #[tokio::test]
    async fn test_a_non_secret_certificate_ref_is_refused() {
        let (ctx, _) = testing::fake_context(vec![], testing::Cached::default());
        let mut listener = tls_listener("infra");
        if let Some(tls) = listener.tls.as_mut()
            && let Some(refs) = tls.certificate_refs.as_mut()
            && let Some(first) = refs.first_mut()
        {
            first.kind = Some("ConfigMap".to_owned());
        }

        let (_, condition) = listener_resolved_refs(&listener, 1, "infra", &ctx).await;

        assert_eq!(
            condition.reason, "InvalidCertificateRef",
            "this operator mounts core Secrets and nothing else"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a plain HTTP listener.
    fn http_listener() -> GatewayListeners {
        GatewayListeners {
            name: "http".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            ..Default::default()
        }
    }

    /// Builds an HTTPS listener naming a certificate in `secret_ns`.
    fn tls_listener(secret_ns: &str) -> GatewayListeners {
        GatewayListeners {
            name: "https".to_owned(),
            port: 443,
            protocol: "HTTPS".to_owned(),
            tls: Some(GatewayListenersTls {
                certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                    name: "cert".to_owned(),
                    namespace: Some(secret_ns.to_owned()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Builds an `allowedRoutes` naming the given kinds.
    fn allowed_kinds(kinds: &[&str]) -> GatewayListenersAllowedRoutes {
        GatewayListenersAllowedRoutes {
            kinds: Some(
                kinds
                    .iter()
                    .map(|kind| GatewayListenersAllowedRoutesKinds {
                        group: None,
                        kind: (*kind).to_owned(),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// The Secret the fake API server hands back, PEM and all.
    ///
    /// A `Secret`'s `data` is base64 on the wire, so the value is
    /// pre-encoded rather than pulling in an encoder for one literal.
    /// It decodes to a one-line PEM block.
    fn secret_response() -> testing::Canned {
        let pem = "LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCngKLS0tLS1FTkQgQ0VSVElGSUNBVEUtLS0tLQo=";
        testing::Canned::ok(
            "/secrets/cert",
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": { "name": "cert", "namespace": "infra" },
                "data": { "tls.crt": pem, "tls.key": pem },
            }),
        )
    }

    /// A grant letting Gateways in `infra` read Secrets in `certs`.
    fn secret_grant() -> ReferenceGrant {
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some("allow-certs".to_owned()),
                namespace: Some("certs".to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![ReferenceGrantFrom {
                    group: "gateway.networking.k8s.io".to_owned(),
                    kind: "Gateway".to_owned(),
                    namespace: "infra".to_owned(),
                }],
                to: vec![ReferenceGrantTo {
                    group: String::new(),
                    kind: "Secret".to_owned(),
                    name: Some("cert".to_owned()),
                }],
            },
        }
    }
}
