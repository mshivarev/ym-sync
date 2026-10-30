//! The audio backend the engine drives.
//!
//! Desktop buffers the whole track in memory, which makes drift correction a
//! decoder reset rather than a network round trip. On Android the player is
//! ExoPlayer, living on the Kotlin side: it streams the signed URL itself and
//! reports its playhead back, so the engine never touches audio there.

use anyhow::Result;
use bytes::Bytes;
use ymsync_proto::TrackRef;

/// What a backend is handed to start a track.
pub enum AudioSource {
    /// The whole encoded track. Seeking is then free of the network.
    Bytes(Bytes),
    /// The track while it is still downloading: playback starts on the first
    /// part, and a read past what has arrived waits for it. Only for a backend
    /// that answers [`Playback::plays_while_downloading`].
    Stream(crate::stream::Progressive),
    /// A signed URL for a backend that streams on its own.
    Url(String),
}

/// Everything the sync engine needs from a player.
///
/// All methods are non-blocking and cheap: the engine calls the observers on
/// every correction tick.
pub trait Playback: Send + Sync + 'static {
    /// Whether the engine should download the track before calling [`Playback::load`].
    /// A streaming backend answers `false` and receives [`AudioSource::Url`].
    fn needs_bytes(&self) -> bool;

    /// Whether this backend can start on a track that is still downloading, given
    /// as [`AudioSource::Stream`]. Those that cannot get the whole file instead.
    fn plays_while_downloading(&self) -> bool {
        false
    }

    /// Stages `track`, paused at its start.
    fn load(&self, track: TrackRef, source: AudioSource) -> Result<()>;

    fn play(&self);
    fn pause(&self);
    fn seek_ms(&self, ms: u64) -> Result<()>;

    fn position_ms(&self) -> u64;
    /// True only while audio is actually being produced.
    fn is_playing(&self) -> bool;
    /// True once the staged track has played out, or nothing is staged.
    fn is_finished(&self) -> bool;
    /// Which track is staged. The backend is the single source of truth for it.
    fn current_track_id(&self) -> Option<String>;

    fn set_volume(&self, volume: f32);
    /// The equaliser's bands, in decibels; see [`crate::eq`]. A backend that
    /// cannot filter ignores it.
    fn set_equalizer(&self, _gains: &[f32]) {}
    fn volume(&self) -> f32;
}

/// How many decibels the slider spans, from the top down to where it fades out.
const RANGE_DB: f32 = 40.0;
/// Below this the slider stops stepping in decibels and fades linearly to
/// silence, so the very bottom is still off.
const FADE_BELOW: f32 = 0.1;

/// The gain for a volume slider at `slider` (0.0 to 1.0).
///
/// Even steps in decibels, because hearing is logarithmic: every tenth of the
/// slider is 4 dB. Half-way is −20 dB, a fifth −32 dB and a tenth −36 dB —
/// quiet but still there, which the old cube (−60 dB at a tenth) was not. The
/// last tenth fades to nothing.
pub fn loudness(slider: f32) -> f32 {
    let slider = if slider.is_finite() { slider.clamp(0.0, 1.0) } else { 0.0 };
    let gain = 10f32.powf(-RANGE_DB * (1.0 - slider) / 20.0);
    if slider < FADE_BELOW {
        gain * slider / FADE_BELOW
    } else {
        gain
    }
}

/// The slider position that gives `gain`: the inverse of [`loudness`].
pub fn slider_for(gain: f32) -> f32 {
    if !gain.is_finite() || gain <= 0.0 {
        return 0.0;
    }
    if gain >= 1.0 {
        return 1.0;
    }
    // `loudness` only rises, so halving the interval finds it to well under
    // a hundredth of a percent.
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..32 {
        let mid = (low + high) / 2.0;
        if loudness(mid) < gain {
            low = mid;
        } else {
            high = mid;
        }
    }
    (low + high) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_slider_follows_hearing() {
        assert_eq!(loudness(0.0), 0.0);
        assert_eq!(loudness(1.0), 1.0);
        let db = |slider: f32| 20.0 * loudness(slider).log10();
        assert!((db(0.5) + 20.0).abs() < 0.01, "{}", db(0.5));
        assert!((db(0.1) + 36.0).abs() < 0.01, "{}", db(0.1));
        assert!(db(0.05) > -45.0, "the bottom is still audible: {}", db(0.05));
        assert_eq!(loudness(7.0), 1.0, "no amplifying past full");
        for step in 0..=100 {
            let slider = step as f32 / 100.0;
            assert!(loudness(slider) >= loudness((slider - 0.01).max(0.0)));
            assert!((slider_for(loudness(slider)) - slider).abs() < 1e-4, "{slider}");
        }
    }
}
