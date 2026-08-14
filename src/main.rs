// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Binary entry point for the Praxis Gateway API operator.

#![deny(unsafe_code)]

/// Starts the operator.
///
/// # Errors
///
/// Propagates any error from [`praxis_operator::run`].
#[tokio::main]
async fn main() -> praxis_operator::error::Result<()> {
    praxis_operator::run().await
}
