//! Desktop front end.
//!
//! A thin bridge: every command forwards to [`ymsync::engine`], and engine
//! snapshots are pushed to the page as `snapshot` events. No playback or sync
//! logic lives here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use ymsync::api::{self, Track, YandexMusic};
use ymsync::cache::{Cache, CachedTrack};
use ymsync::config::Config;
use ymsync::discover::{self, FoundRoom};
use ymsync::engine::{Command, Snapshot};
use ymsync::import;
use ymsync::likes::Likes;
use ymsync::player::Player;
use ymsync::session::{self, Session};
use ymsync_proto::TrackRef;

/// The session slot, shared with the task that watches the engine.
///
/// Shared rather than owned by the state on purpose: when the room closes, the
/// engine ends on its own, and *something* has to empty this slot. Before, only
/// the page was told, so the slot stayed full and every later connect answered
/// «уже подключено» for the rest of the run.
type SessionSlot = Arc<Mutex<Option<Session>>>;

struct AppState {
    /// What the window is working with. Written back to `config_path` whenever a
    /// connection succeeds, so a room is set up once rather than at every launch.
    config: std::sync::Mutex<Config>,
    config_path: PathBuf,
    /// Built on first use, then reused; searching does not need a connection.
    api: Mutex<Option<Arc<YandexMusic>>>,
    /// Opened on first use and kept: the window lists the offline library before
    /// anything connects, and the engine then plays out of that same cache.
    cache: Mutex<Option<Arc<Cache>>>,
    /// The account's «Мне нравится», read off the disk for the same reason: the
    /// list and the hearts drawn from it are on screen before anything connects.
    likes: Mutex<Option<Arc<Likes>>>,
    /// The engine plus whatever it brought up — a hosted relay, the file server
    /// for cached tracks. Held whole so that disconnecting closes those ports.
    session: SessionSlot,
}

impl AppState {
    fn config(&self) -> Config {
        self.config.lock().expect("config mutex").clone()
    }

    /// The API client, if this machine has a token at all.
    ///
    /// Connecting does not need one: a room that plays what is on this disk works
    /// without an account, and the actions that do need Yandex report it.
    async fn api_if_any(&self) -> Result<Option<Arc<YandexMusic>>, String> {
        if self.config().yandex_token.trim().is_empty() {
            return Ok(None);
        }
        self.api().await.map(Some)
    }

    /// The API client, created on demand so the window can open even when no
    /// token is configured yet.
    async fn api(&self) -> Result<Arc<YandexMusic>, String> {
        let mut slot = self.api.lock().await;
        if let Some(api) = slot.as_ref() {
            return Ok(Arc::clone(api));
        }
        let config = self.config();
        let token = config.require_yandex_token().map_err(fail)?;
        let api = Arc::new(YandexMusic::new(token).map_err(fail)?);
        *slot = Some(Arc::clone(&api));
        Ok(api)
    }

    async fn cache(&self) -> Result<Arc<Cache>, String> {
        let mut slot = self.cache.lock().await;
        if let Some(cache) = slot.as_ref() {
            return Ok(Arc::clone(cache));
        }
        let cache = session::open_cache(&self.config()).map_err(fail)?;
        *slot = Some(Arc::clone(&cache));
        Ok(cache)
    }

    /// The stored «Мне нравится». Lives beside the downloads, so it needs the
    /// cache directory and nothing else — no token, no connection.
    async fn likes(&self) -> Result<Arc<Likes>, String> {
        let cache = self.cache().await?;
        let mut slot = self.likes.lock().await;
        if let Some(likes) = slot.as_ref() {
            return Ok(Arc::clone(likes));
        }
        let likes = session::open_likes(&cache);
        *slot = Some(Arc::clone(&likes));
        Ok(likes)
    }

    /// Remembers the settings a successful connection was made with.
    ///
    /// Reported rather than swallowed on failure: silently not saving a password
    /// looks exactly like saving it until the next launch asks for it again.
    fn remember(&self, config: Config) -> Result<(), String> {
        let path = self.config_path.clone();
        *self.config.lock().expect("config mutex") = config.clone();
        config.save(&path).map_err(fail)
    }
}

