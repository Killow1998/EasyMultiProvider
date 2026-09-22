//! Serve the unchanged browser application.

use crate::http::response::response;

/// Embedded directly from the existing Python package so Web UI bytes cannot
/// drift during the rewrite.
pub const WEB_INDEX_BYTES: &[u8] = include_bytes!("../../../easy_multi_provider/web/index.html");

const LOGIN_HTML: &str = r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>登录 EMP</title><body style="font-family:system-ui;max-width:36rem;margin:12vh auto;padding:24px;line-height:1.7"><h1>请从 EMP 打开管理页</h1><p>此浏览器尚未登录，或登录已过期。</p><p>请打开 EMP 启动时自动弹出的网页；也可以使用终端中 Open in browser 后的完整链接。</p><p>登录有效期为 30 天，期间重启 EMP 无需重新登录。</p></body></html>"#;

const LOGIN_HTML_BYTES: &[u8] = LOGIN_HTML.as_bytes();

pub(crate) fn ui_response(cookie: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        WEB_INDEX_BYTES,
        &[("Cache-Control", "no-store"), ("Set-Cookie", cookie)],
    )
}

pub(crate) fn login_response() -> Vec<u8> {
    response(
        "HTTP/1.1 401 Unauthorized",
        "text/html; charset=utf-8",
        LOGIN_HTML_BYTES,
        &[("Cache-Control", "no-store")],
    )
}

pub(crate) fn redirect_response(cookie: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 303 See Other",
        "text/plain; charset=utf-8",
        b"",
        &[("Location", "/"), ("Set-Cookie", cookie)],
    )
}

pub(crate) const VISION_TEST_IMAGE_BYTES: &[u8] =
    include_bytes!("../../../easy_multi_provider/web/vision-test-icon.png");
