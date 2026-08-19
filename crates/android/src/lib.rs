//! JNI surface for the Android client.
//!
//! Only the audio backend differs from the desktop build: ExoPlayer plays, and
//! this layer relays its playhead into the engine and the engine's decisions
//! back out. Everything else — the Yandex API, the relay protocol, clock
//! estimation, drift correction, the queue — is the same code the desktop runs.
//!
//! Kotlin drives it by polling: one call hands in the player's current state and
//! receives the engine snapshot plus any pending player commands. Polling avoids
//! calling back into the JVM from Rust threads, which would mean attaching them
//! and juggling global references.

use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use ymsync::playback::{AudioSource, Playback};
use ymsync_proto::TrackRef;

mod ffi;
mod session;

/// What ExoPlayer is doing right now, pushed in from Kotlin on every poll.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PlayerState {
    pub position_ms: u64,
    pub playing: bool,
    /// The staged track has played out.
    pub finished: bool,
    /// Which track ExoPlayer actually has prepared. The engine waits for this to
    /// match before it corrects anything, so a half-loaded track is never
    /// treated as ready.
    pub loaded_track_id: Option<String>,
    pub volume: f32,
}

/// What the engine wants ExoPlayer to do, drained by Kotlin on every poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlayerCommand {
    /// Stream this URL. It is already signed and needs no headers.
    Load {
        track_id: String,
        title: String,
        artist: String,
        duration_ms: u64,
        url: String,
    },
    Play,
    Pause,
    Seek { to_ms: u64 },
    Volume { value: f32 },
}

/// The `Playback` implementation backed by ExoPlayer across the JNI boundary.
#[derive(Default)]
pub struct ExternalPlayback {
    state: Mutex<PlayerState>,
    pending: Mutex<Vec<PlayerCommand>>,
}

