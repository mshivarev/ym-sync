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
    fn volume(&self) -> f32;
}
