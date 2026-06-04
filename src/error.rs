// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Operator error types.

// -----------------------------------------------------------------------------
// Error
// -----------------------------------------------------------------------------

/// Errors produced during reconciliation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Kubernetes API call failed.
    #[error("kubernetes api: {0}")]
    Kube(#[from] kube::Error),

    /// A required object field was missing.
    #[error("missing object key: {0}")]
    MissingObjectKey(&'static str),

    /// The `finalizer` helper returned an error.
    #[error("finalizer: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Error>>),

    /// The `Gateway` references a `GatewayClass` this controller does not manage.
    #[error("gatewayclass not found: {0}")]
    GatewayClassNotFound(String),

    /// A referenced Kubernetes Secret was not found.
    #[error("secret not found: {namespace}/{name}")]
    #[allow(dead_code, reason = "reserved for TLS secret resolution")]
    SecretNotFound {
        /// Secret namespace.
        namespace: String,
        /// Secret name.
        name: String,
    },

    /// Generated Praxis config is invalid.
    #[error("config generation: {0}")]
    #[allow(dead_code, reason = "reserved for config validation")]
    ConfigGeneration(String),

    /// Serialization failed.
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),

    /// YAML serialization failed.
    #[error("yaml serialization: {0}")]
    YamlSerialization(#[from] serde_yaml::Error),
}

/// Reconciliation result alias.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
