// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Detection of mutually incompatible `Gateway` listeners.
//!
//! Two listeners sharing a port must agree on protocol and must not
//! claim the same hostname. A Gateway declaring both is not merely
//! ambiguous — the data plane cannot bind a single port twice — so the
//! offending listeners are excluded from the generated config and
//! reported through the `Conflicted` condition.

use std::collections::{BTreeMap, HashMap};

use gateway_api::gateways::GatewayListeners;

// -----------------------------------------------------------------------------
// ConflictReason
// -----------------------------------------------------------------------------

/// Why a listener conflicts with another on the same Gateway.
///
/// The variants map onto the Gateway API `ListenerConditionReason`
/// values of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// Another listener claims the same port with a different protocol.
    ProtocolConflict,

    /// Another listener claims the same port, protocol and hostname.
    HostnameConflict,
}

impl ConflictReason {
    /// Returns the Gateway API condition reason string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolConflict => "ProtocolConflict",
            Self::HostnameConflict => "HostnameConflict",
        }
    }

    /// Returns the message to report on the conflicting conditions.
    pub fn message(self) -> &'static str {
        match self {
            Self::ProtocolConflict => {
                "listener conflicts with another listener on the same port using a \
                                       different protocol"
            },
            Self::HostnameConflict => "listener conflicts with another listener claiming the same port and hostname",
        }
    }
}

// -----------------------------------------------------------------------------
// Conflict Detection
// -----------------------------------------------------------------------------

/// Finds every listener that conflicts with another on the same Gateway.
///
/// Returns a map from listener name to the reason it conflicts. Both
/// sides of a conflict are reported: neither can be programmed, so
/// neither may claim the port.
pub fn detect_conflicts(listeners: &[GatewayListeners]) -> HashMap<String, ConflictReason> {
    let mut conflicts = HashMap::new();

    for group in group_by_port(listeners).values() {
        mark_protocol_conflicts(group, &mut conflicts);
        mark_hostname_conflicts(group, &mut conflicts);
    }

    conflicts
}

/// Groups listeners by the port they bind.
///
/// Ordered so that conflict reporting is stable across reconciles.
fn group_by_port(listeners: &[GatewayListeners]) -> BTreeMap<i32, Vec<&GatewayListeners>> {
    let mut by_port: BTreeMap<i32, Vec<&GatewayListeners>> = BTreeMap::new();
    for listener in listeners {
        by_port.entry(listener.port).or_default().push(listener);
    }
    by_port
}

/// Flags every listener in a port group when the group mixes protocols.
fn mark_protocol_conflicts(group: &[&GatewayListeners], conflicts: &mut HashMap<String, ConflictReason>) {
    let Some(first) = group.first() else {
        return;
    };

    if group.iter().all(|l| l.protocol == first.protocol) {
        return;
    }

    for listener in group {
        conflicts.insert(listener.name.clone(), ConflictReason::ProtocolConflict);
    }
}

/// Flags listeners in a port group that claim the same hostname.
///
/// Distinct hostnames on one port are legitimate SNI or Host-header
/// multiplexing, so only duplicates conflict. A listener with no
/// hostname claims every hostname on that port, and duplicates of that
/// claim conflict too.
fn mark_hostname_conflicts(group: &[&GatewayListeners], conflicts: &mut HashMap<String, ConflictReason>) {
    let mut by_hostname: BTreeMap<String, Vec<&str>> = BTreeMap::new();

    for listener in group {
        let key = hostname_key(listener);
        by_hostname.entry(key).or_default().push(listener.name.as_str());
    }

    for names in by_hostname.values() {
        if names.len() < 2 {
            continue;
        }
        for name in names {
            conflicts
                .entry((*name).to_owned())
                .or_insert(ConflictReason::HostnameConflict);
        }
    }
}

