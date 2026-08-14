// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Gateway listener protocols.
//!
//! The Gateway API models `listener.protocol` as a free string, which
//! left `"HTTP"` and `"HTTPS"` literals compared by hand at every site
//! that cared. A typo in any one of them would silently drop a listener
//! from the generated config, so the parse happens once here.

// -----------------------------------------------------------------------------
// ListenerProtocol
// -----------------------------------------------------------------------------

/// A listener protocol this operator recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerProtocol {
    /// Cleartext HTTP.
    Http,

    /// HTTP over TLS, terminated by the data plane.
    Https,
}

impl ListenerProtocol {
    /// Parses a Gateway API protocol string.
    ///
    /// Returns `None` for protocols this operator does not serve, which
    /// the caller reports as `UnsupportedProtocol` rather than silently
    /// ignoring.
    pub fn parse(protocol: &str) -> Option<Self> {
        match protocol {
            "HTTP" => Some(Self::Http),
            "HTTPS" => Some(Self::Https),
            _ => None,
        }
    }

    /// Returns whether this operator can serve `protocol`.
    ///
    /// ```
    /// use praxis_operator::gateway_api::protocol::ListenerProtocol;
    ///
    /// assert!(ListenerProtocol::is_supported("HTTPS"));
    /// assert!(!ListenerProtocol::is_supported("TCP"));
    ///
    /// // The Gateway API spells protocols in upper case.
    /// assert!(!ListenerProtocol::is_supported("https"));
    /// ```
    pub fn is_supported(protocol: &str) -> bool {
        Self::parse(protocol).is_some()
    }

    /// Returns whether `protocol` terminates TLS.
    pub fn terminates_tls(protocol: &str) -> bool {
        Self::parse(protocol) == Some(Self::Https)
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recognises_the_served_protocols() {
        assert_eq!(ListenerProtocol::parse("HTTP"), Some(ListenerProtocol::Http));
        assert_eq!(ListenerProtocol::parse("HTTPS"), Some(ListenerProtocol::Https));
    }

    #[test]
    fn test_rejects_protocols_the_data_plane_cannot_serve() {
        for protocol in ["TCP", "UDP", "TLS", "GRPC", ""] {
            assert!(
                !ListenerProtocol::is_supported(protocol),
                "{protocol} is not served and must be reported as unsupported, not ignored"
            );
        }
    }

    #[test]
    fn test_protocol_matching_is_case_sensitive() {
        assert!(
            !ListenerProtocol::is_supported("http"),
            "the Gateway API spells protocols in upper case; accepting other spellings would \
             diverge from what the API server validates"
        );
    }

    #[test]
    fn test_only_https_terminates_tls() {
        assert!(ListenerProtocol::terminates_tls("HTTPS"), "HTTPS terminates TLS");
        assert!(!ListenerProtocol::terminates_tls("HTTP"), "plain HTTP does not");
        assert!(
            !ListenerProtocol::terminates_tls("TCP"),
            "an unserved protocol does not"
        );
    }
}
