//! One live sync session, owned by the JNI layer.
//!
//! Holds the tokio runtime the engine runs on, so the whole thing dies together
//! when Kotlin calls `stop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use ymsync::api::{self, Track, YandexMusic};
use ymsync::config::Config;
use ymsync::engine::{Command, Handle};
use ymsync::playback::Playback;
use ymsync::session::{self as core, Session as CoreSession};
use ymsync_proto::TrackRef;

use crate::{ExternalPlayback, PlayerState};

/// A playback request from the UI. Mirrors [`engine::Command`], minus the parts
/// the Android UI has no business sending.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Toggle,
    Next,
    Prev,
    Seek { to_ms: u64 },
    SeekBy { delta_ms: i64 },
    Volume { value: f32 },
    Index { index: usize },
    /// Stops feeding the wave. What is already queued still plays.
    StopWave,
    /// Recalibrates ExoPlayer's reporting lag without a reconnect.
    Bias { ms: i64 },
    /// Keeps music on this device: the whole queue, or just what is playing.
    Download { queue: bool },
    /// Forgets everything still waiting to be downloaded.
    CancelDownloads,
    /// Deletes one track from this device.
    Forget { track_id: String },
}

impl Request {
    /// The engine command this request means, where that does not depend on what
    /// is currently on screen.
    ///
    /// [`Request::Download`] is the exception: the UI says *which* tracks it means
    /// — this one, or everything queued — rather than naming ids, so
    /// [`Session::send`] resolves it against the latest snapshot.
    fn command(self) -> Option<Command> {
        Some(match self {
            Request::Toggle => Command::TogglePause,
            Request::Next => Command::Next,
            Request::Prev => Command::Prev,
            Request::Seek { to_ms } => Command::SeekTo(to_ms),
            Request::SeekBy { delta_ms } => Command::SeekBy(delta_ms),
            Request::Volume { value } => Command::SetVolume(value),
            Request::Index { index } => Command::PlayIndex(index),
            Request::StopWave => Command::StopStation,
            Request::Bias { ms } => Command::SetPositionBias(ms),
            Request::CancelDownloads => Command::CancelDownloads,
            Request::Forget { track_id } => Command::Forget { track_id },
            Request::Download { .. } => return None,
        })
    }
}

pub struct Session {
    runtime: tokio::runtime::Runtime,
    /// The engine plus whatever it brought up: a relay when this phone hosts the
    /// room, and the file server offering its downloads to the others.
    core: CoreSession,
    api: Arc<YandexMusic>,
    playback: Arc<ExternalPlayback>,
    /// Queue revision Kotlin has already been given; see [`Session::poll`].
    sent_revision: AtomicU64,
    /// Same idea for the list of downloaded ids.
    sent_cache_revision: AtomicU64,
}

impl Session {
    pub fn start(config_json: &str) -> Result<Self> {
        let cfg: Config = serde_json::from_str(config_json).context("разбор настроек")?;

        // `rustls` has no default provider in this build, and the WebSocket
        // client picks one from the process, so this has to happen first.
        ymsync::install_tls_provider();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ymsync-rt")
            .enable_all()
            .build()
            .context("создание рантайма")?;

        let api = Arc::new(YandexMusic::new(cfg.require_yandex_token()?)?);
        let playback = Arc::new(ExternalPlayback::new(cfg.volume));
        // Kotlin passes an app-private path in `cache.dir`, which is the only
        // place this process may write without asking for a permission.
        let cache = core::open_cache(&cfg)?;
        let core = runtime.block_on(core::start(
            &cfg,
            Arc::clone(&api),
            Arc::clone(&playback) as Arc<dyn Playback>,
            cache,
        ))?;

        Ok(Self {
            runtime,
            core,
            api,
            playback,
            // Revisions start at 1, so the first queue is always sent.
            sent_revision: AtomicU64::new(0),
            // The cache revision does start at 0, so this one has to begin
            // somewhere it can never match.
            sent_cache_revision: AtomicU64::new(u64::MAX),
        })
    }

    fn engine(&self) -> &Handle {
        self.core.handle()
    }

    /// The one call Kotlin makes on a timer: hand in the player's state, take out
    /// the engine snapshot and whatever the engine wants the player to do.
    ///
    /// The queue is left out unless it changed. «Мне нравится» runs to well over
    /// a thousand tracks, and serialising that five times a second would cost
    /// more than the playback itself.
    pub fn poll(&self, state: PlayerState) -> serde_json::Value {
        self.playback.report(state);

        let snapshot = self.engine().snapshot();
        let revision = snapshot.queue_revision;
        let known = self.sent_revision.swap(revision, Ordering::Relaxed) == revision;
        let cache_revision = snapshot.cache_revision;
        let cache_known =
            self.sent_cache_revision.swap(cache_revision, Ordering::Relaxed) == cache_revision;

        let mut snapshot = serde_json::to_value(&snapshot).unwrap_or_default();
        if let Some(object) = snapshot.as_object_mut() {
            if known {
                object.remove("queue");
            }
            // For the same reason as the queue: a well-used offline library runs
            // into the thousands of ids.
            if cache_known {
                object.remove("cached");
            }
        }

        serde_json::json!({
            "snapshot": snapshot,
            "commands": self.playback.drain(),
        })
    }

