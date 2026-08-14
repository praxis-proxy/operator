// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Operator error types.

// -----------------------------------------------------------------------------
// Error
// -----------------------------------------------------------------------------

/// Errors produced during operator reconciliation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OperatorError {
    /// Kubernetes API call failed.
    #[error("kubernetes api: {0}")]
    Kube(#[from] kube::Error),

    /// A required object field was missing.
    #[error("missing object key: {0}")]
    MissingObjectKey(&'static str),

    /// The `finalizer` helper returned an error.
    #[error("finalizer: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<OperatorError>>),

    /// The `Gateway` references a `GatewayClass` this controller does not manage.
    #[error("gatewayclass not found: {0}")]
    GatewayClassNotFound(String),

    /// Leadership was taken by another replica.
    #[error("leadership lost to another replica")]
    LeadershipLost,

    /// Serialization failed.
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),

    /// YAML serialization failed.
    #[error("yaml serialization: {0}")]
    YamlSerialization(#[from] serde_yaml::Error),
}

/// Reconciliation result alias.
pub(crate) type Result<T, E = OperatorError> = std::result::Result<T, E>;
