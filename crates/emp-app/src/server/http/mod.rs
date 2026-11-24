pub(crate) mod accounts;
pub(crate) mod body;
pub(crate) mod catalog;
pub(crate) mod error;
pub(crate) mod gateway;
pub(crate) mod inference;
pub(crate) mod inspection;
pub(super) mod lifecycle;
pub(crate) mod local_writes;
pub(crate) mod login;
pub(crate) mod management;
pub(crate) mod migration;
pub(crate) mod modules;
pub(crate) mod pages;
pub(crate) mod proxy;
pub(crate) mod settings;
pub(crate) mod stream;
pub(crate) mod team;
pub(super) mod updates;
pub(crate) mod usage;
pub(super) mod websocket;

use super::head::RawRequestHead;
use crate::app::ServerState;
use axum::{
    Extension, Router,
    extract::{FromRequestParts, Request, State},
    http::{Extensions, Method},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
};
use std::{borrow::Cow, fmt::Write as _, sync::Arc};

pub(crate) fn with_entry_state<S>(
    router: Router<Arc<ServerState>>,
    state: Arc<ServerState>,
) -> Router<S> {
    router
        .layer(from_fn_with_state(
            state.clone(),
            error::present_entry_errors,
        ))
        .with_state(state)
}

pub(super) async fn unmatched(
    _access: proxy::RequestAccess,
    State(state): State<Arc<ServerState>>,
    Extension(raw): Extension<RawRequestHead>,
) -> error::EntryError {
    let request = raw.request();
    if request.raw_path().starts_with("/api/")
        && let Err(error) =
            management::ManagementSession::require(request, &state, crate::util::system_now())
    {
        return error;
    }
    error::EntryError::NotFound
}

pub(super) async fn guard_route_method(
    state: State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = match raw_head(request.extensions()) {
        Ok(raw) => raw.request().raw_path(),
        Err(error) => return error.into_response(),
    };
    if path.starts_with('/')
        && (request.method() != Method::HEAD || matches!(path, "/" | "/index.html" | "/healthz"))
    {
        return next.run(request).await;
    }
    let (mut parts, _) = request.into_parts();
    let result = async {
        let access = proxy::RequestAccess::from_request_parts(&mut parts, &state.0).await?;
        let raw = raw_head(&parts.extensions)?.clone();
        Ok::<_, error::EntryError>(unmatched(access, state, Extension(raw)).await)
    }
    .await;
    result.unwrap_or_else(|error| error).into_response()
}

pub(super) fn routing_path(path: &str) -> Cow<'_, str> {
    let needs_escape =
        |character| matches!(character, '\0'..=' ' | '"' | '#' | '<' | '>' | '`' | '\x7f');
    let Some(first) = path.find(needs_escape) else {
        return Cow::Borrowed(path);
    };
    let mut escaped = String::with_capacity(path.len());
    escaped.push_str(&path[..first]);
    for character in path[first..].chars() {
        if needs_escape(character) {
            write!(escaped, "%{:02X}", character as u8).expect("write to String");
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

fn raw_head(extensions: &Extensions) -> Result<&RawRequestHead, error::EntryError> {
    extensions
        .get::<RawRequestHead>()
        .ok_or(error::EntryError::InvalidHead)
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
#[test]
fn routing_path_escapes_every_ascii_byte_rejected_by_http_uri() {
    for byte in 0x00..=0x7f {
        let character = char::from(byte);
        for target in [format!("/a{character}b"), format!("/a?x={character}b")] {
            let escaped = routing_path(&target);
            assert!(
                axum::http::Uri::try_from(escaped.as_ref()).is_ok(),
                "{target:?} -> {escaped:?}"
            );
        }
    }
}
