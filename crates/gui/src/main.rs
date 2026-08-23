//! Desktop front end.
//!
//! A thin bridge: every command forwards to [`ymsync::engine`], and engine
//! snapshots are pushed to the page as `snapshot` events. No playback or sync
//! logic lives here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use ymsync::api::{self, Track, YandexMusic};
use ymsync::cache::{Cache, CachedTrack};
use ymsync::config::Config;
use ymsync::engine::{Command, Snapshot};
use ymsync::player::Player;
use ymsync::session::{self, Session};
use ymsync_proto::TrackRef;

struct AppState {
    config: Config,
    config_path: String,
    /// Built on first use, then reused; searching does not need a connection.
    api: Mutex<Option<Arc<YandexMusic>>>,
    /// Opened on first use and kept: the window lists the offline library before
    /// anything connects, and the engine then plays out of that same cache.
    cache: Mutex<Option<Arc<Cache>>>,
    /// The engine plus whatever it brought up — a hosted relay, the file server
    /// for cached tracks. Held whole so that disconnecting closes those ports.
    session: Mutex<Option<Session>>,
}

impl AppState {
    /// The API client, created on demand so the window can open even when no
    /// token is configured yet.
    async fn api(&self) -> Result<Arc<YandexMusic>, String> {
        let mut slot = self.api.lock().await;
        if let Some(api) = slot.as_ref() {
            return Ok(Arc::clone(api));
        }
        let token = self.config.require_yandex_token().map_err(fail)?;
        let api = Arc::new(YandexMusic::new(token).map_err(fail)?);
        *slot = Some(Arc::clone(&api));
        Ok(api)
    }

    async fn cache(&self) -> Result<Arc<Cache>, String> {
        let mut slot = self.cache.lock().await;
        if let Some(cache) = slot.as_ref() {
            return Ok(Arc::clone(cache));
        }
        let cache = session::open_cache(&self.config).map_err(fail)?;
        *slot = Some(Arc::clone(&cache));
        Ok(cache)
    }
}

/// Anything the page needs before connecting.
#[derive(Serialize)]
struct Settings {
    relay: String,
    room: String,
    volume: f32,
    has_yandex_token: bool,
    has_room_token: bool,
    config_path: String,
    /// This app will run the room's relay itself rather than dial one.
    hosting: bool,
    /// Address to advertise while hosting; empty means "work it out".
    advertise: String,
    /// Cached tracks will be offered to the rest of the room over the network.
    sharing: bool,
    cache_dir: String,
    /// 0 means no limit.
    cache_limit_bytes: u64,
    /// Everything that plays is kept, not only explicit downloads.
    auto_cache: bool,
}

#[tauri::command]
fn settings(state: State<'_, AppState>) -> Settings {
    let cfg = &state.config;
    Settings {
        relay: cfg.relay.clone(),
        room: cfg.room.clone(),
        volume: cfg.volume,
        has_yandex_token: !cfg.yandex_token.trim().is_empty(),
        has_room_token: !cfg.room_token.trim().is_empty(),
        config_path: state.config_path.clone(),
        hosting: cfg.host.enabled,
        advertise: cfg.host.advertise.clone(),
        sharing: cfg.share.enabled,
        // A path that cannot be worked out is not worth failing the whole window
        // over: the panel simply shows no location.
        cache_dir: cfg
            .cache
            .directory()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
        cache_limit_bytes: cfg.cache.limit_bytes(),
        auto_cache: cfg.cache.auto,
    }
}

