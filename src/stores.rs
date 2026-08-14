// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Cluster-wide caches the reconcilers read instead of listing.
//!
//! Reconciling one Gateway needs to see every `HTTPRoute` in the
//! cluster (any of them may name it as a parent), every
//! `ReferenceGrant` (any of them may authorize one of its backends),
//! and every `Namespace` (its listeners may select routes by namespace
//! label). Fetching those with a `LIST` meant three cluster-wide reads
//! per Gateway per reconcile — so a cluster with G Gateways and R
//! routes paid O(G × R) object deserializations every resync, and a
//! single route edit fanned out to a full re-read for every Gateway.
//!
//! A reflector turns that into one watch connection per kind, held for
//! the operator's lifetime, feeding an in-memory store. Reconcile-time
//! reads become local.
//!
//! The stores are populated by their own watches rather than by the
//! controller's `watches()` triggers, which do not expose a store on
//! stable kube-runtime. That costs one extra connection per kind and
//! buys back a `LIST` per reconcile, which is not a close trade.

use std::{fmt::Debug, hash::Hash, sync::Arc, time::Duration};

use futures::StreamExt as _;
use gateway_api::{httproutes::HTTPRoute, referencegrants::ReferenceGrant};
use k8s_openapi::api::core::v1::Namespace;
use kube::{
    Api, Client, Resource,
    runtime::{WatchStreamExt as _, reflector, watcher},
};
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::error::{OperatorError, Result};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// How long to wait for a store's first full sync before giving up.
///
/// A store that never syncs is worse than a failed start: reconcilers
/// would read an empty cache and rewrite every Gateway's config as if
/// the cluster had no routes. Exiting lets the pod crash-loop with a
/// visible reason instead.
const INITIAL_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

// -----------------------------------------------------------------------------
// Stores
// -----------------------------------------------------------------------------

/// Read-only caches shared by every reconciler.
#[derive(Clone)]
pub struct Stores {
    /// Every `HTTPRoute` in the cluster.
    routes: reflector::Store<HTTPRoute>,

    /// Every `ReferenceGrant` in the cluster.
    grants: reflector::Store<ReferenceGrant>,

    /// Every `Namespace` in the cluster.
    namespaces: reflector::Store<Namespace>,
}

impl Stores {
    /// Starts a reflector per kind and waits for each to sync.
    ///
    /// # Errors
    ///
    /// Returns an error if any store fails to complete its initial
    /// sync within one minute, which usually means the operator lacks
    /// list/watch permission on that kind.
    pub async fn spawn(client: &Client) -> Result<Self> {
        let stores = Self {
            routes: spawn_reflector(Api::<HTTPRoute>::all(client.clone()), "HTTPRoute"),
            grants: spawn_reflector(Api::<ReferenceGrant>::all(client.clone()), "ReferenceGrant"),
            namespaces: spawn_reflector(Api::<Namespace>::all(client.clone()), "Namespace"),
        };

        wait_ready(&stores.routes, "HTTPRoute").await?;
        wait_ready(&stores.grants, "ReferenceGrant").await?;
        wait_ready(&stores.namespaces, "Namespace").await?;

        info!(
            routes = stores.routes.len(),
            grants = stores.grants.len(),
            namespaces = stores.namespaces.len(),
            "caches synced"
        );
        Ok(stores)
    }

    /// Returns every cached `HTTPRoute`.
    ///
    /// Handed out as `Arc`s because the route set is the one that grows
    /// with the cluster; cloning it per Gateway reconcile would give
    /// back much of what the cache saves.
    pub fn routes(&self) -> Vec<Arc<HTTPRoute>> {
        sorted(self.routes.state())
    }

    /// Returns every cached `ReferenceGrant`.
    pub fn grants(&self) -> Vec<ReferenceGrant> {
        cloned(&self.grants)
    }

    /// Returns cached `ReferenceGrants` in one namespace.
    pub fn grants_in(&self, namespace: &str) -> Vec<ReferenceGrant> {
        sorted(self.grants.state())
            .iter()
            .filter(|grant| grant.metadata.namespace.as_deref() == Some(namespace))
            .map(|grant| (**grant).clone())
            .collect()
    }

