// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! A Kubernetes client backed by canned responses.
//!
//! Most of what an operator does is talk to an API server, so most of
//! it used to be unreachable from a unit test: every reconciler, every
//! apply, every lookup sat behind a [`kube::Client`] that only exists
//! against a real cluster. Testing those paths through the conformance
//! suite alone means a twenty-minute round trip to learn that a status
//! patch names the wrong field.
//!
//! [`kube::Client::new`] takes any `tower::Service` over HTTP, so a
//! client can be built from a function that answers requests from a
//! table instead. What the tests then exercise is the real reconciler,
//! the real serialization, and the real error handling — only the
//! socket is fake.
//!
//! ```ignore
//! let client = fake_client(vec![
//!     Route::get("/apis/gateway.networking.k8s.io/v1/gatewayclasses/praxis", json!({ ... })),
//! ]);
//! ```

use std::{
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use kube::Client;
use serde_json::Value;

// -----------------------------------------------------------------------------
// Canned Responses
// -----------------------------------------------------------------------------

/// One canned answer, matched against the request that asks for it.
#[derive(Debug, Clone)]
pub(crate) struct Canned {
    /// Substring the request path must contain for this entry to apply.
    ///
    /// A substring rather than the whole path: kube appends field
    /// managers, dry-run flags and label selectors to a query string
    /// that a test has no reason to care about.
    pub(crate) path: String,

    /// HTTP status to answer with.
    pub(crate) status: StatusCode,

    /// JSON body to answer with.
    pub(crate) body: Value,
}

impl Canned {
    /// Answers a request whose path contains `path` with `body` and 200.
    pub(crate) fn ok(path: &str, body: Value) -> Self {
        Self {
            path: path.to_owned(),
            status: StatusCode::OK,
            body,
        }
    }

    /// Answers with 404 and a Kubernetes `Status` object.
    ///
    /// The shape matters: kube parses the body to decide whether an
    /// error is `ErrorResponse::NotFound`, and code under test
    /// routinely branches on exactly that.
    pub(crate) fn not_found(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            status: StatusCode::NOT_FOUND,
            body: serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "status": "Failure",
                "message": "not found",
                "reason": "NotFound",
                "code": 404,
            }),
        }
    }

    /// Answers with 500 and a Kubernetes `Status` object.
    pub(crate) fn server_error(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "status": "Failure",
                "message": "boom",
                "reason": "InternalError",
                "code": 500,
            }),
        }
    }
}

// -----------------------------------------------------------------------------
// Recorded Requests
// -----------------------------------------------------------------------------

/// A request the code under test issued.
#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    /// HTTP method.
    pub(crate) method: String,

    /// Request path, query string included.
    pub(crate) uri: String,

    /// Request body, parsed as JSON when it was JSON.
    pub(crate) body: Option<Value>,
}

/// The requests one fake client has seen, in order.
///
/// Shared with the service so a test can assert on what was sent —
/// which for a status writer is the whole of the observable behaviour.
#[derive(Debug, Clone, Default)]
pub(crate) struct Journal(Arc<Mutex<Vec<Recorded>>>);

impl Journal {
    /// Returns every request recorded so far.
    ///
    /// # Panics
    ///
    /// Panics if a previous holder of the lock panicked, which in a
    /// test is a failure worth surfacing rather than hiding.
    #[must_use]
    pub(crate) fn requests(&self) -> Vec<Recorded> {
        self.0.lock().expect("the journal lock is only held to push").clone()
    }

    /// Returns the recorded requests whose path contains `needle`.
    #[must_use]
    pub(crate) fn matching(&self, needle: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|request| request.uri.contains(needle))
            .collect()
    }

    /// Records one request.
    fn push(&self, recorded: Recorded) {
        self.0
            .lock()
            .expect("the journal lock is only held to push")
            .push(recorded);
    }
}

// -----------------------------------------------------------------------------
// Fake Service
// -----------------------------------------------------------------------------

/// A `tower::Service` answering from a table of canned responses.
#[derive(Clone)]
struct FakeService {
    /// Canned answers, tried in order; the first path match wins.
    canned: Arc<[Canned]>,

    /// Where requests are recorded.
    journal: Journal,
}

impl tower::Service<Request<kube::client::Body>> for FakeService {
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = Response<Full<Bytes>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<kube::client::Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let body = collect_json(req.into_body()).await;

            this.journal.push(Recorded {
                method,
                uri: uri.clone(),
                body,
            });
            Ok(this.answer(&uri))
        })
    }
}

/// Reads a request body and parses it as JSON, if it is JSON.
///
/// The body is what a test asserting on a status patch actually cares
/// about, and it is only readable here — by the time the request
/// reaches the journal the stream is gone.
async fn collect_json(body: kube::client::Body) -> Option<Value> {
    use http_body_util::BodyExt as _;

    let bytes = body.collect().await.ok()?.to_bytes();
    serde_json::from_slice(&bytes).ok()
}

