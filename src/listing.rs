// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Paginated collection listing.
//!
//! An unbounded `LIST` asks the API server to marshal every object of a
//! kind into one response. On a large cluster that is a multi-megabyte
//! body the operator must hold entirely in memory, and one the API
//! server may refuse outright. Every cluster-wide read goes through
//! here so the cost stays bounded by page size rather than cluster size.

use kube::{Api, api::ListParams};
use serde::de::DeserializeOwned;

use crate::error::Result;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Objects requested per `LIST` page.
const PAGE_SIZE: u32 = 500;

// -----------------------------------------------------------------------------
// Listing
// -----------------------------------------------------------------------------

/// Lists every object the API exposes, following continuation tokens.
///
/// # Errors
///
/// Returns an error if any page request fails. A partial listing is
/// never returned: a caller acting on half a cluster's routes would
/// generate a config that silently drops the rest.
pub async fn list_all<K>(api: &Api<K>) -> Result<Vec<K>>
where
    K: Clone + std::fmt::Debug + DeserializeOwned,
{
    let mut params = ListParams::default().limit(PAGE_SIZE);
    let mut items = Vec::new();

    loop {
        let page = api.list(&params).await?;
        let next = page.metadata.continue_.clone();
        items.extend(page.items);

        match next.filter(|token| !token.is_empty()) {
            Some(token) => params = params.continue_token(&token),
            None => return Ok(items),
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_params_carry_the_page_limit() {
        let params = ListParams::default().limit(PAGE_SIZE);

        assert_eq!(
            params.limit,
            Some(PAGE_SIZE),
            "the limit must reach the API server or the listing stays unbounded"
        );
    }

    #[test]
    fn test_continue_token_is_threaded_into_params() {
        let params = ListParams::default().limit(PAGE_SIZE).continue_token("abc");

        assert_eq!(
            params.continue_token.as_deref(),
            Some("abc"),
            "the continuation token must be carried into the next page request"
        );
    }
}
