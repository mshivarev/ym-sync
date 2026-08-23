//! ym-sync core: the Yandex Music client, the audio player, the relay link and
//! the sync engine that ties them together.
//!
//! Front ends (the `ymsync` CLI, the Tauri desktop app, the Android JNI layer)
//! all drive [`engine`]; nothing in this crate reads stdin or prints.

#![forbid(unsafe_code)]

pub mod api;
pub mod cache;
pub mod config;
pub mod discover;
pub mod engine;
pub mod link;
pub mod net;
pub mod playback;
pub mod session;
pub mod share;

/// The desktop audio backend. Absent on Android, where ExoPlayer plays instead
/// and `rodio`/`cpal` would only be dead weight.
#[cfg(feature = "audio-rodio")]
pub mod player;

/// Installs the process-wide crypto provider that `rustls` needs.
///
/// `reqwest` is built with `rustls-no-provider`, which leaves the choice to the
/// application. The API client picks its provider explicitly, but the WebSocket
/// client builds its own configuration from the process default, so this must run
/// once before connecting. Calling it repeatedly is harmless.
#[cfg(feature = "tls-rustls")]
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Formats a duration as `m:ss`.
pub fn fmt_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_format_as_minutes_and_seconds() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(9_000), "0:09");
        assert_eq!(fmt_ms(65_000), "1:05");
        assert_eq!(fmt_ms(286_000), "4:46");
        assert_eq!(fmt_ms(3_600_000), "60:00");
    }
}