/// Connects, optionally running the room's relay in this process.
///
/// `host` and `advertise` come from the window rather than from the file, and are
/// not written back — the same deal as `ymsync play --host`: the tick holds for
/// this run, and `config.toml` stays the default. Hosting has to be decided here
/// because the relay is bound before the engine connects to it.
#[tauri::command]
async fn connect(
    app: AppHandle,
    host: bool,
    advertise: String,
    state: State<'_, AppState>,
) -> Result<Snapshot, String> {
    let mut slot = state.session.lock().await;
    if slot.is_some() {
        return Err("уже подключено".to_string());
    }

    let api = state.api().await?;
    let cache = state.cache().await?;
    let volume = state.config.volume;
    // Opening the audio device blocks briefly.
    let player = tokio::task::spawn_blocking(move || Player::new(volume))
        .await
        .map_err(fail)?
        .map_err(fail)?;

    let mut cfg = state.config.clone();
    if host {
        cfg.host.enabled = true;
    }
    let advertise = advertise.trim();
    if !advertise.is_empty() {
        cfg.host.advertise = advertise.to_string();
    }

    // Hosting the room and serving cached tracks are both set up here, in the
    // order the engine needs them.
    let session = session::start(&cfg, api, Arc::new(player), cache)
        .await
        .map_err(fail)?;
    let snapshot = session.handle().snapshot();

    // Push every engine state change to the page.
    let mut snapshots = session.handle().subscribe();
    tauri::async_runtime::spawn(async move {
        while snapshots.changed().await.is_ok() {
            let snapshot = snapshots.borrow_and_update().clone();
            if app.emit("snapshot", snapshot).is_err() {
                break;
            }
        }
        let _ = app.emit("closed", ());
    });

    *slot = Some(session);
    Ok(snapshot)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(session) = state.session.lock().await.take() {
        session.shutdown().await.map_err(fail)?;
    }
    Ok(())
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<Option<Snapshot>, String> {
    Ok(state
        .session
        .lock()
        .await
        .as_ref()
        .map(|session| session.handle().snapshot()))
}

#[tauri::command]
async fn search(
    query: String,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<Vec<TrackRef>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let api = state.api().await?;
    let found = api
        .search_tracks(query.trim(), limit.clamp(1, 50))
        .await
        .map_err(fail)?;
    Ok(found.iter().map(Track::to_track_ref).collect())
}

/// Resolves `kind`/`value` into tracks and queues them. Returns how many were
/// added; the wave reports 0, since it delivers as it goes.
#[tauri::command]
async fn play_source(
    kind: String,
    value: String,
    replace: bool,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    // The wave has no list to resolve: the engine follows the station itself.
    if kind == "wave" {
        send(
            &state,
            Command::PlayStation {
                id: api::WAVE_STATION.to_string(),
                replace,
            },
        )
        .await?;
        return Ok(0);
    }

    let api = state.api().await?;
    let tracks = match kind.as_str() {
        "track" => {
            let id = api::parse_track_id(&value).map_err(fail)?;
            vec![api.track(&id).await.map_err(fail)?]
        }
        "search" => {
            let found = api.search_tracks(value.trim(), 1).await.map_err(fail)?;
            if found.is_empty() {
                return Err(format!("по запросу «{value}» ничего не найдено"));
            }
            found
        }
        "album" => {
            let id = api::parse_album_id(&value).map_err(fail)?;
            api.album_tracks(&id).await.map_err(fail)?
        }
        "playlist" => {
            let (owner, number) = api::parse_playlist_ref(&value).map_err(fail)?;
            api.playlist_tracks(&owner, &number).await.map_err(fail)?
        }
        "likes" => api.liked_tracks().await.map_err(fail)?,
        other => return Err(format!("неизвестный источник: {other}")),
    };

    let queue: Vec<TrackRef> = tracks.iter().map(Track::to_track_ref).collect();
    let length = queue.len();
    send(&state, queue_command(queue, 0, replace)).await?;
    Ok(length)
}

/// Queues an explicit list, e.g. one row from the search results.
#[tauri::command]
async fn queue_tracks(
    tracks: Vec<TrackRef>,
    start: usize,
    replace: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if tracks.is_empty() {
        return Err("пустой список".to_string());
    }
    send(&state, queue_command(tracks, start, replace)).await
}

fn queue_command(tracks: Vec<TrackRef>, start: usize, replace: bool) -> Command {
    if replace {
        Command::SetQueue { tracks, start }
    } else {
        Command::Enqueue { tracks }
    }
}

#[tauri::command]
async fn control(
    action: String,
    value: Option<f64>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let number = value.unwrap_or_default();
    let command = match action.as_str() {
        "toggle" => Command::TogglePause,
        "next" => Command::Next,
        "prev" => Command::Prev,
        "seek" => Command::SeekTo(number.max(0.0) as u64),
        "seek_by" => Command::SeekBy(number as i64),
        "volume" => Command::SetVolume(number as f32),
        "index" => Command::PlayIndex(number.max(0.0) as usize),
        "stop_wave" => Command::StopStation,
        other => return Err(format!("неизвестное действие: {other}")),
    };
    send(&state, command).await
}

/// This device's offline library, newest first.
///
/// Answers with no connection and no internet: the metadata was stored beside the
/// audio precisely so this list can exist without asking Yandex anything.
#[tauri::command]
async fn library(state: State<'_, AppState>) -> Result<Vec<CachedTrack>, String> {
    Ok(state.cache().await?.tracks())
}

/// Keeps these tracks on this device. Queued, one at a time, behind playback.
#[tauri::command]
async fn download(tracks: Vec<TrackRef>, state: State<'_, AppState>) -> Result<(), String> {
    if tracks.is_empty() {
        return Err("нечего скачивать".to_string());
    }
    send(&state, Command::Download { tracks }).await
}

#[tauri::command]
async fn cancel_downloads(state: State<'_, AppState>) -> Result<(), String> {
    send(&state, Command::CancelDownloads).await
}

/// Deletes a track from this device.
///
/// Goes through the engine while connected, because deleting also drops the track
/// from the download queue and withdraws it from what the room is offered. With no
/// session there is neither, so the cache is edited directly — otherwise the
/// library panel would be read-only until you connect.
#[tauri::command]
async fn forget(id: String, state: State<'_, AppState>) -> Result<(), String> {
    let connected = state.session.lock().await.is_some();
    if connected {
        return send(&state, Command::Forget { track_id: id }).await;
    }

    let cache = state.cache().await?;
    tokio::task::spawn_blocking(move || cache.remove(&id))
        .await
        .map_err(fail)?
        .map_err(fail)
}

async fn send(state: &State<'_, AppState>, command: Command) -> Result<(), String> {
    match state.session.lock().await.as_ref() {
        Some(session) => {
            session.handle().send(command);
            Ok(())
        }
        None => Err("нет подключения к релею".to_string()),
    }
}

/// Errors cross the IPC boundary as text, so keep the whole `anyhow` chain.
fn fail<E: std::fmt::Display>(err: E) -> String {
    format!("{err:#}")
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .init();

    let (config, config_path) = match Config::load(None) {
        Ok((config, path)) => (config, path.display().to_string()),
        Err(err) => {
            // A broken config must not stop the window from opening: the page
            // shows the problem and where to fix it.
            tracing::error!("{err:#}");
            (Config::default(), String::new())
        }
    };

    tauri::Builder::default()
        .setup(move |app| {
            app.manage(AppState {
                config,
                config_path,
                api: Mutex::new(None),
                cache: Mutex::new(None),
                session: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings,
            connect,
            disconnect,
            snapshot,
            search,
            play_source,
            queue_tracks,
            control,
            library,
            download,
            cancel_downloads,
            forget
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно");
}