/// Returns the hostname a listener claims, normalised for comparison.
///
/// Hostnames are compared case-insensitively per RFC 1123; an absent
/// hostname is a distinct claim over every host on the port.
fn hostname_key(listener: &GatewayListeners) -> String {
    listener
        .hostname
        .as_ref()
        .map_or_else(|| "*".to_owned(), |h| h.to_ascii_lowercase())
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distinct_ports_never_conflict() {
        let listeners = vec![
            listener("http", 80, "HTTP", None),
            listener("https", 443, "HTTPS", None),
        ];

        assert!(
            detect_conflicts(&listeners).is_empty(),
            "listeners on different ports cannot conflict"
        );
    }

    #[test]
    fn test_same_port_different_protocol_is_a_protocol_conflict() {
        let listeners = vec![
            listener("plain", 80, "HTTP", None),
            listener("secure", 80, "HTTPS", None),
        ];
        let conflicts = detect_conflicts(&listeners);

        assert_eq!(
            conflicts.get("plain"),
            Some(&ConflictReason::ProtocolConflict),
            "a port cannot serve two protocols"
        );
        assert_eq!(
            conflicts.get("secure"),
            Some(&ConflictReason::ProtocolConflict),
            "both sides of a protocol conflict must be reported"
        );
    }

    #[test]
    fn test_same_port_distinct_hostnames_do_not_conflict() {
        let listeners = vec![
            listener("a", 443, "HTTPS", Some("a.example.com")),
            listener("b", 443, "HTTPS", Some("b.example.com")),
        ];

        assert!(
            detect_conflicts(&listeners).is_empty(),
            "distinct hostnames on one port are legitimate multiplexing"
        );
    }

    #[test]
    fn test_same_port_same_hostname_is_a_hostname_conflict() {
        let listeners = vec![
            listener("a", 443, "HTTPS", Some("dup.example.com")),
            listener("b", 443, "HTTPS", Some("dup.example.com")),
        ];
        let conflicts = detect_conflicts(&listeners);

        assert_eq!(
            conflicts.get("a"),
            Some(&ConflictReason::HostnameConflict),
            "two listeners cannot claim the same port and hostname"
        );
        assert_eq!(
            conflicts.get("b"),
            Some(&ConflictReason::HostnameConflict),
            "both sides of a hostname conflict must be reported"
        );
    }

    #[test]
    fn test_hostname_conflict_is_case_insensitive() {
        let listeners = vec![
            listener("a", 443, "HTTPS", Some("Dup.Example.com")),
            listener("b", 443, "HTTPS", Some("dup.example.com")),
        ];

        assert_eq!(
            detect_conflicts(&listeners).len(),
            2,
            "hostnames differing only in case are the same claim per RFC 1123"
        );
    }

    #[test]
    fn test_duplicate_absent_hostnames_conflict() {
        let listeners = vec![listener("a", 80, "HTTP", None), listener("b", 80, "HTTP", None)];

        assert_eq!(
            detect_conflicts(&listeners).len(),
            2,
            "two listeners each claiming every hostname on a port conflict"
        );
    }

    #[test]
    fn test_absent_and_specific_hostname_do_not_conflict() {
        let listeners = vec![
            listener("catchall", 80, "HTTP", None),
            listener("specific", 80, "HTTP", Some("a.example.com")),
        ];

        assert!(
            detect_conflicts(&listeners).is_empty(),
            "a catch-all listener and a hostname-specific one can share a port"
        );
    }

    #[test]
    fn test_protocol_conflict_takes_precedence_over_hostname() {
        let listeners = vec![
            listener("a", 80, "HTTP", Some("dup.example.com")),
            listener("b", 80, "HTTPS", Some("dup.example.com")),
        ];
        let conflicts = detect_conflicts(&listeners);

        assert_eq!(
            conflicts.get("a"),
            Some(&ConflictReason::ProtocolConflict),
            "the protocol mismatch is the more fundamental conflict and should be reported"
        );
    }

    #[test]
    fn test_single_listener_never_conflicts() {
        assert!(
            detect_conflicts(&[listener("only", 80, "HTTP", None)]).is_empty(),
            "a lone listener has nothing to conflict with"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a Gateway listener with the given identity.
    fn listener(name: &str, port: i32, protocol: &str, hostname: Option<&str>) -> GatewayListeners {
        GatewayListeners {
            allowed_routes: None,
            hostname: hostname.map(str::to_owned),
            name: name.to_owned(),
            port,
            protocol: protocol.to_owned(),
            tls: None,
        }
    }
}
