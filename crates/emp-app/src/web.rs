//! Serve the browser application.

use crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE;
use crate::http::response::response;

/// The browser page and its feature assets ship with the same executable.
pub const WEB_INDEX_BYTES: &[u8] = include_bytes!("../web/index.html");

pub(crate) fn asset_response(path: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match path {
        "/assets/i18n.js" => include_bytes!("../web/i18n.js"),
        "/assets/copy-core.js" => include_bytes!("../web/copy-core.js"),
        "/assets/copy-services.js" => include_bytes!("../web/copy-services.js"),
        "/assets/copy-models.js" => include_bytes!("../web/copy-models.js"),
        "/assets/copy-reports.js" => include_bytes!("../web/copy-reports.js"),
        "/assets/copy-transfers.js" => include_bytes!("../web/copy-transfers.js"),
        "/assets/copy-recovery.js" => include_bytes!("../web/copy-recovery.js"),
        "/assets/service-presentation.js" => include_bytes!("../web/service-presentation.js"),
        "/assets/service-presentation.css" => include_bytes!("../web/service-presentation.css"),
        "/assets/migration-view.js" => include_bytes!("../web/migration-view.js"),
        "/assets/migration.js" => include_bytes!("../web/migration.js"),
        "/assets/migration.css" => include_bytes!("../web/migration.css"),
        "/assets/subscription-settings.js" => include_bytes!("../web/subscription-settings.js"),
        "/assets/model-manager.js" => include_bytes!("../web/model-manager.js"),
        "/assets/service-list.js" => include_bytes!("../web/service-list.js"),
        "/assets/service-usage.js" => include_bytes!("../web/service-usage.js"),
        "/assets/service-connections.js" => include_bytes!("../web/service-connections.js"),
        "/assets/service-chooser.js" => include_bytes!("../web/service-chooser.js"),
        "/assets/service-icons.js" => include_bytes!("../web/service-icons.js"),
        "/assets/service-list.css" => include_bytes!("../web/service-list.css"),
        "/assets/updates.js" => include_bytes!("../web/updates.js"),
        "/assets/release-notes.js" => include_bytes!("../web/release-notes.js"),
        "/assets/model-editor.js" => include_bytes!("../web/model-editor.js"),
        "/assets/context-help.js" => include_bytes!("../web/context-help.js"),
        "/assets/account-details.js" => include_bytes!("../web/account-details.js"),
        "/assets/account-import.js" => include_bytes!("../web/account-import.js"),
        "/assets/call-outcomes.js" => include_bytes!("../web/call-outcomes.js"),
        "/assets/call-reports.js" => include_bytes!("../web/call-reports.js"),
        "/assets/report-query.js" => include_bytes!("../web/report-query.js"),
        "/assets/period-controls.js" => include_bytes!("../web/period-controls.js"),
        "/assets/usage-report.js" => include_bytes!("../web/usage-report.js"),
        "/assets/management-events.js" => include_bytes!("../web/management-events.js"),
        "/assets/management-client.js" => include_bytes!("../web/management-client.js"),
        "/assets/quota-history.js" => include_bytes!("../web/quota-history.js"),
        "/assets/quota-display.js" => include_bytes!("../web/quota-display.js"),
        "/assets/settings.js" => include_bytes!("../web/settings.js"),
        "/assets/collaboration-comparison.js" => {
            include_bytes!("../web/collaboration-comparison.js")
        }
        "/assets/routing-settings.js" => include_bytes!("../web/routing-settings.js"),
        "/assets/diagnostics.js" => include_bytes!("../web/diagnostics.js"),
        "/assets/style.css" => include_bytes!("../web/style.css"),
        "/assets/brand-colors.js" => include_bytes!("../web/brand-colors.js"),
        "/assets/interactions.js" => include_bytes!("../web/interactions.js"),
        "/assets/motion.js" => include_bytes!("../web/motion.js"),
        "/assets/performance-chart.js" => include_bytes!("../web/performance-chart.js"),
        "/assets/performance.js" => include_bytes!("../web/performance.js"),
        "/assets/request-errors.js" => include_bytes!("../web/request-errors.js"),
        "/assets/service-errors.js" => include_bytes!("../web/service-errors.js"),
        "/assets/statistics.js" => include_bytes!("../web/statistics.js"),
        "/assets/session-bills.js" => include_bytes!("../web/session-bills.js"),
        "/assets/usage-data.js" => include_bytes!("../web/usage-data.js"),
        "/assets/presentation.css" => include_bytes!("../web/presentation.css"),
        "/assets/presentation.js" => include_bytes!("../web/presentation.js"),
        _ => return None,
    };
    Some(response(
        "HTTP/1.1 200 OK",
        if path.ends_with(".css") {
            "text/css; charset=utf-8"
        } else {
            "text/javascript; charset=utf-8"
        },
        bytes,
        &[("Cache-Control", "no-store")],
    ))
}

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

pub(crate) const VISION_TEST_IMAGE_BYTES: &[u8] = include_bytes!("../web/vision-test-icon.png");

/// One second of stereo speech: "Front left" on the left channel, then "Front right" on the right.
pub(crate) const AUDIO_TEST_WAV_BYTES: &[u8] = include_bytes!("../web/audio-test-channels.wav");
