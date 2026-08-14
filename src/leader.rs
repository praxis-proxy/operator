// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Leader election over a coordination `Lease`.
//!
//! Two replicas reconciling the same Gateway would race on every status
//! write and fight over the generated config, so exactly one instance
//! reconciles at a time. `kube-runtime` 3.1 ships no lease helper, so
//! the acquire-and-renew cycle is implemented against the
//! `coordination.k8s.io` API directly.

use std::time::Duration;

use k8s_openapi::{api::coordination::v1::Lease, apimachinery::pkg::apis::meta::v1::MicroTime, jiff::Timestamp};
use kube::{
    Api, Client,
    api::{Patch, PatchParams},
};
use tracing::{debug, info, warn};

use crate::{
    context::FIELD_MANAGER,
    error::{OperatorError, Result},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Name of the `Lease` object arbitrating leadership.
const LEASE_NAME: &str = "praxis-operator";

/// Namespace the `Lease` lives in when the downward API is unset.
const DEFAULT_LEASE_NAMESPACE: &str = "praxis-system";

/// Seconds a lease stays valid without renewal.
///
/// A holder that dies is replaced after at most this long, so it trades
/// failover latency against tolerance for a slow API server.
const LEASE_DURATION_SECONDS: i32 = 15;

/// How often the holder renews, comfortably inside the duration.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);

/// How often a non-holder re-checks whether the lease has expired.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);

// -----------------------------------------------------------------------------
// Identity
// -----------------------------------------------------------------------------

/// Returns this replica's unique holder identity.
///
/// Prefers the pod name supplied by the downward API so the holder is
/// identifiable with `kubectl get lease`; falls back to the hostname,
/// then to the process id.
pub fn identity() -> String {
    std::env::var("POD_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()))
}

/// Returns the namespace holding the lease.
fn lease_namespace() -> String {
    std::env::var("POD_NAMESPACE")
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| DEFAULT_LEASE_NAMESPACE.to_owned())
}

// -----------------------------------------------------------------------------
// Election
// -----------------------------------------------------------------------------

/// Blocks until this replica holds the lease.
///
/// # Errors
///
/// Returns an error only when the API rejects a lease write for a reason
/// other than another replica holding it; contention is retried.
pub async fn acquire(client: &Client, identity: &str) -> Result<()> {
    let api: Api<Lease> = Api::namespaced(client.clone(), &lease_namespace());

    loop {
        if try_acquire(&api, identity).await? {
            info!("acquired leadership as {identity}");
            return Ok(());
        }

        debug!("leadership held elsewhere, retrying in {RETRY_INTERVAL:?}");
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

/// Renews the lease until leadership is lost.
///
/// # Errors
///
/// Returns [`OperatorError::LeadershipLost`] when another replica takes
/// the lease. The caller is expected to stop reconciling and exit so the
/// Deployment restarts it as a follower, which is simpler to reason
/// about than resuming mid-flight.
pub async fn renew_until_lost(client: &Client, identity: &str) -> Result<()> {
    let api: Api<Lease> = Api::namespaced(client.clone(), &lease_namespace());

    loop {
        tokio::time::sleep(RENEW_INTERVAL).await;

        match try_acquire(&api, identity).await {
            Ok(true) => debug!("renewed leadership"),
            Ok(false) => {
                warn!("lost leadership to another replica");
                return Err(OperatorError::LeadershipLost);
            },
            Err(e) => warn!(%e, "lease renewal failed, will retry"),
        }
    }
}

/// Takes or renews the lease, returning whether this replica holds it.
async fn try_acquire(api: &Api<Lease>, identity: &str) -> Result<bool> {
    let now = Timestamp::now();
    let observed = match api.get(LEASE_NAME).await {
        Ok(lease) => Some(lease),
        Err(kube::Error::Api(resp)) if resp.code == 404 => None,
        Err(e) => return Err(e.into()),
    };

    if let Some(lease) = observed.as_ref()
        && !is_claimable(lease, identity, now)
    {
        return Ok(false);
    }

    let transitions = next_transitions(observed.as_ref(), identity);
    let patch = lease_patch(identity, now, transitions);

    api.patch(
        LEASE_NAME,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(true)
}

// -----------------------------------------------------------------------------
// Utility Functions
// -----------------------------------------------------------------------------

/// Returns whether `identity` may take the lease.
///
/// The current holder may always renew. Anyone else must wait for the
/// recorded renewal to age past the lease duration, which is what stops
/// two replicas from both believing they lead.
fn is_claimable(lease: &Lease, identity: &str, now: Timestamp) -> bool {
    let Some(spec) = lease.spec.as_ref() else {
        return true;
    };

    match spec.holder_identity.as_deref() {
        None => true,
        Some(holder) if holder == identity => true,
        Some(_) => is_expired(spec.renew_time.as_ref(), spec.lease_duration_seconds, now),
    }
}

/// Returns whether a recorded renewal has aged out.
///
/// A lease with no renewal timestamp is treated as expired: it cannot be
/// shown to be live, and refusing to ever claim it would deadlock every
/// replica.
fn is_expired(renewed: Option<&MicroTime>, duration_seconds: Option<i32>, now: Timestamp) -> bool {
    let Some(renewed) = renewed else {
        return true;
    };
    let duration = i64::from(duration_seconds.unwrap_or(LEASE_DURATION_SECONDS));

    now.as_second().saturating_sub(renewed.0.as_second()) > duration
}

/// Returns the transition count the next holder should record.
///
/// The count increments only when leadership actually changes hands, so
/// it stays a useful signal of instability rather than a renewal tally.
fn next_transitions(observed: Option<&Lease>, identity: &str) -> i32 {
    let Some(spec) = observed.and_then(|lease| lease.spec.as_ref()) else {
        return 0;
    };
    let current = spec.lease_transitions.unwrap_or(0);

    if spec.holder_identity.as_deref() == Some(identity) {
        current
    } else {
        current.saturating_add(1)
    }
}

/// Builds the server-side apply patch claiming the lease.
fn lease_patch(identity: &str, now: Timestamp, transitions: i32) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": { "name": LEASE_NAME },
        "spec": {
            "acquireTime": MicroTime(now),
            "holderIdentity": identity,
            "leaseDurationSeconds": LEASE_DURATION_SECONDS,
            "leaseTransitions": transitions,
            "renewTime": MicroTime(now),
        },
    })
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use k8s_openapi::api::coordination::v1::LeaseSpec;

    use super::*;

    #[test]
    fn test_a_lease_with_no_spec_is_claimable() {
        let lease = Lease::default();

        assert!(
            is_claimable(&lease, "me", Timestamp::now()),
            "a freshly created lease carrying no spec belongs to nobody"
        );
    }

    #[test]
    fn test_the_current_holder_may_always_renew() {
        let lease = held_by("me", 0);

        assert!(
            is_claimable(&lease, "me", Timestamp::now()),
            "the holder must be able to renew even while its own lease is live"
        );
    }

    #[test]
    fn test_a_live_lease_blocks_another_replica() {
        let lease = held_by("other", 0);

        assert!(
            !is_claimable(&lease, "me", Timestamp::now()),
            "a live lease held elsewhere must block, or two replicas reconcile at once"
        );
    }

    #[test]
    fn test_an_expired_lease_may_be_taken_over() {
        let lease = held_by("other", LEASE_DURATION_SECONDS + 5);

        assert!(
            is_claimable(&lease, "me", Timestamp::now()),
            "a holder that stopped renewing must be replaceable or failover never happens"
        );
    }

    #[test]
    fn test_a_lease_at_exactly_its_duration_is_still_live() {
        let lease = held_by("other", LEASE_DURATION_SECONDS);

        assert!(
            !is_claimable(&lease, "me", Timestamp::now()),
            "expiry is strictly past the duration, so the boundary still belongs to the holder"
        );
    }

    #[test]
    fn test_a_lease_without_a_renew_time_is_expired() {
        let lease = Lease {
            spec: Some(LeaseSpec {
                holder_identity: Some("other".to_owned()),
                renew_time: None,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(
            is_claimable(&lease, "me", Timestamp::now()),
            "a lease that cannot be shown live must be claimable, or every replica deadlocks"
        );
    }

    #[test]
    fn test_transitions_increment_only_on_handover() {
        let held = held_by("other", 0);

        assert_eq!(
            next_transitions(Some(&held), "me"),
            1,
            "taking over from another holder is a transition"
        );
        assert_eq!(
            next_transitions(Some(&held_by("me", 0)), "me"),
            0,
            "renewing your own lease is not a transition"
        );
        assert_eq!(
            next_transitions(None, "me"),
            0,
            "a first claim starts the count at zero"
        );
    }

    #[test]
    fn test_patch_records_holder_and_duration() {
        let patch = lease_patch("me", Timestamp::now(), 3);

        assert_eq!(patch["spec"]["holderIdentity"], "me", "the claimant must be recorded");
        assert_eq!(
            patch["spec"]["leaseDurationSeconds"], LEASE_DURATION_SECONDS,
            "followers rely on the duration to decide when the lease expired"
        );
        assert_eq!(patch["spec"]["leaseTransitions"], 3, "the transition count is carried");
    }

    #[test]
    fn test_identity_is_never_empty() {
        assert!(
            !identity().is_empty(),
            "an empty holder identity would make every replica look like the same holder"
        );
    }

    // -----------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------

    /// Builds a lease held by `holder`, renewed `age` seconds ago.
    fn held_by(holder: &str, age: i32) -> Lease {
        let renewed = Timestamp::from_second(Timestamp::now().as_second() - i64::from(age))
            .expect("timestamp should be representable");

        Lease {
            spec: Some(LeaseSpec {
                holder_identity: Some(holder.to_owned()),
                lease_duration_seconds: Some(LEASE_DURATION_SECONDS),
                renew_time: Some(MicroTime(renewed)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