/// Anything the page needs before connecting.
#[derive(Serialize)]
struct Settings {
    relay: String,
    room: String,
    /// The room's password. Local to this machine and shown as typed: it is what
    /// you read out to whoever joins from the sofa.
    password: String,
    volume: f32,
    has_yandex_token: bool,
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
    let cfg = state.config();
    Settings {
        relay: cfg.relay.clone(),
        room: cfg.room.clone(),
        password: cfg.room_token.clone(),
        volume: cfg.volume,
        has_yandex_token: !cfg.yandex_token.trim().is_empty(),
        config_path: state.config_path.display().to_string(),
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
/// The two buttons in the window are this one command: «Подключиться» dials
/// `relay`, «Хостить» binds a relay here and dials that instead. Hosting has to
/// be decided at this point because the relay is bound before the engine connects
/// to it.
///
/// Everything that worked is written back to `config.toml`, so the name and the
/// password of a room are typed once.
#[tauri::command]
async fn connect(
    app: AppHandle,
    host: bool,
    advertise: String,
    relay: String,
    room: String,
    password: String,
    state: State<'_, AppState>,
) -> Result<Snapshot, String> {
    let mut slot = state.session.lock().await;
    if slot.is_some() {
        return Err("уже подключено".to_string());
    }

    let room = room.trim().to_string();
    let password = password.trim().to_string();
    let relay = relay.trim().to_string();
    if room.is_empty() {
        return Err("укажите название комнаты".to_string());
    }
    if password.is_empty() {
        return Err("укажите пароль комнаты — он должен совпадать у всех участников".to_string());
    }
    if !host && relay.is_empty() {
        return Err("укажите адрес комнаты или найдите её кнопкой «Комнаты»".to_string());
    }

    let mut cfg = state.config();
    cfg.room = room;
    cfg.room_token = password;
    cfg.host.enabled = host;
    if host {
        // Empty means «определи сам» rather than «оставь как было»: the field is
        // filled from the file, so what is in it now is what the user meant.
        cfg.host.advertise = advertise.trim().to_string();
    } else {
        cfg.relay = relay;
    }

    let api = state.api_if_any().await?;
    let cache = state.cache().await?;
    let likes = state.likes().await?;
    let volume = cfg.volume;
    // Opening the audio device blocks briefly.
    let player = tokio::task::spawn_blocking(move || Player::new(volume))
        .await
        .map_err(fail)?
        .map_err(fail)?;

    // Hosting the room and serving cached tracks are both set up here, in the
    // order the engine needs them.
    let session = session::start(&cfg, api, Arc::new(player), cache, likes)
        .await
        .map_err(fail)?;
    let snapshot = session.handle().snapshot();

    if let Err(err) = state.remember(cfg) {
        let _ = app.emit("notice", format!("не удалось сохранить настройки: {err}"));
    }

    // Push every engine state change to the page, and hand the session back when
    // the engine stops — which is what happens when the room closes.
    let mut snapshots = session.handle().subscribe();
    let slot_for_watch = Arc::clone(&state.session);
    tauri::async_runtime::spawn(async move {
        while snapshots.changed().await.is_ok() {
            let snapshot = snapshots.borrow_and_update().clone();
            if app.emit("snapshot", snapshot).is_err() {
                break;
            }
        }
        // The engine is already gone; this closes the ports it brought up — the
        // hosted relay, the file server — so the next connect can bind them again.
        if let Some(session) = slot_for_watch.lock().await.take() {
            let _ = session.shutdown().await;
        }
        let _ = app.emit("closed", ());
    });

    *slot = Some(session);
    Ok(snapshot)
}

/// Saves the Yandex token typed into the window, checking it first.
///
/// Yandex is asked before anything is written, so a wrong token is caught while it
/// is still on screen instead of surfacing as a puzzling failure on connect. Only
/// an outright refusal counts against the token: when Yandex cannot be reached the
/// check is skipped and the token stored with a warning, because a machine with no
/// internet has to be configurable too — and an unreachable server says nothing
/// about whether the token is any good.
///
/// Returns what to tell the user: whose account it is, and whether it has Плюс.
#[tauri::command]
async fn save_token(token: String, state: State<'_, AppState>) -> Result<String, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("впишите токен — где его взять, написано в README".to_string());
    }

    let api = Arc::new(YandexMusic::new(&token).map_err(fail)?);
    let verdict = match api.account_status().await {
        Ok(status) => {
            let who = if status.account.display_name.is_empty() {
                status.account.login.clone()
            } else {
                status.account.display_name.clone()
            };
            let has_plus = status.plus.as_ref().is_some_and(|plus| plus.has_plus);
            if has_plus {
                format!("токен принят: {who}")
            } else {
                format!("токен принят: {who} — но у аккаунта нет Плюса, полные треки недоступны")
            }
        }
        // Yandex answered, and the answer was no: keep the field as typed.
        Err(err) if err.downcast_ref::<api::TokenRejected>().is_some() => {
            return Err(fail(err));
        }
        Err(err) => format!("токен сохранён, но проверить не удалось: {err:#}"),
    };

    let mut cfg = state.config();
    cfg.yandex_token = token;
    state.remember(cfg)?;
    // Reuse the client just built, and replace an older one: the window may have
    // been running with a token that has since expired.
    *state.api.lock().await = Some(api);
    Ok(verdict)
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

/// Asks the local network which rooms are out there.
///
/// Needs no connection and no tokens: this is what you press *before* knowing
/// where to connect. Answering rooms name themselves and count their listeners —
/// getting in still needs the room's secret.
#[tauri::command]
async fn find_rooms(wait: u64) -> Result<Vec<FoundRoom>, String> {
    let wait = Duration::from_millis(wait.clamp(100, 5_000));
    discover::find_rooms(wait).await.map_err(fail)
}

/// What to offer while the listener is still typing.
///
/// Yandex's own suggest endpoint, which is what its apps draw their dropdown
/// from. Cheap enough for a keystroke, unlike a full search — and it needs no
/// room, only a token.
#[tauri::command]
async fn suggest(part: String, state: State<'_, AppState>) -> Result<api::Suggest, String> {
    let part = part.trim().to_string();
    if part.is_empty() {
        return Ok(api::Suggest::default());
    }
    let api = state.api().await?;
    api.suggest(&part).await.map_err(fail)
}

/// Adds local audio files to this device's downloads.
///
/// Opens the system's file picker and imports whatever comes back: the tracks
/// then behave like anything else downloaded here — they play with no internet,
/// and the room's other devices can pull them over the local network.
///
/// Note that this cannot read what the Yandex Music app itself downloaded: that
/// app stores its offline tracks encrypted.
#[tauri::command]
async fn import_tracks(state: State<'_, AppState>) -> Result<String, String> {
    let cache = state.cache().await?;

    // The dialog is modal and blocking, so it does not belong on the async
    // runtime's threads.
    let picked = tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("Выберите музыку")
            .add_filter("Аудио", ymsync::import::AUDIO_EXTENSIONS)
            .pick_files()
    })
    .await
    .map_err(fail)?;

