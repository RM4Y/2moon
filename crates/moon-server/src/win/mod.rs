//! Windows host: DXGI screen capture, WASAPI loopback audio, SendInput + ViGEm
//! input, apps started as the signed-in user, and the Windows service.

pub mod audio;
pub mod capture;
pub mod input;
pub mod service;
pub mod session;
pub mod tray;

use tracing::info;

/// The hardware encoder family to use: the first of NVIDIA (NVENC), AMD (AMF) and
/// Intel (Quick Sync) whose H.264 encoder FFmpeg can open; "software" otherwise.
pub fn detect_encoder() -> String {
    for (family, vendor) in [("nvenc", "NVIDIA NVENC"), ("amf", "AMD AMF"), ("qsv", "Intel Quick Sync")] {
        match moon_media::ffenc::probe(&format!("h264_{family}")) {
            Ok(()) => {
                info!("video encoder: {vendor}");
                return family.into();
            }
            Err(e) => info!("{vendor}: {e:#}"),
        }
    }
    tracing::warn!("no hardware video encoder found: software H.264 (OpenH264)");
    "software".into()
}