    pub fn send(&self, request: Request) {
        if let Request::Download { queue } = request {
            let snapshot = self.engine().snapshot();
            let tracks = if queue {
                snapshot.queue
            } else {
                snapshot.track.into_iter().collect()
            };
            if !tracks.is_empty() {
                self.engine().send(Command::Download { tracks });
            }
            return;
        }
        if let Some(command) = request.command() {
            self.engine().send(command);
        }
    }

    /// What is on this device's disk, for the offline screen.
    ///
    /// Reads the in-memory index only, so Kotlin may call it straight from the UI
    /// thread — and it answers with no network at all.
    pub fn library(&self) -> serde_json::Value {
        let cache = self.engine().cache();
        serde_json::json!({
            "tracks": cache.tracks(),
            "bytes": cache.total_bytes(),
            "limit_bytes": cache.limit_bytes(),
            "directory": cache.directory().display().to_string(),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<TrackRef>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let found = self
            .runtime
            .block_on(self.api.search_tracks(query.trim(), limit.clamp(1, 50)))?;
        Ok(found.iter().map(Track::to_track_ref).collect())
    }

    /// Resolves a source into tracks and hands them to the engine. Returns how
    /// many were added; a station reports 0, since it delivers as it goes.
    pub fn queue_from(&self, kind: &str, value: &str, replace: bool) -> Result<usize> {
        // The wave has no track list to resolve: the engine follows the station
        // and asks it for more as the queue runs down.
        if kind == "wave" {
            self.engine().send(Command::PlayStation {
                id: api::WAVE_STATION.to_string(),
                replace,
            });
            return Ok(0);
        }

        // Neither does the offline library: its metadata is on disk beside the
        // audio, which is what makes this work with no internet.
        if kind == "offline" {
            let tracks: Vec<TrackRef> = self
                .engine()
                .cache()
                .tracks()
                .into_iter()
                .map(|entry| entry.track)
                .collect();
            if tracks.is_empty() {
                bail!("на этом устройстве ничего не скачано");
            }
            return Ok(self.queue(tracks, replace));
        }

        let tracks = self.runtime.block_on(async {
            match kind {
                "track" => {
                    let id = api::parse_track_id(value)?;
                    Ok(vec![self.api.track(&id).await?])
                }
                "search" => {
                    let found = self.api.search_tracks(value.trim(), 1).await?;
                    if found.is_empty() {
                        bail!("по запросу «{value}» ничего не найдено");
                    }
                    Ok(found)
                }
                "album" => {
                    let id = api::parse_album_id(value)?;
                    self.api.album_tracks(&id).await
                }
                "playlist" => {
                    let (owner, number) = api::parse_playlist_ref(value)?;
                    self.api.playlist_tracks(&owner, &number).await
                }
                "likes" => self.api.liked_tracks().await,
                other => bail!("неизвестный источник: {other}"),
            }
        })?;

        Ok(self.queue(
            tracks.iter().map(Track::to_track_ref).collect(),
            replace,
        ))
    }

    /// Sends a resolved list to the room, replacing the queue or extending it.
    fn queue(&self, tracks: Vec<TrackRef>, replace: bool) -> usize {
        let length = tracks.len();
        self.engine().send(if replace {
            Command::SetQueue { tracks, start: 0 }
        } else {
            Command::Enqueue { tracks }
        });
        length
    }

    pub fn stop(self) {
        let Session { runtime, core, .. } = self;
        // Shuts the engine down first and the servers it brought up second, so a
        // hosted relay is still there to hear this player say goodbye.
        let _ = runtime.block_on(core.shutdown());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_map_onto_engine_commands() {
        let cases = [
            (r#"{"action":"toggle"}"#, Command::TogglePause),
            (r#"{"action":"next"}"#, Command::Next),
            (r#"{"action":"prev"}"#, Command::Prev),
            (r#"{"action":"seek","to_ms":1500}"#, Command::SeekTo(1_500)),
            (
                r#"{"action":"seek_by","delta_ms":-10000}"#,
                Command::SeekBy(-10_000),
            ),
            (r#"{"action":"index","index":3}"#, Command::PlayIndex(3)),
            (r#"{"action":"stop_wave"}"#, Command::StopStation),
            (r#"{"action":"bias","ms":400}"#, Command::SetPositionBias(400)),
            (
                r#"{"action":"cancel_downloads"}"#,
                Command::CancelDownloads,
            ),
            (
                r#"{"action":"forget","track_id":"42"}"#,
                Command::Forget {
                    track_id: "42".to_string(),
                },
            ),
        ];
        for (json, expected) in cases {
            let request: Request = serde_json::from_str(json).expect(json);
            assert_eq!(request.command(), Some(expected), "{json}");
        }
    }

    /// Downloading names a scope rather than ids, so it cannot become a command
    /// without the current snapshot.
    #[test]
    fn a_download_request_has_no_command_of_its_own() {
        for json in [
            r#"{"action":"download","queue":false}"#,
            r#"{"action":"download","queue":true}"#,
        ] {
            let request: Request = serde_json::from_str(json).expect(json);
            assert_eq!(request.command(), None, "{json}");
        }
    }

    #[test]
    fn an_unknown_action_is_rejected() {
        assert!(serde_json::from_str::<Request>(r#"{"action":"launch_rocket"}"#).is_err());
    }
}
