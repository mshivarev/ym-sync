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
use ymsync::config::Config;
use ymsync::engine::{self, Command, Handle, Snapshot};
use ymsync::player::Player;
use ymsync_proto::TrackRef;

struct AppState {
    config: Config,
    config_path: String,
    /// Built on first use, then reused; searching does not need a connection.
    api: Mutex<Option<Arc<YandexMusic>>>,
    engine: Mutex<Option<Handle>>,
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
    }
}

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>) -> Result<Snapshot, String> {
    let mut slot = state.engine.lock().await;
    if slot.is_some() {
        return Err("уже подключено".to_string());
    }

    let api = state.api().await?;
    let volume = state.config.volume;
    // Opening the audio device blocks briefly.
    let player = tokio::task::spawn_blocking(move || Player::new(volume))
        .await
        .map_err(fail)?
        .map_err(fail)?;

    let handle = engine::spawn(&state.config, api, Arc::new(player))
        .await
        .map_err(fail)?;
    let snapshot = handle.snapshot();

    // Push every engine state change to the page.
    let mut snapshots = handle.subscribe();
    tauri::async_runtime::spawn(async move {
        while snapshots.changed().await.is_ok() {
            let snapshot = snapshots.borrow_and_update().clone();
            if app.emit("snapshot", snapshot).is_err() {
                break;
            }
        }
        let _ = app.emit("closed", ());
    });

    *slot = Some(handle);
    Ok(snapshot)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(handle) = state.engine.lock().await.take() {
        handle.send(Command::Shutdown);
        handle.join().await.map_err(fail)?;
    }
    Ok(())
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<Option<Snapshot>, String> {
    Ok(state.engine.lock().await.as_ref().map(Handle::snapshot))
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

async fn send(state: &State<'_, AppState>, command: Command) -> Result<(), String> {
    match state.engine.lock().await.as_ref() {
        Some(handle) => {
            handle.send(command);
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
                engine: Mutex::new(None),
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
            control
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно");
}