    let Some(files) = picked else {
        // The picker was closed. Nothing to report and nothing went wrong.
        return Ok(String::new());
    };

    let report = tokio::task::spawn_blocking(move || {
        let mut added = 0usize;
        let mut known = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for file in files {
            match import::from_path(&cache, &file) {
                Ok(imported) if imported.already_there => known += 1,
                Ok(_) => added += 1,
                Err(err) => failed.push(format!("{err:#}")),
            }
        }
        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("добавлено: {added}"));
        }
        if known > 0 {
            parts.push(format!("уже было: {known}"));
        }
        if !failed.is_empty() {
            parts.push(format!("не вышло: {} ({})", failed.len(), failed.join("; ")));
        }
        if parts.is_empty() {
            "ничего не выбрано".to_string()
        } else {
            parts.join(" · ")
        }
    })
    .await
    .map_err(fail)?;

    Ok(report)
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

/// This account's «Мне нравится», as last read.
///
/// Off the disk, like the offline library: no connection, no internet, and the
/// hearts on every list are drawn from this.
#[tauri::command]
async fn likes(state: State<'_, AppState>) -> Result<Vec<TrackRef>, String> {
    let likes = state.likes().await?;
    Ok(likes.tracks().as_ref().clone())
}

/// Re-reads «Мне нравится» from Yandex. Returns how many there are.
#[tauri::command]
async fn refresh_likes(state: State<'_, AppState>) -> Result<usize, String> {
    let api = state.api().await?;
    state.likes().await?.refresh(&api).await.map_err(fail)
}

/// Puts a track into «Мне нравится», or takes it out.
///
/// Yandex first, then the stored list — so a refusal leaves the heart where it
/// was rather than lighting it up locally and lying until the next refresh.
/// Likes belong to this account, not to the room: nobody else's app changes.
#[tauri::command]
async fn like(
    track: TrackRef,
    liked: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let api = state.api().await?;
    state
        .likes()
        .await?
        .set(&api, &track, liked)
        .await
        .map_err(fail)
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
        Ok(pair) => pair,
        Err(err) => {
            // A broken config must not stop the window from opening: the page
            // shows the problem and where to fix it.
            tracing::error!("{err:#}");
            (
                Config::default(),
                Config::default_path().unwrap_or_default(),
            )
        }
    };

    tauri::Builder::default()
        .setup(move |app| {
            app.manage(AppState {
                config: std::sync::Mutex::new(config),
                config_path,
                api: Mutex::new(None),
                cache: Mutex::new(None),
                likes: Mutex::new(None),
                session: SessionSlot::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings,
            connect,
            save_token,
            disconnect,
            snapshot,
            search,
            suggest,
            play_source,
            queue_tracks,
            control,
            library,
            likes,
            refresh_likes,
            like,
            download,
            cancel_downloads,
            forget,
            find_rooms,
            import_tracks
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно");
}
