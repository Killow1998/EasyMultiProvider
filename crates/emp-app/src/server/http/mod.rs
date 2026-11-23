pub(crate) mod body;
pub(crate) mod error;
pub(crate) mod management;
pub(crate) mod proxy;

use super::head::RawRequestHead;
use crate::app::ServerState;
use axum::{Router, http::Extensions, middleware::from_fn_with_state};
use std::sync::Arc;

pub(crate) fn with_entry_state(
    router: Router<Arc<ServerState>>,
    state: Arc<ServerState>,
) -> Router {
    router
        .layer(from_fn_with_state(
            state.clone(),
            error::present_entry_errors,
        ))
        .with_state(state)
}

fn raw_head(extensions: &Extensions) -> Result<&RawRequestHead, error::EntryError> {
    extensions
        .get::<RawRequestHead>()
        .ok_or(error::EntryError::InvalidHead)
}

#[cfg(test)]
pub(crate) mod tests;
