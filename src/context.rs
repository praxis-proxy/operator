// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Shared controller context.

use kube::{
    Client,
    runtime::events::{Recorder, Reporter},
};

use crate::stores::Stores;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// The controller name registered in `GatewayClass` resources.
pub const CONTROLLER_NAME: &str = "praxis.sh/gateway-controller";

/// Finalizer string applied to Gateways.
pub const GATEWAY_FINALIZER: &str = "gateway.praxis.sh/finalizer";

/// Field manager recorded on every server-side apply the operator issues.
///
/// Server-side apply tracks ownership per manager name, so every write
/// this operator makes must use the same one. Two names would let the
/// operator fight itself: fields written under the first would look
/// like another actor's to the second, and neither would ever release
/// them.
pub const FIELD_MANAGER: &str = "praxis-operator";

/// Admin port on the Praxis data-plane container.
pub const ADMIN_PORT: i32 = 9901;

/// Image used when `PRAXIS_IMAGE` is unset.
const DEFAULT_PRAXIS_IMAGE: &str = "ghcr.io/praxis-proxy/praxis:latest";

// -----------------------------------------------------------------------------
// Data Plane Image
// -----------------------------------------------------------------------------

/// Praxis container image, configurable via `PRAXIS_IMAGE` env var.
///
/// Falls back to `ghcr.io/praxis-proxy/praxis:latest` when unset.
pub fn praxis_image() -> String {
    std::env::var("PRAXIS_IMAGE").unwrap_or_else(|_| DEFAULT_PRAXIS_IMAGE.to_owned())
}

// -----------------------------------------------------------------------------
// Context
// -----------------------------------------------------------------------------

/// Shared state passed to all reconcilers.
pub struct Context {
    /// Kubernetes API client.
    pub client: Client,

    /// Publishes Kubernetes events for user-visible decisions.
    pub recorder: Recorder,

    /// Cluster-wide caches read in place of per-reconcile listing.
    pub stores: Stores,
}

/// Builds the event reporter identifying this operator.
pub fn reporter() -> Reporter {
    Reporter {
        controller: "praxis-operator".to_owned(),
        instance: std::env::var("POD_NAME").ok(),
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_praxis_image_follows_the_environment() {
        let expected = std::env::var("PRAXIS_IMAGE").unwrap_or_else(|_| DEFAULT_PRAXIS_IMAGE.to_owned());

        assert_eq!(
            praxis_image(),
            expected,
            "PRAXIS_IMAGE should override the image, and an unset variable should fall back to the default"
        );
    }

    #[test]
    fn test_default_praxis_image_is_a_tagged_reference() {
        assert!(
            DEFAULT_PRAXIS_IMAGE.contains('/') && DEFAULT_PRAXIS_IMAGE.contains(':'),
            "the fallback must be a fully qualified, tagged image reference"
        );
    }

    #[test]
    fn test_controller_name_is_the_registered_domain_path() {
        assert_eq!(
            CONTROLLER_NAME, "praxis.sh/gateway-controller",
            "the controller name is part of the public GatewayClass contract"
        );
    }

    #[test]
    fn test_gateway_finalizer_is_domain_qualified() {
        assert!(
            GATEWAY_FINALIZER.contains('/'),
            "Kubernetes requires a domain-qualified finalizer name"
        );
    }

    #[test]
    fn test_admin_port_matches_the_praxis_default() {
        assert_eq!(ADMIN_PORT, 9901, "probes target the Praxis admin endpoint");
    }
}
