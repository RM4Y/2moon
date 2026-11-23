//! macOS host: ScreenCaptureKit screen and audio capture, CoreGraphics input,
//! VideoToolbox encoding (through FFmpeg).

pub mod audio;
pub mod capture;
pub mod input;
pub mod sck;

use tracing::info;

/// VideoToolbox when FFmpeg can open its H.264 encoder, else software H.264.
pub fn detect_encoder() -> String {
    match moon_media::ffenc::probe("h264_videotoolbox") {
        Ok(()) => {
            info!("video encoder: Apple VideoToolbox");
            "videotoolbox".into()
        }
        Err(e) => {
            tracing::warn!("VideoToolbox unavailable ({e:#}): software H.264 (OpenH264)");
            "software".into()
        }
    }
}

/// The Mac's name as set in System Settings › General › Sharing.
pub fn computer_name() -> Option<String> {
    let out = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