impl ExternalPlayback {
    pub fn new(volume: f32) -> Self {
        Self {
            state: Mutex::new(PlayerState {
                volume,
                ..PlayerState::default()
            }),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Called from the JNI poll: store what the player reports.
    pub fn report(&self, state: PlayerState) {
        *self.state.lock().expect("player state") = state;
    }

    /// Called from the JNI poll: hand over everything the engine asked for.
    pub fn drain(&self) -> Vec<PlayerCommand> {
        std::mem::take(&mut *self.pending.lock().expect("player commands"))
    }

    fn push(&self, command: PlayerCommand) {
        let mut pending = self.pending.lock().expect("player commands");
        // Repeated transport commands collapse: only the latest matters, and a
        // slow poll must not build up a backlog of stale seeks.
        match &command {
            PlayerCommand::Seek { .. } => {
                pending.retain(|queued| !matches!(queued, PlayerCommand::Seek { .. }));
            }
            PlayerCommand::Volume { .. } => {
                pending.retain(|queued| !matches!(queued, PlayerCommand::Volume { .. }));
            }
            PlayerCommand::Play | PlayerCommand::Pause => {
                pending.retain(|queued| {
                    !matches!(queued, PlayerCommand::Play | PlayerCommand::Pause)
                });
            }
            PlayerCommand::Load { .. } => pending.clear(),
        }
        pending.push(command);
    }
}

impl Playback for ExternalPlayback {
    /// ExoPlayer streams the signed URL itself; buffering nine megabytes in Rust
    /// and copying it across JNI would be pure waste on a phone.
    fn needs_bytes(&self) -> bool {
        false
    }

    fn load(&self, track: TrackRef, source: AudioSource) -> Result<()> {
        let AudioSource::Url(url) = source else {
            anyhow::bail!("ExoPlayer ожидает ссылку, а не байты");
        };
        self.push(PlayerCommand::Load {
            track_id: track.track_id,
            title: track.title,
            artist: track.artist,
            duration_ms: track.duration_ms,
            url,
        });
        Ok(())
    }

    fn play(&self) {
        self.push(PlayerCommand::Play);
    }

    fn pause(&self) {
        self.push(PlayerCommand::Pause);
    }

    fn seek_ms(&self, ms: u64) -> Result<()> {
        self.push(PlayerCommand::Seek { to_ms: ms });
        Ok(())
    }

    fn position_ms(&self) -> u64 {
        self.state.lock().expect("player state").position_ms
    }

    fn is_playing(&self) -> bool {
        self.state.lock().expect("player state").playing
    }

    fn is_finished(&self) -> bool {
        self.state.lock().expect("player state").finished
    }

    fn current_track_id(&self) -> Option<String> {
        self.state
            .lock()
            .expect("player state")
            .loaded_track_id
            .clone()
    }

    fn set_volume(&self, volume: f32) {
        self.push(PlayerCommand::Volume { value: volume });
    }

    fn volume(&self) -> f32 {
        self.state.lock().expect("player state").volume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackRef {
        TrackRef {
            track_id: "42".into(),
            album_id: None,
            title: "T".into(),
            artist: "A".into(),
            duration_ms: 1000,
        }
    }

    #[test]
    fn the_engine_sees_only_what_the_player_confirms() {
        let playback = ExternalPlayback::new(0.8);
        playback
            .load(track(), AudioSource::Url("https://x/y.mp3".into()))
            .unwrap();

        // Asking for a track must not make it look staged: ExoPlayer has not
        // reported it yet, so the engine has to keep waiting.
        assert_eq!(playback.current_track_id(), None);

        playback.report(PlayerState {
            loaded_track_id: Some("42".into()),
            position_ms: 500,
            playing: true,
            ..PlayerState::default()
        });
        assert_eq!(playback.current_track_id().as_deref(), Some("42"));
        assert_eq!(playback.position_ms(), 500);
        assert!(playback.is_playing());
    }

    #[test]
    fn a_load_is_reported_with_everything_exoplayer_needs() {
        let playback = ExternalPlayback::new(1.0);
        playback
            .load(track(), AudioSource::Url("https://x/y.mp3".into()))
            .unwrap();
        assert_eq!(
            playback.drain(),
            vec![PlayerCommand::Load {
                track_id: "42".into(),
                title: "T".into(),
                artist: "A".into(),
                duration_ms: 1000,
                url: "https://x/y.mp3".into(),
            }]
        );
    }

    #[test]
    fn bytes_are_refused() {
        let playback = ExternalPlayback::new(1.0);
        let source = AudioSource::Bytes(bytes::Bytes::from_static(b"mp3"));
        assert!(playback.load(track(), source).is_err());
    }

    #[test]
    fn superseded_seeks_do_not_pile_up() {
        let playback = ExternalPlayback::new(1.0);
        playback.seek_ms(1_000).unwrap();
        playback.seek_ms(2_000).unwrap();
        playback.seek_ms(3_000).unwrap();
        assert_eq!(playback.drain(), vec![PlayerCommand::Seek { to_ms: 3_000 }]);
    }

    #[test]
    fn pause_replaces_a_pending_play() {
        let playback = ExternalPlayback::new(1.0);
        playback.play();
        playback.pause();
        assert_eq!(playback.drain(), vec![PlayerCommand::Pause]);
    }

    #[test]
    fn a_new_track_discards_commands_meant_for_the_old_one() {
        let playback = ExternalPlayback::new(1.0);
        playback.seek_ms(9_000).unwrap();
        playback.play();
        playback
            .load(track(), AudioSource::Url("https://x/y.mp3".into()))
            .unwrap();
        assert!(matches!(
            playback.drain().as_slice(),
            [PlayerCommand::Load { .. }]
        ));
    }

    #[test]
    fn draining_twice_yields_nothing_the_second_time() {
        let playback = ExternalPlayback::new(1.0);
        playback.play();
        assert_eq!(playback.drain().len(), 1);
        assert!(playback.drain().is_empty());
    }
}
