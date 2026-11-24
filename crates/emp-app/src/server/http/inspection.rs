use super::management::{ManagementRead, read_route};
use crate::{
    api::{diagnostics, images, inspection, support_report},
    app::ServerState,
};
use axum::Router;
use std::sync::Arc;

const READS: [(&str, ManagementRead); 6] = [
    ("/api/diagnostics", diagnostics::read),
    ("/api/support-report", support_report::read),
    ("/api/capabilities", inspection::capabilities),
    (
        "/api/models/vision-test-image",
        inspection::vision_test_image,
    ),
    ("/api/models/audio-test-sound", inspection::audio_test_sound),
    ("/api/image-credit", images::credit_status),
];

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    READS
        .into_iter()
        .fold(Router::new(), |router, (path, read)| {
            router.route(path, read_route(read))
        })
}
