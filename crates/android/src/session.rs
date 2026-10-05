//! One live sync session, owned by the JNI layer.
//!
//! Holds the tokio runtime the engine runs on and the audio sink, so the whole
//! thing dies together when Kotlin calls `stop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use ymsync::api::{self, Track, YandexMusic};
use ymsync::config::Config;
use ymsync::engine::{Command, Handle};
use ymsync::playback::Playback;
use ymsync::player::Player;
use ymsync::session::{self as core, Session as CoreSession};
use ymsync_proto::TrackRef;

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
    /// Keeps music on this device: the whole queue, or just what is playing.
    Download { queue: bool },
    /// Forgets everything still waiting to be downloaded.
    CancelDownloads,
    /// Deletes one track from this device.
    Forget { track_id: String },
    /// Puts a track into «Мне нравится», or takes it out. The screen sends the
    /// whole track, so a like from a search result needs no lookup.
    Like { track: TrackRef, liked: bool },
    /// Re-reads «Мне нравится» from Yandex.
    RefreshLikes,
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
            Request::CancelDownloads => Command::CancelDownloads,
            Request::Forget { track_id } => Command::Forget { track_id },
            Request::Like { track, liked } => Command::Like { track, liked },
            Request::RefreshLikes => Command::RefreshLikes,
            Request::Download { .. } => return None,
        })
    }
}

pub struct Session {
    runtime: tokio::runtime::Runtime,
    /// The engine plus whatever it brought up: a relay when this phone hosts the
    /// room, and the file server offering its downloads to the others.
    core: CoreSession,
    /// `None` when the phone has no Yandex token: it then plays what it has
    /// downloaded, and whatever the room's other devices can supply.
    api: Option<Arc<YandexMusic>>,
    /// Queue revision Kotlin has already been given; see [`Session::snapshot`].
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

        // Optional: a phone with no token still plays what it downloaded.
        let api = match cfg.yandex_token.trim() {
            "" => None,
            token => Some(Arc::new(YandexMusic::new(token)?)),
        };
        // Opening the audio device blocks; this call already runs on a Kotlin
        // background thread. The same player as the desktop: the track is decoded
        // from memory, so seeking costs a decoder reset instead of a re-buffer.
        let player = Arc::new(Player::new(cfg.volume).context("не удалось открыть звук")?);
        // Kotlin passes an app-private path in `cache.dir`, which is the only
        // place this process may write without asking for a permission.
        let cache = core::open_cache(&cfg)?;
        // «Мне нравится» lives beside the downloads, and like them it is read off
        // the disk first, so the hearts are right before Yandex has answered.
        let likes = core::open_likes(&cache);
        let core = runtime.block_on(core::start(
            &cfg,
            api.clone(),
            player as Arc<dyn Playback>,
            cache,
            likes,
        ))?;

