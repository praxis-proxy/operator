// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Listener configuration generation for Praxis proxy.

use gateway_api::gateways::GatewayListeners;
use serde::Serialize;

use crate::gateway_api::protocol::ListenerProtocol;

// -----------------------------------------------------------------------------
// PraxisListener
// -----------------------------------------------------------------------------

/// Praxis listener configuration.
///
/// Serializes to YAML format for the Praxis proxy configuration file.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisListener {
    /// Listener name.
    pub name: String,

    /// Bind address (e.g., "0.0.0.0:80").
    pub address: String,

    /// Filter chain names.
    pub filter_chains: Vec<String>,

    /// Listener hostname constraint (not serialized to Praxis config).
    ///
    /// Propagated from Gateway listener; used to scope routes that lack
    /// an HTTPRoute-level hostname.
    #[serde(skip)]
    pub hostname: Option<String>,

    /// All section names in the merged port group (not serialized).
    ///
    /// When multiple Gateway listeners share a port, routes targeting
    /// any of these section names belong to this listener's filter chain.
    #[serde(skip)]
    pub merged_section_names: Vec<String>,

    /// Protocol (omit for HTTP, "http" for HTTPS with TLS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,

    /// TLS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<PraxisTls>,
}

/// Praxis TLS configuration.
///
/// Contains certificate references for TLS listeners.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisTls {
    /// Certificate configurations.
    pub certificates: Vec<PraxisCertificate>,
}

/// Praxis certificate configuration.
///
/// Points to certificate and key files on disk with optional SNI routing.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PraxisCertificate {
    /// Path to certificate file.
    pub cert_path: String,

    /// Path to private key file.
    pub key_path: String,

    /// SNI hostnames this certificate serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_names: Option<Vec<String>>,

    /// Whether this is the default certificate when no SNI matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
}

// -----------------------------------------------------------------------------
// Listener Conversion
// -----------------------------------------------------------------------------

/// Converts a `Gateway` API listener to Praxis listener configuration.
///
/// Maps `Gateway` listener properties to Praxis-compatible YAML structure.
/// For HTTP listeners, protocol is omitted (Praxis defaults to HTTP).
/// For HTTPS listeners, TLS certificates are mapped from `Secret` references.
pub fn convert_listener(listener: &GatewayListeners, chain_name: &str) -> PraxisListener {
    let port = listener.port;
    let address = format!("0.0.0.0:{port}");
    let is_https = ListenerProtocol::terminates_tls(&listener.protocol);

    PraxisListener {
        name: listener.name.clone(),
        address,
        filter_chains: vec![chain_name.to_owned()],
        hostname: listener.hostname.clone(),
        merged_section_names: vec![listener.name.clone()],
        protocol: is_https.then(|| "http".to_owned()),
        tls: if is_https { build_tls_config(listener) } else { None },
    }
}

/// Builds TLS configuration from an HTTPS listener's certificate refs.
///
/// Sets `server_names` from the listener hostname for SNI routing.
/// Listeners without a hostname get `default: true`.
///
/// Returns `None` when the listener has no TLS block.
fn build_tls_config(listener: &GatewayListeners) -> Option<PraxisTls> {
    listener.tls.as_ref().map(|tls_config| {
        let certificates = tls_config
            .certificate_refs
            .as_ref()
            .map(|refs| {
                refs.iter()
                    .map(|cert_ref| build_certificate(cert_ref, &listener.hostname))
                    .collect()
            })
            .unwrap_or_default();

        PraxisTls { certificates }
    })
}

/// Builds a single certificate entry with SNI routing metadata.
fn build_certificate(
    cert_ref: &gateway_api::gateways::GatewayListenersTlsCertificateRefs,
    listener_hostname: &Option<String>,
) -> PraxisCertificate {
    let secret_name = cert_ref.name.as_str();
    let (server_names, default) = match listener_hostname {
        Some(h) => (Some(vec![h.clone()]), None),
        None => (None, Some(true)),
    };
    PraxisCertificate {
        cert_path: format!("/tls/{secret_name}/tls.crt"),
        key_path: format!("/tls/{secret_name}/tls.key"),
        server_names,
        default,
    }
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
    fn test_convert_http_listener() {
        let listener = GatewayListeners {
            name: "http".to_owned(),
            port: 80,
            protocol: "HTTP".to_owned(),
            hostname: None,
            tls: None,
            allowed_routes: None,
        };

        let result = convert_listener(&listener, "http-chain");

        assert_eq!(result.name, "http", "name should match");
        assert_eq!(result.address, "0.0.0.0:80", "address should include port");
        assert_eq!(result.protocol, None, "HTTP should have no protocol set");
        assert_eq!(result.hostname, None, "listener without hostname should have None");
        assert_eq!(
            result.filter_chains,
            vec!["http-chain".to_owned()],
            "filter chains should match"
        );
        assert_eq!(result.tls, None, "HTTP should have no TLS");
    }

    #[test]
    fn test_convert_https_listener() {
        use gateway_api::gateways::{GatewayListenersTls, GatewayListenersTlsCertificateRefs};

        let listener = GatewayListeners {
            name: "https".to_owned(),
            port: 443,
            protocol: "HTTPS".to_owned(),
            hostname: None,
            tls: Some(GatewayListenersTls {
                certificate_refs: Some(vec![GatewayListenersTlsCertificateRefs {
                    name: "my-cert".to_owned(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            allowed_routes: None,
        };

        let result = convert_listener(&listener, "https-chain");

        assert_eq!(result.name, "https", "name should match");
        assert_eq!(result.address, "0.0.0.0:443", "address should include port");
        assert_eq!(
            result.protocol,
            Some("http".to_owned()),
            "HTTPS should have http protocol"
        );
        assert_eq!(
            result.filter_chains,
            vec!["https-chain".to_owned()],
            "filter chains should match"
        );

        let tls = result.tls.expect("HTTPS should have TLS config");
        assert_eq!(tls.certificates.len(), 1, "should have one certificate");
        assert_eq!(
            tls.certificates[0].cert_path, "/tls/my-cert/tls.crt",
            "cert path should match"
        );
        assert_eq!(
            tls.certificates[0].key_path, "/tls/my-cert/tls.key",
            "key path should match"
        );
    }

    #[test]
    fn test_convert_https_listener_multiple_certs() {
        use gateway_api::gateways::{GatewayListenersTls, GatewayListenersTlsCertificateRefs};

        let listener = GatewayListeners {
            name: "https".to_owned(),
            port: 443,
            protocol: "HTTPS".to_owned(),
            hostname: None,
            tls: Some(GatewayListenersTls {
                certificate_refs: Some(vec![
                    GatewayListenersTlsCertificateRefs {
                        name: "cert-a".to_owned(),
                        ..Default::default()
                    },
                    GatewayListenersTlsCertificateRefs {
                        name: "cert-b".to_owned(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            allowed_routes: None,
        };

        let result = convert_listener(&listener, "https-chain");

        let tls = result.tls.expect("HTTPS should have TLS config");
        assert_eq!(tls.certificates.len(), 2, "should have two certificates");
        assert_eq!(
            tls.certificates[0].cert_path, "/tls/cert-a/tls.crt",
            "first cert path should match"
        );
        assert_eq!(
            tls.certificates[1].cert_path, "/tls/cert-b/tls.crt",
            "second cert path should match"
        );
    }
}
