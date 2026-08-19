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
use ymsync::engine::{self, Command, Handle};
use ymsync::playback::Playback;
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
}

impl From<Request> for Command {
    fn from(request: Request) -> Self {
        match request {
            Request::Toggle => Command::TogglePause,
            Request::Next => Command::Next,
            Request::Prev => Command::Prev,
            Request::Seek { to_ms } => Command::SeekTo(to_ms),
            Request::SeekBy { delta_ms } => Command::SeekBy(delta_ms),
            Request::Volume { value } => Command::SetVolume(value),
            Request::Index { index } => Command::PlayIndex(index),
            Request::StopWave => Command::StopStation,
        }
    }
}

pub struct Session {
    runtime: tokio::runtime::Runtime,
    engine: Handle,
    api: Arc<YandexMusic>,
    playback: Arc<ExternalPlayback>,
    /// Queue revision Kotlin has already been given; see [`Session::poll`].
    sent_revision: AtomicU64,
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
        let engine = runtime.block_on(engine::spawn(
            &cfg,
            Arc::clone(&api),
            Arc::clone(&playback) as Arc<dyn Playback>,
        ))?;

        Ok(Self {
            runtime,
            engine,
            api,
            playback,
            // Revisions start at 1, so the first queue is always sent.
            sent_revision: AtomicU64::new(0),
        })
    }

    /// The one call Kotlin makes on a timer: hand in the player's state, take out
    /// the engine snapshot and whatever the engine wants the player to do.
    ///
    /// The queue is left out unless it changed. «Мне нравится» runs to well over
    /// a thousand tracks, and serialising that five times a second would cost
    /// more than the playback itself.
    pub fn poll(&self, state: PlayerState) -> serde_json::Value {
        self.playback.report(state);

        let snapshot = self.engine.snapshot();
        let revision = snapshot.queue_revision;
        let known = self.sent_revision.swap(revision, Ordering::Relaxed) == revision;

        let mut snapshot = serde_json::to_value(&snapshot).unwrap_or_default();
        if known && let Some(object) = snapshot.as_object_mut() {
            object.remove("queue");
        }

        serde_json::json!({
            "snapshot": snapshot,
            "commands": self.playback.drain(),
        })
    }

    pub fn send(&self, request: Request) {
        self.engine.send(request.into());
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
            self.engine.send(Command::PlayStation {
                id: api::WAVE_STATION.to_string(),
                replace,
            });
            return Ok(0);
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

        let queue: Vec<TrackRef> = tracks.iter().map(Track::to_track_ref).collect();
        let length = queue.len();
        self.engine.send(if replace {
            Command::SetQueue {
                tracks: queue,
                start: 0,
            }
        } else {
            Command::Enqueue { tracks: queue }
        });
        Ok(length)
    }

    pub fn stop(self) {
        let Session {
            runtime, engine, ..
        } = self;
        engine.send(Command::Shutdown);
        // Give the loop a moment to say goodbye to the relay.
        let _ = runtime.block_on(engine.join());
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
        ];
        for (json, expected) in cases {
            let request: Request = serde_json::from_str(json).expect(json);
            assert_eq!(Command::from(request), expected, "{json}");
        }
    }

    #[test]
    fn an_unknown_action_is_rejected() {
        assert!(serde_json::from_str::<Request>(r#"{"action":"launch_rocket"}"#).is_err());
    }
}