    /// Returns every cached `Namespace`.
    pub fn namespaces(&self) -> Vec<Namespace> {
        cloned(&self.namespaces)
    }
}

impl Debug for Stores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stores")
            .field("routes", &self.routes.len())
            .field("grants", &self.grants.len())
            .field("namespaces", &self.namespaces.len())
            .finish()
    }
}

// -----------------------------------------------------------------------------
// Reflectors
// -----------------------------------------------------------------------------

/// Clones a store's contents out of its `Arc`s.
///
/// Used for the small, slow-moving kinds where the clone costs less
/// than threading `Arc` through every consumer.
fn cloned<K>(store: &reflector::Store<K>) -> Vec<K>
where
    K: Resource + Clone + 'static,
    K::DynamicType: Eq + Hash + Clone + Default,
{
    sorted(store.state()).iter().map(|obj| (**obj).clone()).collect()
}

/// Orders cached objects by namespace and name.
///
/// `Store::state` collects the values of an `AHashMap`, so it hands back
/// a different order from one call to the next. That is fatal here, not
/// merely untidy: route order reaches the generated Praxis YAML, the
/// YAML is hashed into the data-plane pod template, and a hash that
/// changes every reconcile rolls a new `ReplicaSet` every reconcile. The
/// rollout then never completes, so routes are never accepted and the
/// Gateway never reports Programmed.
///
/// The `LIST` this cache replaced returned API-server order, which is
/// stable, so nothing downstream had ever needed to sort. Sorting here
/// restores the property the rest of the pipeline was written against.
fn sorted<K>(mut objects: Vec<Arc<K>>) -> Vec<Arc<K>>
where
    K: Resource + 'static,
{
    objects.sort_by(|a, b| {
        let left = (a.meta().namespace.as_deref(), a.meta().name.as_deref());
        let right = (b.meta().namespace.as_deref(), b.meta().name.as_deref());
        left.cmp(&right)
    });
    objects
}