impl FakeService {
    /// Builds the response for a request path.
    fn answer(&self, uri: &str) -> Response<Full<Bytes>> {
        let found = self.canned.iter().find(|entry| uri.contains(&entry.path));

        let (status, body) = found.map_or_else(
            || (StatusCode::NOT_FOUND, Canned::not_found("").body),
            |entry| (entry.status, entry.body.clone()),
        );

        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
    }
}

// -----------------------------------------------------------------------------
// Constructors
// -----------------------------------------------------------------------------

/// Builds a client answering from `canned`, and the journal of what it
/// was asked.
///
/// A request matching no entry gets a 404, which is what an empty
/// cluster would say and keeps a test from having to enumerate the
/// lookups it does not care about.
#[must_use]
pub(crate) fn fake_client(canned: Vec<Canned>) -> (Client, Journal) {
    let journal = Journal::default();
    let service = FakeService {
        canned: canned.into(),
        journal: journal.clone(),
    };
    (Client::new(service, "default"), journal)
}

/// Builds a client that answers everything with 500.
///
/// The error paths are worth their own tests: a reconciler that
/// swallows an API failure looks identical to one that succeeded until
/// something downstream is missing.
#[must_use]
pub(crate) fn failing_client() -> (Client, Journal) {
    fake_client(vec![Canned::server_error("")])
}

/// Builds the [`Context`] a reconciler takes, over a fake client.
///
/// [`Context`]: crate::context::Context
#[must_use]
pub(crate) fn fake_context(canned: Vec<Canned>, cached: Cached) -> (Arc<crate::context::Context>, Journal) {
    let (client, journal) = fake_client(canned);
    let recorder = kube::runtime::events::Recorder::new(client.clone(), crate::context::reporter());
    let context = crate::context::Context {
        client,
        recorder,
        stores: crate::stores::Stores::fake(cached.routes, cached.grants, cached.namespaces),
    };
    (Arc::new(context), journal)
}

/// What the reflector caches hold for one test.
///
/// A struct rather than three positional vectors, which at three empty
/// `vec![]`s in a row stop saying which kind is which.
#[derive(Debug, Default)]
pub(crate) struct Cached {
    /// Cached `HTTPRoutes`.
    pub(crate) routes: Vec<gateway_api::httproutes::HTTPRoute>,

    /// Cached `ReferenceGrants`.
    pub(crate) grants: Vec<gateway_api::referencegrants::ReferenceGrant>,

    /// Cached `Namespaces`.
    pub(crate) namespaces: Vec<k8s_openapi::api::core::v1::Namespace>,
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use gateway_api::gatewayclasses::GatewayClass;
    use kube::Api;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn test_a_canned_object_comes_back_typed() {
        let (client, _) = fake_client(vec![Canned::ok(
            "/gatewayclasses/praxis",
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "GatewayClass",
                "metadata": { "name": "praxis" },
                "spec": { "controllerName": "praxis.sh/gateway-controller" },
            }),
        )]);

        let class = Api::<GatewayClass>::all(client)
            .get("praxis")
            .await
            .expect("the canned response deserializes");

        assert_eq!(
            class.spec.controller_name, "praxis.sh/gateway-controller",
            "the fake client has to round-trip real objects, or the tests built on it prove nothing"
        );
    }

    #[tokio::test]
    async fn test_an_unlisted_path_reads_as_absent() {
        let (client, journal) = fake_client(vec![]);

        let result = Api::<GatewayClass>::all(client).get("missing").await;

        assert!(result.is_err(), "an empty cluster has no GatewayClass to return");
        assert_eq!(journal.matching("missing").len(), 1, "the lookup should be recorded");
    }

    #[tokio::test]
    async fn test_a_patch_records_its_method_and_body() {
        let (client, journal) = fake_client(vec![Canned::ok(
            "/gatewayclasses/praxis",
            json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "GatewayClass",
                "metadata": { "name": "praxis" },
                "spec": { "controllerName": "praxis.sh/gateway-controller" },
            }),
        )]);

        let patch = json!({ "status": { "conditions": [] } });
        Api::<GatewayClass>::all(client)
            .patch_status(
                "praxis",
                &kube::api::PatchParams::apply("test"),
                &kube::api::Patch::Apply(&patch),
            )
            .await
            .expect("the canned response deserializes");

        let sent = journal.matching("/status").pop().expect("the patch should be recorded");
        assert_eq!(sent.method, "PATCH", "a server-side apply is a PATCH");
        assert_eq!(
            sent.body,
            Some(patch),
            "the body is the whole of what a status writer does, so a test has to be able to see it"
        );
    }

    #[tokio::test]
    async fn test_a_failing_client_reports_the_status_code() {
        let (client, _) = failing_client();

        let error = Api::<GatewayClass>::all(client)
            .get("praxis")
            .await
            .expect_err("a 500 is an error");

        assert!(
            matches!(&error, kube::Error::Api(response) if response.code == 500),
            "the error has to survive as an API error, since callers branch on the code: {error}"
        );
    }
}
