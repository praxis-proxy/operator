// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Shared controller context.

use kube::Client;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// The controller name registered in `GatewayClass` resources.
pub(crate) const CONTROLLER_NAME: &str = "praxis.sh/gateway-controller";

/// Finalizer string applied to Gateways.
pub(crate) const GATEWAY_FINALIZER: &str = "gateway.praxis.sh/finalizer";

/// Praxis container image, configurable via `PRAXIS_IMAGE` env var.
///
/// Falls back to `ghcr.io/praxis-proxy/praxis:latest` when unset.
pub(crate) fn praxis_image() -> String {
    std::env::var("PRAXIS_IMAGE").unwrap_or_else(|_| "ghcr.io/praxis-proxy/praxis:latest".to_owned())
}

/// Admin port on the Praxis data-plane container.
pub(crate) const ADMIN_PORT: i32 = 9901;

// -----------------------------------------------------------------------------
// Context
// -----------------------------------------------------------------------------

/// Shared state passed to all reconcilers.
pub(crate) struct Context {
    /// Kubernetes API client.
    pub(crate) client: Client,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}