        Ok(Self {
            runtime,
            core,
            api,
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

    /// The one call Kotlin makes on a timer: what to draw.
    ///
    /// The queue is left out unless it changed. «Мне нравится» runs to well over
    /// a thousand tracks, and serialising that five times a second would cost
    /// more than the playback itself.
    pub fn snapshot(&self) -> serde_json::Value {
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
        snapshot
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

    /// This account's «Мне нравится», as last read.
    ///
    /// Off the disk like [`Session::library`], so the screen can list it — and
    /// draw its hearts — with no network at all. The engine refreshes it from
    /// Yandex on connecting and whenever the phone asks.
    pub fn likes(&self) -> serde_json::Value {
        let likes = self.engine().likes();
        serde_json::json!({
            "tracks": likes.tracks().as_ref(),
            "updated_ms": likes.updated_ms(),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<TrackRef>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let api = self.api()?;
        let found = self
            .runtime
            .block_on(api.search_tracks(query.trim(), limit.clamp(1, 50)))?;
        Ok(found.iter().map(Track::to_track_ref).collect())
    }

    /// What to offer while the listener is still typing; see `api::suggest`.
    pub fn suggest(&self, part: &str) -> Result<serde_json::Value> {
        let part = part.trim();
        if part.is_empty() {
            return Ok(serde_json::json!({ "suggestions": [] }));
        }
        let api = self.api()?;
        let found = self.runtime.block_on(api.suggest(part))?;
        Ok(serde_json::to_value(found)?)
    }

    /// Queues tracks the screen already holds in full.
    ///
    /// This is how a row is played: a search result and a downloaded track both
    /// arrive complete — title, artist, length — so asking Yandex to describe them
    /// again would be a pointless request, and for a track on this disk it would
    /// mean going to the internet to play something that is already here. The
    /// engine then resolves the audio itself, in its own order: this disk, then a
    /// peer on the local network, then Yandex.
    pub fn queue_tracks(&self, tracks_json: &str, replace: bool) -> Result<usize> {
        let tracks: Vec<TrackRef> =
            serde_json::from_str(tracks_json).context("разбор списка треков")?;
        if tracks.is_empty() {
            bail!("пустой список треков");
        }
        Ok(self.queue(tracks, replace))
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

        let api = self.api()?;
        let tracks = self.runtime.block_on(async {
            match kind {
                "track" => {
                    let id = api::parse_track_id(value)?;
                    Ok(vec![api.track(&id).await?])
                }
                "search" => {
                    let found = api.search_tracks(value.trim(), 1).await?;
                    if found.is_empty() {
                        bail!("по запросу «{value}» ничего не найдено");
                    }
                    Ok(found)
                }
                "album" => {
                    let id = api::parse_album_id(value)?;
                    api.album_tracks(&id).await
                }
                "playlist" => {
                    let (owner, number) = api::parse_playlist_ref(value)?;
                    api.playlist_tracks(&owner, &number).await
                }
                "likes" => api.liked_tracks().await,
                other => bail!("неизвестный источник: {other}"),
            }
        })?;

        Ok(self.queue(
            tracks.iter().map(Track::to_track_ref).collect(),
            replace,
        ))
    }

    /// Adds a local audio file to this device's downloads.
    ///
    /// The phone reads the bytes itself: its picker hands back a content URI,
    /// which is not a path this process could open. Needs no account and no
    /// network — the file is simply stored the way a download would be.
    pub fn import(&self, name: &str, data: Vec<u8>) -> Result<serde_json::Value> {
        let imported = ymsync::import::from_bytes(self.engine().cache().as_ref(), name, data)?;
        // Written past the engine, so the engine has to be told: it keeps its own
        // list of what this phone can serve to the room.
        if !imported.already_there {
            self.engine().send(Command::CacheChanged);
        }
        Ok(serde_json::json!({
            "track": imported.track,
            "bytes": imported.bytes,
            "already_there": imported.already_there,
        }))
    }

    /// The Yandex client, or the one error worth reporting without it.
    fn api(&self) -> Result<Arc<YandexMusic>> {
        self.api
            .clone()
            .context("нет токена Яндекса: впишите его в настройках — без него играет только скачанное")
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
            (r#"{"action":"refresh_likes"}"#, Command::RefreshLikes),
        ];
        for (json, expected) in cases {
            let request: Request = serde_json::from_str(json).expect(json);
            assert_eq!(request.command(), Some(expected), "{json}");
        }
    }

    /// A heart carries the whole track, so liking a search result costs no lookup
    /// — the same reason a row is played by sending its track rather than its id.
    #[test]
    fn a_like_carries_the_track_it_means() {
        let json = r#"{"action":"like","liked":true,
            "track":{"track_id":"42","album":"Легенда","title":"T","artist":"A","duration_ms":1000}}"#;
        let request: Request = serde_json::from_str(json).expect(json);
        match request.command() {
            Some(Command::Like { track, liked }) => {
                assert_eq!(track.track_id, "42");
                assert_eq!(track.album.as_deref(), Some("Легенда"));
                assert!(liked);
            }
            other => panic!("не лайк: {other:?}"),
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

    /// The shape the screen sends when a row is tapped. Nothing here may need the
    /// network: that is the whole point of the call.
    #[test]
    fn a_row_from_the_screen_parses_into_tracks() {
        let json = r#"[
            {"track_id":"42","title":"T","artist":"A","duration_ms":1000},
            {"track_id":"43","album_id":"7","title":"U","artist":"B","duration_ms":2000}
        ]"#;
        let tracks: Vec<TrackRef> = serde_json::from_str(json).expect("tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, "42");
        assert_eq!(tracks[1].album_id.as_deref(), Some("7"));
        assert_eq!(tracks[1].duration_ms, 2000);
    }
}
