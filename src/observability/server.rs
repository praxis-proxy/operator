// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Health, readiness and metrics endpoints.
//!
//! Serves three fixed `GET` routes over HTTP/1.1. The responder is
//! hand-written on a tokio listener rather than pulling in a web
//! framework: the surface is three constant paths, and the conventions
//! favour avoiding a dependency that earns nothing.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, warn};

use super::metrics;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Address the observability endpoints bind.
const BIND_ADDRESS: &str = "0.0.0.0:8080";

/// Largest request head the responder will read.
const MAX_REQUEST_BYTES: usize = 8192; // 8 KiB

// -----------------------------------------------------------------------------
// State
// -----------------------------------------------------------------------------

/// Liveness and readiness shared with the controllers.
#[derive(Debug, Default)]
pub(crate) struct Health {
    /// Whether every controller has completed a first pass.
    ready: AtomicBool,
}

impl Health {
    /// Marks the operator ready to serve.
    pub(crate) fn mark_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    /// Returns whether the operator is ready to serve.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
}

// -----------------------------------------------------------------------------
// Server
// -----------------------------------------------------------------------------

/// Serves the observability endpoints until the process exits.
///
/// Binding failures are logged rather than propagated: losing metrics
/// is not a reason to take a working control plane down.
pub(crate) async fn serve(health: Arc<Health>) {
    let listener = match TcpListener::bind(BIND_ADDRESS).await {
        Ok(listener) => listener,
        Err(e) => {
            warn!(%e, "observability endpoints unavailable; continuing without them");
            return;
        },
    };

    debug!("observability endpoints listening on {BIND_ADDRESS}");
    accept_loop(listener, health).await;
}

/// Accepts connections until the process exits.
///
/// An accept failure is transient — a peer that vanished mid-handshake,
/// a momentary descriptor shortage — so it is logged and the loop
/// continues rather than taking the endpoints down.
async fn accept_loop(listener: TcpListener, health: Arc<Health>) -> ! {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let health = Arc::clone(&health);
                drop(tokio::spawn(async move { handle(stream, health).await }));
            },
            Err(e) => debug!(%e, "observability connection failed"),
        }
    }
}

/// Reads one request and writes the matching response.
async fn handle(mut stream: TcpStream, health: Arc<Health>) {
    let mut buf = vec![0_u8; MAX_REQUEST_BYTES];

    let read = match stream.read(&mut buf).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };

    let path = buf
        .get(..read)
        .map(String::from_utf8_lossy)
        .and_then(|head| request_path(&head).map(str::to_owned));

    let response = match path.as_deref() {
        Some("/healthz") => text_response(200, "ok"),
        Some("/readyz") if health.is_ready() => text_response(200, "ready"),
        Some("/readyz") => text_response(503, "not ready"),
        Some("/metrics") => text_response(200, &metrics::global().to_string()),
        _ => text_response(404, "not found"),
    };

    if let Err(e) = stream.write_all(response.as_bytes()).await {
        debug!(%e, "observability response failed");
    }
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Extracts the request target from an HTTP request head.
///
/// Returns `None` for anything that is not a `GET`, so the endpoints
/// never act on a write verb.
fn request_path(head: &str) -> Option<&str> {
    let mut parts = head.split_whitespace();

    if parts.next()? != "GET" {
        return None;
    }

    parts.next()
}

/// Builds a complete HTTP/1.1 plain-text response.
fn text_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let length = body.len();

    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: \
         {length}\r\nconnection: close\r\n\r\n{body}"
    )
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
    fn test_request_path_extracts_the_target() {
        assert_eq!(
            request_path("GET /metrics HTTP/1.1\r\nhost: x\r\n\r\n"),
            Some("/metrics"),
            "the request target should be parsed from the request line"
        );
    }

    #[test]
    fn test_request_path_rejects_non_get_verbs() {
        for head in ["POST /metrics HTTP/1.1\r\n", "DELETE /healthz HTTP/1.1\r\n"] {
            assert_eq!(
                request_path(head),
                None,
                "the observability endpoints must ignore write verbs: {head}"
            );
        }
    }

    #[test]
    fn test_request_path_rejects_garbage() {
        assert_eq!(request_path(""), None, "an empty request has no target");
        assert_eq!(request_path("GET"), None, "a truncated request line has no target");
    }

    #[test]
    fn test_response_declares_an_accurate_content_length() {
        let response = text_response(200, "ready");

        assert!(
            response.contains("content-length: 5"),
            "content-length must match the body or clients hang: {response}"
        );
        assert!(response.ends_with("\r\n\r\nready"), "the body follows a blank line");
    }

    #[test]
    fn test_response_maps_status_codes_to_reasons() {
        assert!(text_response(503, "x").starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(text_response(404, "x").starts_with("HTTP/1.1 404 Not Found"));
    }

    #[test]
    fn test_health_starts_unready() {
        assert!(
            !Health::default().is_ready(),
            "readiness must be earned; reporting ready before the first pass would route \
             traffic at an operator that has reconciled nothing"
        );
    }

    #[test]
    fn test_health_becomes_ready_when_marked() {
        let health = Health::default();
        health.mark_ready();

        assert!(health.is_ready(), "marking ready should take effect");
    }
}
