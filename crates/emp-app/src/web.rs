//! Serve the browser application.

use crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE;
use crate::http::response::response;

/// Embedded directly from the existing Python package so Web UI bytes cannot
/// drift during the rewrite.
pub const WEB_INDEX_BYTES: &[u8] = include_bytes!("../../../easy_multi_provider/web/index.html");

/// Serve the UI without authentication. It contains no secrets: the script
/// exchanges the single-use bootstrap token for a session kept in origin-scoped
/// storage. Any cookie left by older releases is expired.
pub(crate) fn ui_response() -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        WEB_INDEX_BYTES,
        &[
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
            ("Set-Cookie", CLEAR_LEGACY_SESSION_COOKIE),
        ],
    )
}

pub(crate) const VISION_TEST_IMAGE_BYTES: &[u8] =
    include_bytes!("../../../easy_multi_provider/web/vision-test-icon.png");