/// Starts a watch feeding a store, and returns the store.
///
/// The watch runs for the process lifetime. `watcher` reconnects and
/// re-lists on its own, so a transient API error is logged and the
/// stream continues; the store keeps serving its last known state
/// meanwhile.
fn spawn_reflector<K>(api: Api<K>, kind: &'static str) -> reflector::Store<K>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Eq + Hash + Clone + Default,
{
    let (store, writer) = reflector::store();
    let stream = reflector(writer, watcher(api, watcher::Config::default())).applied_objects();

    drop(tokio::spawn(async move {
        let mut stream = Box::pin(stream);
        while let Some(event) = stream.next().await {
            if let Err(e) = event {
                warn!(%e, kind, "watch error, cache may be briefly stale");
            }
        }
        warn!(kind, "watch stream ended");
    }));

    store
}

/// Waits for a store's initial sync, mapping a timeout to an error.
async fn wait_ready<K>(store: &reflector::Store<K>, kind: &'static str) -> Result<()>
where
    K: Resource + Clone + Send + Sync + 'static,
    K::DynamicType: Eq + Hash + Clone + Default + Send + Sync,
{
    match tokio::time::timeout(INITIAL_SYNC_TIMEOUT, store.wait_until_ready()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(OperatorError::CacheSync(kind)),
    }
}

// -----------------------------------------------------------------------------
// Test Construction
// -----------------------------------------------------------------------------

#[cfg(test)]
impl Stores {
    /// Builds populated stores without touching an API server.
    ///
    /// A reflector `Writer` shares its cache with the `Store` rather
    /// than owning it, so the contents outlive the writers dropped at
    /// the end of this function. Only `wait_until_ready` would notice
    /// the missing writer, and nothing reading these stores calls it.
    pub(crate) fn fake(routes: Vec<HTTPRoute>, grants: Vec<ReferenceGrant>, namespaces: Vec<Namespace>) -> Self {
        use kube::runtime::watcher::Event;

        let (route_store, mut route_writer) = reflector::store::<HTTPRoute>();
        let (grant_store, mut grant_writer) = reflector::store::<ReferenceGrant>();
        let (ns_store, mut ns_writer) = reflector::store::<Namespace>();

        for route in routes {
            route_writer.apply_watcher_event(&Event::Apply(route));
        }
        for grant in grants {
            grant_writer.apply_watcher_event(&Event::Apply(grant));
        }
        for namespace in namespaces {
            ns_writer.apply_watcher_event(&Event::Apply(namespace));
        }

        drop((route_writer, grant_writer, ns_writer));
        Self {
            routes: route_store,
            grants: grant_store,
            namespaces: ns_store,
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use gateway_api::referencegrants::ReferenceGrantSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    /// Builds a `ReferenceGrant` in the given namespace.
    fn grant(namespace: &str, name: &str) -> ReferenceGrant {
        ReferenceGrant {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            spec: ReferenceGrantSpec {
                from: vec![],
                to: vec![],
            },
        }
    }

    /// Builds a `Stores` populated without touching an API server.
    ///
    /// The writers go out of scope at the end of this function. That is
    /// safe because a `Writer` shares its cache with the `Store` rather
    /// than owning it, so the contents outlive the writer; only
    /// `wait_until_ready` would notice, and these tests read state
    /// directly.
    fn populated(grants: Vec<ReferenceGrant>, namespaces: Vec<Namespace>) -> Stores {
        Stores::fake(vec![], grants, namespaces)
    }

    #[test]
    fn test_grants_in_returns_only_the_named_namespace() {
        let stores = populated(
            vec![grant("apps", "a"), grant("infra", "b"), grant("apps", "c")],
            vec![],
        );

        let mut names: Vec<_> = stores
            .grants_in("apps")
            .iter()
            .filter_map(|g| g.metadata.name.clone())
            .collect();
        names.sort();

        assert_eq!(
            names,
            vec!["a".to_owned(), "c".to_owned()],
            "a grant only authorizes references into its own namespace, so the filter must not \
             leak grants from elsewhere"
        );
    }

    #[test]
    fn test_grants_in_is_empty_for_an_unknown_namespace() {
        let stores = populated(vec![grant("apps", "a")], vec![]);

        assert!(
            stores.grants_in("other").is_empty(),
            "an unmatched namespace must yield no grants, not every grant"
        );
    }

    #[test]
    fn test_grants_returns_the_whole_cache() {
        let stores = populated(vec![grant("apps", "a"), grant("infra", "b")], vec![]);

        assert_eq!(stores.grants().len(), 2, "both grants should be returned");
    }

    #[test]
    fn test_empty_caches_read_as_empty_rather_than_failing() {
        let stores = populated(vec![], vec![]);

        assert!(stores.routes().is_empty(), "no routes were applied");
        assert!(stores.grants().is_empty(), "no grants were applied");
        assert!(stores.namespaces().is_empty(), "no namespaces were applied");
    }

    #[test]
    fn test_debug_reports_cache_sizes() {
        let stores = populated(vec![grant("apps", "a")], vec![]);

        let rendered = format!("{stores:?}");
        assert!(
            rendered.contains("grants: 1"),
            "Debug should surface cache depth, since an unexpectedly empty cache is the \
             failure mode worth seeing in a log: {rendered}"
        );
    }
    #[test]
    fn test_reads_are_ordered_regardless_of_insertion_order() {
        let forward = populated(vec![grant("b", "two"), grant("a", "one"), grant("a", "two")], vec![]);
        let reverse = populated(vec![grant("a", "two"), grant("b", "two"), grant("a", "one")], vec![]);

        let key = |g: &ReferenceGrant| {
            format!(
                "{}/{}",
                g.metadata.namespace.clone().unwrap_or_default(),
                g.metadata.name.clone().unwrap_or_default()
            )
        };
        let forward_keys: Vec<_> = forward.grants().iter().map(key).collect();
        let reverse_keys: Vec<_> = reverse.grants().iter().map(key).collect();

        assert_eq!(
            forward_keys,
            vec!["a/one".to_owned(), "a/two".to_owned(), "b/two".to_owned()],
            "cache reads must come back sorted by namespace then name"
        );
        assert_eq!(
            forward_keys, reverse_keys,
            "the order objects happened to arrive in must not reach the caller. `Store::state` \
             iterates an AHashMap, and route order flows into the generated Praxis YAML, which \
             is hashed into the data-plane pod template — an unstable order there rolls a new \
             ReplicaSet on every reconcile and the rollout never finishes"
        );
    }
}
