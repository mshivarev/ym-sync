//! The sync engine: one loop that owns the player and the relay link.
//!
//! Every front end (CLI, Tauri, the Android JNI layer) drives the same engine:
//! commands go in through a channel, and state comes out as snapshots on a
//! `watch` channel. Nothing here reads stdin or prints, so the loop is reusable
//! and the UI never blocks on a download.
//!
//! Since protocol 3 there is only one kind of peer. The relay owns the queue and
//! the playhead, so this loop does exactly two things: it asks the relay for
//! changes, and it pulls the local player onto whatever the relay says the room
//! is doing. Nothing here decides anything about the room on its own — which is
//! what lets several people command it at once without fighting.
//!
//! The one asymmetry left is the station. The relay has no Yandex credentials,
//! so an endless station needs a peer to resolve batches and push them in; the
//! peer that switched the wave on becomes that feeder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use ymsync_proto::{
    Correction, Event, PeerShare, PlaybackState, SyncParams, TrackRef, decide, target_position_ms,
    trust_clock,
};

use crate::api::{Track, YandexMusic};
use crate::cache::{Cache, Insertion};
use crate::config::Config;
use crate::link::Link;
use crate::playback::{AudioSource, Playback};
use crate::share;

/// A snapshot whose seq is this far below the last one means the relay restarted
/// and began a fresh counter, rather than a frame arriving late.
const SEQ_RESTART_GAP: u64 = 64;

/// A load can fail for transient reasons — Yandex rate-limits concurrent stream
/// requests per account, and networks blip — so a track is only written off
/// after this many attempts.
const MAX_LOAD_ATTEMPTS: u8 = 3;

/// How long a track is left alone after a failure that may pass.
///
/// Long enough that a dead network is not hammered — every attempt costs the one
/// network slot — and short enough that opening a firewall or walking back into
/// Wi-Fi fixes the room without restarting anything.
const DEFER_AFTER: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Shortest gap between telling the room what this machine has cached.
///
/// Downloading an album finishes a track every few seconds, and each one changes
/// the list. Without a floor here every peer in the room would get a fresh copy
/// of the whole list that often, so changes are coalesced instead.
const ANNOUNCE_MIN_GAP: Duration = Duration::from_secs(2);

/// Quiet period after a hard seek.
///
/// A backend can report a playhead that trails the position it was just seeked
/// to — ExoPlayer does, by roughly the length of its audio pipeline. Without this
/// pause the engine would seek on every tick for ever, chasing an offset it
/// cannot remove, and the result is continuous stuttering rather than sync.
const SEEK_COOLDOWN: Duration = Duration::from_secs(2);

/// How long a notice stays in the snapshot.
///
/// Notices report events — a peer joining, a track skipped. Without an expiry
/// "участник подключился" would sit in every later snapshot and read as the
/// current state long after that peer left.
const NOTICE_TTL: Duration = Duration::from_secs(8);

/// How close to the end of the queue a station is topped up.
///
/// A station hands out a few tracks at a time, so the next batch has to be asked
/// for before the current one runs out — otherwise playback stops for as long as
/// the request takes.
const STATION_LEAD: usize = 2;

/// What a front end can ask the engine to do.
///
/// Everything except volume becomes a request to the relay: this peer does not
/// change the room by itself, it asks, and then follows the answer along with
/// everybody else. Volume never leaves the machine — it belongs to these
/// speakers, not to the room.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Replaces the room's queue and starts playing at `start`.
    SetQueue { tracks: Vec<TrackRef>, start: usize },
    /// Adds tracks to the end of the room's queue.
    ///
    /// Never interrupts what is playing — that is the whole point of a queue —
    /// but a queue that is empty or has run dry starts on the new tracks.
    Enqueue { tracks: Vec<TrackRef> },
    /// Follows an endless station, topping the room's queue up as it runs down.
    /// [`crate::api::WAVE_STATION`] is «Моя волна». This peer becomes the feeder.
    PlayStation { id: String, replace: bool },
    /// Stops feeding a station. What is already queued still plays.
    StopStation,
    PlayIndex(usize),
    TogglePause,
    SeekTo(u64),
    SeekBy(i64),
    Next,
    Prev,
    SetVolume(f32),
    /// Recalibrates this device's reporting lag while it plays, so the figure can
    /// be dialled in against the drift on screen instead of through a reconnect.
    SetPositionBias(i64),
    /// Keeps these tracks on this device, so they play with no network at all.
    ///
    /// Queued rather than started at once: one download at a time, behind
    /// whatever playback needs — see [`Engine::network_free`].
    Download { tracks: Vec<TrackRef> },
    /// Forgets everything still waiting to be downloaded. What is on disk stays.
    CancelDownloads,
    /// Deletes a downloaded track from this device.
    Forget { track_id: String },
    Shutdown,
}

/// Everything a UI needs to draw itself. Published after every loop iteration.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub connected: bool,
    pub peers: usize,
    pub queue: Vec<TrackRef>,
    /// Bumped on every queue change, so a front end can skip redrawing it.
    pub queue_revision: u64,
    pub index: usize,
    pub track: Option<TrackRef>,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
    /// A track is being fetched; the playhead is meaningless until it clears.
    pub loading: bool,
    pub volume: f32,
    /// The station feeding the room, if any.
    pub station: Option<String>,
    /// Whether *this* peer is the one feeding that station.
    pub feeding: bool,
    /// How far ahead (+) or behind (-) the room this peer is.
    pub drift_ms: Option<i64>,
    pub rtt_ms: Option<i64>,
    pub offset_ms: Option<i64>,
    /// Ids of the tracks on this device's disk.
    ///
    /// Behind an `Arc` because the whole list is cloned into every snapshot
    /// several times a second, and a well-used cache runs into the thousands.
    pub cached: Arc<Vec<String>>,
    /// Bumped whenever `cached` changes, so a front end can skip re-sending a
    /// list that has not moved.
    pub cache_revision: u64,
    pub cache_bytes: u64,
    /// 0 means no limit.
    pub cache_limit_bytes: u64,
    /// The track being downloaded for offline use, if any.
    pub downloading: Option<String>,
    /// How many more are waiting behind it.
    pub download_queue: usize,
    /// How many peers in the room are offering cached tracks over the local
    /// network.
    pub sharing_peers: usize,
    /// Ids the room can supply over the local network, whoever holds them.
    /// A front end uses this to show that a track will cost no internet.
    pub on_lan: Arc<Vec<String>>,
    /// The port this machine serves its own cache on, if it is sharing.
    pub share_port: Option<u16>,
    /// Set when this app is running the room's relay itself; the address others
    /// should connect to.
    pub hosting: Option<String>,
    /// Last thing worth telling the user about.
    pub notice: Option<String>,
}

/// Handle to a running engine. Dropping every clone of the command sender ends
/// the loop.
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Receiver<Snapshot>,
    cache: Arc<Cache>,
    task: JoinHandle<Result<()>>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }

    /// A receiver that wakes on every state change.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    /// This device's downloaded tracks.
    ///
    /// The way to fill a queue with no network: the metadata was stored next to
    /// the audio, so nothing has to be asked of Yandex. Reads the in-memory index
    /// only, which is why the Android front end may call it from its UI thread.
    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    /// Waits for the loop to finish — after [`Command::Shutdown`], or when the
    /// relay connection dies.
    pub async fn join(self) -> Result<()> {
        self.task.await.context("движок аварийно завершился")?
    }
}

/// What a front end hands the engine besides the configuration.
///
/// Assembled by [`crate::session::start`], which is what every front end actually
/// calls: hosting a room and serving cached tracks both have to be set up before
/// the engine connects, and doing that in one place keeps the three front ends
/// from each getting it subtly wrong.
pub struct Wiring {
    pub api: Arc<YandexMusic>,
    pub player: Arc<dyn Playback>,
    pub cache: Arc<Cache>,
    /// Where to connect. Not `cfg.relay` when this app hosts the room itself.
    pub relay_url: String,
    /// Port the local file server listens on, to be announced to the room.
    pub share_port: Option<u16>,
    /// The address to give other people, when this app is hosting.
    pub hosting: Option<String>,
}

/// Connects to the relay and starts the loop.
pub async fn spawn(cfg: &Config, wiring: Wiring) -> Result<Handle> {
    let Wiring {
        api,
        player,
        cache,
        relay_url,
        share_port,
        hosting,
    } = wiring;

    let room_token = cfg.require_room_token()?.to_string();
    let (link, events) = Link::connect(
        &relay_url,
        &cfg.room,
        &room_token,
        Duration::from_secs(cfg.sync.clock_probe_secs),
    )
    .await?;

    let peers = link.peers_at_join();
    let cached = Arc::new(cache.ids());
    let (snapshots, snapshot_rx) = watch::channel(Snapshot {
        connected: true,
        peers,
        queue: Vec::new(),
        queue_revision: 0,
        index: 0,
        track: None,
        position_ms: 0,
        duration_ms: 0,
        playing: false,
        loading: false,
        volume: player.volume(),
        station: None,
        feeding: false,
        drift_ms: None,
        rtt_ms: link.clock().rtt_ms(),
        offset_ms: link.clock().offset_ms(),
        cached: Arc::clone(&cached),
        cache_revision: 0,
        cache_bytes: cache.total_bytes(),
        cache_limit_bytes: cache.limit_bytes(),
        downloading: None,
        download_queue: 0,
        sharing_peers: 0,
        on_lan: Arc::new(Vec::new()),
        share_port,
        hosting: hosting.clone(),
        notice: None,
    });

    let engine = Engine {
        api,
        http: share::client()?,
        cache: Arc::clone(&cache),
        auto_cache: cfg.cache.auto,
        room_token,
        player,
        link,
        params: cfg.sync.params(),
        queue: Vec::new(),
        revision: 0,
        station: None,
        refilling: false,
        remote: None,
        unavailable: HashSet::new(),
        deferred: HashMap::new(),
        attempts: HashMap::new(),
        loading: None,
        prefetched: None,
        prefetching: None,
        downloads: VecDeque::new(),
        downloading: None,
        shares: Vec::new(),
        share_port,
        // True from the start: the room has not been told anything yet, and even
        // an empty cache is worth stating so a peer knows where it stands.
        share_dirty: true,
        last_announce: None,
        cached,
        cache_revision: 0,
        hosting,
        drift_ms: None,
        position_bias_ms: cfg.sync.position_bias_ms,
        last_seek: None,
        notice: None,
        notice_seen: None,
        notice_since: None,
        peers,
        connected: true,
        snapshots,
    };

    let tick = Duration::from_millis(cfg.sync.correction_interval_ms);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(engine.run(command_rx, events, tick));

    Ok(Handle {
        commands,
        snapshots: snapshot_rx,
        cache,
        task,
    })
}

/// Result of a background track fetch.
enum Internal {
    Loaded {
        track_id: String,
        error: Option<LoadFailure>,
        /// Where the audio came from, so the user can see when the room supplied
        /// it instead of the internet.
        origin: Option<Origin>,
        /// Whether it landed on disk, which is what moves the shared list.
        stored: bool,
    },
    /// A transient failure earned another try.
    Retry {
        track_id: String,
    },
    /// What a station handed out, or why it handed out nothing.
    Station {
        tracks: Vec<TrackRef>,
        error: Option<String>,
    },
    /// The next track, fetched before the room got to it. `None` means the fetch
    /// failed; nothing is reported, since the ordinary load will try again and
    /// speak up then.
    Prefetched {
        track_id: String,
        ready: Option<(TrackRef, AudioSource, Origin)>,
        stored: bool,
    },
    /// One track finished downloading for offline use.
    Downloaded {
        track_id: String,
        label: String,
        origin: Option<Origin>,
        error: Option<String>,
        /// The cache had to ignore its limit because everything in it was
        /// downloaded deliberately.
        over_limit: bool,
    },
}

/// Where a track's audio came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Already on this disk. No network at all — this is what offline play is.
    Cache,
    /// Another player in the room, over the local network.
    Peer,
    Yandex,
}

/// A track fetched ahead of time, waiting for the room to reach it.
struct Prefetched {
    /// The queue's id for it, which is what the arriving track is matched on.
    track_id: String,
    /// As the API resolved it, which is what gets staged.
    track: TrackRef,
    source: AudioSource,
    /// Kept so the notice can still say where the audio came from once the room
    /// reaches this track.
    origin: Origin,
}

/// An endless station this peer is feeding into the room.
struct Station {
    id: String,
    /// The last track it handed out, so the next request continues from there
    /// instead of replaying the same batch.
    after: Option<String>,
    /// Whether the relay has confirmed this peer as the feeder. Until it has, we
    /// resolve batches but may still lose the claim to someone quicker.
    claimed: bool,
    /// The first batch replaces the room's queue rather than joining it.
    replace_first: bool,
}

/// Why a track did not start.
///
/// The distinction decides what happens next, so it is worth keeping: one of
/// these is an answer that will not change, and the other is a road that may
/// reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadFailure {
    /// Yandex says this account may not play the track, and nobody in the room
    /// offered it either. Nothing to retry.
    NotLicensed(String),
    /// Everything else: no internet, a peer that announced the track but cannot
    /// be reached, a player that refused the source. All of these may pass, so
    /// the track is put aside for a while rather than written off.
    Temporary(String),
}

impl LoadFailure {
    fn message(&self) -> &str {
        match self {
            LoadFailure::NotLicensed(text) | LoadFailure::Temporary(text) => text,
        }
    }

    /// The same failure with the track's name in front of it, for the notice line.
    fn labelled(self, label: &str) -> Self {
        match self {
            LoadFailure::NotLicensed(text) => {
                LoadFailure::NotLicensed(format!("{label}: {text}"))
            }
            LoadFailure::Temporary(text) => LoadFailure::Temporary(format!("{label}: {text}")),
        }
    }

    /// Classifies a refusal from Yandex.
    ///
    /// Whether an account may play a track only settles the matter when nobody in
    /// the room has it: a neighbour's copy plays regardless of licensing, so as
    /// long as somebody offered it, the road may reopen and this is temporary.
    fn from_yandex(err: &anyhow::Error, peer_error: Option<String>) -> Self {
        match peer_error {
            Some(peer) => LoadFailure::Temporary(format!(
                "не удалось забрать у участника ({peer}); Яндекс тоже не ответил ({err:#})"
            )),
            None if err.downcast_ref::<NotLicensed>().is_some() => {
                LoadFailure::NotLicensed(format!("{err:#}"))
            }
            None => LoadFailure::Temporary(format!("{err:#}")),
        }
    }
}

/// Yandex's answer that this account may not play a track.
///
/// A marker type rather than a message, so the difference between «нельзя» and
/// «не дозвонился» survives the trip up through the context layers.
#[derive(Debug)]
struct NotLicensed;

impl std::fmt::Display for NotLicensed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("недоступен на этом аккаунте")
    }
}

impl std::error::Error for NotLicensed {}

struct Engine {
    api: Arc<YandexMusic>,
    /// For pulling tracks off other players on the local network. Separate from
    /// the Yandex client because a peer answers at once or not at all.
    http: reqwest::Client,
    cache: Arc<Cache>,
    /// Keep every track that plays, not just the ones asked for by name.
    auto_cache: bool,
    /// Also the credential the file server checks, so a peer fetch can present it.
    room_token: String,
    player: Arc<dyn Playback>,
    link: Link,
    params: SyncParams,

    /// Mirror of the room's queue, as last sent by the relay.
    queue: Vec<TrackRef>,
    revision: u64,

    /// Set only while *this* peer feeds a station.
    station: Option<Station>,
    /// A request for more station tracks is in flight.
    refilling: bool,

    /// The room's state, as the relay last described it. This is the only
    /// authority on what should be playing.
    remote: Option<PlaybackState>,
    /// Tracks this account cannot play, so we stop retrying them.
    unavailable: HashSet<String>,
    /// Tracks put aside after a failure that may pass — no internet, or a peer
    /// that offered a track and could not be reached — and when to try again.
    ///
    /// Separate from [`Engine::unavailable`] on purpose: that one is a verdict
    /// about the account, this one is about the moment. Without it a network
    /// failure would either burn the track for the whole session or spin in a
    /// retry loop holding the single network slot.
    deferred: HashMap<String, std::time::Instant>,
    /// Failed attempts per track, cleared once one succeeds.
    attempts: HashMap<String, u8>,

    loading: Option<String>,
    /// The next track, already in hand. See [`Engine::prefetch_next`].
    prefetched: Option<Prefetched>,
    /// Which track a prefetch is in flight for.
    prefetching: Option<String>,

    /// Tracks the user asked to keep on this device, oldest request first.
    downloads: VecDeque<TrackRef>,
    /// The download in flight.
    downloading: Option<String>,

    /// Who in the room is offering cached tracks, as the relay last described it.
    shares: Vec<PeerShare>,
    /// The port this machine serves its own cache on.
    share_port: Option<u16>,
    /// The cache has changed and the room has not been told yet.
    share_dirty: bool,
    last_announce: Option<std::time::Instant>,
    /// Cached ids, as last published in a snapshot.
    cached: Arc<Vec<String>>,
    cache_revision: u64,
    /// Set when this app runs the room's relay: the address to hand out.
    hosting: Option<String>,

    drift_ms: Option<i64>,
    /// Cancels a backend's constant reporting lag; see [`crate::config::SyncConfig`].
    position_bias_ms: i64,
    /// When the last hard seek happened, for [`SEEK_COOLDOWN`].
    last_seek: Option<std::time::Instant>,
    notice: Option<String>,
    /// The notice as of the previous publish, so a replacement restarts its life.
    notice_seen: Option<String>,
    notice_since: Option<std::time::Instant>,
    peers: usize,
    connected: bool,

    snapshots: watch::Sender<Snapshot>,
}

impl Engine {
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<Event>,
        tick: Duration,
    ) -> Result<()> {
        let (internal, mut internal_rx) = mpsc::unbounded_channel::<Internal>();

        let mut tick = tokio::time::interval(tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    // Every handle was dropped.
                    None => break,
                    Some(Command::Shutdown) => break,
                    Some(command) => self.handle_command(command, &internal),
                },

                message = internal_rx.recv() => {
                    if let Some(message) = message {
                        self.handle_internal(message, &internal);
                    }
                }

                event = events.recv() => match event {
                    None => {
                        self.connected = false;
                        self.notice = Some("соединение с релеем потеряно".to_string());
                        self.publish();
                        bail!("соединение с релеем потеряно");
                    }
                    Some(event) => self.handle_event(event, &internal),
                },

                _ = tick.tick() => self.tick(&internal),
            }

            self.publish();
        }

        self.player.pause();
        self.link.say_goodbye();
        self.connected = false;
        self.publish();
        Ok(())
    }

    /// Sends a request to the relay, noting a dead link.
    fn ask(&mut self, command: ymsync_proto::Command) {
        if !self.link.send_command(command) {
            self.connected = false;
        }
    }

    /// Where the room's playhead is right now, in track time. `None` when the
    /// clock is not established yet.
    fn room_position_ms(&self) -> Option<i64> {
        let remote = self.remote.as_ref()?;
        let now = self.link.clock().now_server_ms()?;
        Some(target_position_ms(remote, now))
    }

    fn handle_command(&mut self, command: Command, internal: &mpsc::UnboundedSender<Internal>) {
        match command {
            Command::SetQueue { tracks, start } => {
                if tracks.is_empty() {
                    self.notice = Some("пустая очередь".to_string());
                    return;
                }
                // Asking for particular tracks ends any wave we were feeding;
                // the relay drops the claim on its side for the same reason.
                self.station = None;
                self.ask(ymsync_proto::Command::SetQueue { tracks, start });
            }

            Command::Enqueue { tracks } => {
                if tracks.is_empty() {
                    self.notice = Some("нечего добавить в очередь".to_string());
                    return;
                }
                self.ask(ymsync_proto::Command::Enqueue { tracks });
            }

            Command::PlayStation { id, replace } => {
                self.notice = Some("настраиваю волну…".to_string());
                self.station = Some(Station {
                    id,
                    after: None,
                    claimed: false,
                    replace_first: replace,
                });
                self.refilling = false;
                self.request_station_tracks(internal);
            }

            Command::StopStation => {
                if self.station.take().is_some() {
                    self.ask(ymsync_proto::Command::SetStation { id: None });
                }
            }

            Command::PlayIndex(index) => self.ask(ymsync_proto::Command::PlayIndex { index }),
            Command::Next => self.ask(ymsync_proto::Command::Next),
            Command::Prev => self.ask(ymsync_proto::Command::Prev),

            Command::TogglePause => {
                // Derived from the room, not from this player: a toggle computed
                // locally would flip the wrong way on a peer that is still
                // loading, and two peers toggling at once would cancel out.
                let playing = self.remote.as_ref().is_some_and(|state| state.playing);
                self.ask(if playing {
                    ymsync_proto::Command::Pause
                } else {
                    ymsync_proto::Command::Resume
                });
            }

            Command::SeekTo(ms) => self.ask(ymsync_proto::Command::Seek { position_ms: ms }),

            Command::SeekBy(delta_ms) => {
                // Relative to where the *room* is, so that two peers nudging at
                // once both move from the same place.
                let Some(from) = self.room_position_ms() else {
                    self.notice = Some("часы ещё не сверены — подождите секунду".to_string());
                    return;
                };
                let target = (from + delta_ms).max(0) as u64;
                self.ask(ymsync_proto::Command::Seek {
                    position_ms: target,
                });
            }

            // The only commands that never leave this machine: both describe this
            // device, not the room.
            Command::SetVolume(volume) => self.player.set_volume(volume),
            Command::SetPositionBias(ms) => {
                self.position_bias_ms = ms;
                // The old figure produced the drift on screen; drop it so the
                // next tick reports against the new one rather than the stale
                // reading the user is trying to correct.
                self.drift_ms = None;
                self.notice = Some(format!("поправка позиции: {ms} мс"));
            }

            Command::Download { tracks } => {
                let mut queued = 0;
                let mut already = 0;
                for track in tracks {
                    if self.cache.has(&track.track_id) {
                        already += 1;
                        continue;
                    }
                    let known = self.downloading.as_deref() == Some(track.track_id.as_str())
                        || self.downloads.iter().any(|t| t.track_id == track.track_id);
                    if known {
                        continue;
                    }
                    self.downloads.push_back(track);
                    queued += 1;
                }
                self.notice = Some(match (queued, already) {
                    (0, 0) => "нечего скачивать".to_string(),
                    (0, _) => "всё это уже скачано".to_string(),
                    (1, _) => "скачиваю 1 трек".to_string(),
                    (n, 0) => format!("скачиваю {n} треков"),
                    (n, already) => format!("скачиваю {n} треков, {already} уже есть"),
                });
            }

            Command::CancelDownloads => {
                let dropped = self.downloads.len();
                self.downloads.clear();
                // The one in flight is left to finish: it is nearly free by now,
                // and cancelling a request mid-body gains nothing.
                self.notice = Some(match dropped {
                    0 => "очередь скачивания пуста".to_string(),
                    n => format!("отменил скачивание {n} треков"),
                });
            }

            Command::Forget { track_id } => {
                self.downloads.retain(|t| t.track_id != track_id);
                match self.cache.remove(&track_id) {
                    Ok(()) => {
                        self.refresh_cache_view();
                        self.notice = Some("удалил трек с устройства".to_string());
                    }
                    Err(err) => self.notice = Some(format!("не удалось удалить: {err:#}")),
                }
            }

            // Handled by the loop.
            Command::Shutdown => {}
        }
    }

    /// Asks the station for its next tracks, unless a request is already out.
    fn request_station_tracks(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        let Some(station) = self.station.as_ref() else {
            return;
        };
        if self.refilling {
            return;
        }
        self.refilling = true;

        let api = Arc::clone(&self.api);
        let internal = internal.clone();
        let id = station.id.clone();
        let after = station.after.clone();

        tokio::spawn(async move {
            let message = match api.station_tracks(&id, after.as_deref()).await {
                Ok(tracks) => Internal::Station {
                    tracks: tracks.iter().map(Track::to_track_ref).collect(),
                    error: None,
                },
                Err(err) => Internal::Station {
                    tracks: Vec::new(),
                    error: Some(format!("{err:#}")),
                },
            };
            let _ = internal.send(message);
        });
    }

    /// Fetches a track in the background. The playhead is not touched until the
    /// download lands, so the loop keeps answering commands meanwhile.
    fn start_load(&mut self, track: &TrackRef, internal: &mpsc::UnboundedSender<Internal>) {
        self.loading = Some(track.track_id.clone());
        self.drift_ms = None;

        let player = Arc::clone(&self.player);
        let internal_tx = internal.clone();
        let track_id = track.track_id.clone();
        let label = track.to_string();

        // Already in hand from the prefetch: no API calls, no download, just the
        // decoder. This is what makes a track change quiet.
        if let Some(ready) = self
            .prefetched
            .take_if(|ready| ready.track_id == track.track_id)
        {
            let Prefetched {
                track,
                source,
                origin,
                ..
            } = ready;
            tokio::spawn(async move {
                // A player that refuses a source we already hold is a local
                // problem, not a verdict on the track: worth another go.
                let error = stage_track(&player, track, source)
                    .await
                    .err()
                    .map(|err| LoadFailure::Temporary(format!("{label}: {err:#}")));
                let _ = internal_tx.send(Internal::Loaded {
                    track_id,
                    error,
                    origin: Some(origin),
                    // Whatever it cost was accounted for when it was prefetched.
                    stored: false,
                });
            });
            return;
        }

        let fetch = self.fetch_for(&track.track_id, self.auto_cache, false);
        let want = track.clone();
        tokio::spawn(async move {
            let outcome = fetch_source(&fetch, &want).await;
            let (error, origin, stored) = match outcome {
                Err(failure) => (Some(failure.labelled(&label)), None, false),
                Ok(Fetched {
                    track,
                    source,
                    origin,
                    stored,
                }) => {
                    let error = stage_track(&player, track, source)
                        .await
                        .err()
                        .map(|err| LoadFailure::Temporary(format!("{label}: {err:#}")));
                    (error, Some(origin), stored)
                }
            };
            let _ = internal_tx.send(Internal::Loaded {
                track_id,
                error,
                origin,
                stored,
            });
        });
    }

    /// Whether the one network slot is free.
    ///
    /// Yandex answers HTTP 429 «Concurrency limit exceeded» when one account pulls
    /// two streams at once, so the current track, the next one and anything being
    /// downloaded for offline use all queue behind this single gate. Playback has
    /// the first claim on it: filling the offline library must never be the reason
    /// a track starts late.
    fn network_free(&self) -> bool {
        self.loading.is_none() && self.prefetching.is_none() && self.downloading.is_none()
    }

    /// Assembles what a background fetch needs, including who in the room can
    /// supply this track without going to the internet.
    fn fetch_for(&self, track_id: &str, store: bool, pinned: bool) -> Fetch {
        Fetch {
            api: Arc::clone(&self.api),
            http: self.http.clone(),
            cache: Arc::clone(&self.cache),
            player: Arc::clone(&self.player),
            room_token: self.room_token.clone(),
            peers: self.peers_with(track_id),
            store,
            pinned,
        }
    }

    /// Endpoints that say they hold this track.
    ///
    /// Our own entry is in the room's map too and is deliberately not filtered
    /// out: we only ever announce what is cached here, so an id that reached this
    /// point — having already missed the cache — cannot match it. In the small
    /// window after an eviction it can, and then our own file server answers 404
    /// and the fetch moves on, which costs one request on loopback.
    fn peers_with(&self, track_id: &str) -> Vec<String> {
        self.shares
            .iter()
            .filter(|share| share.tracks.iter().any(|id| id == track_id))
            .map(|share| share.endpoint.clone())
            .collect()
    }

    /// Whether this track is still in its cool-down after a failure that may pass.
    ///
    /// See [`Engine::deferred`]: this is what stops a dead network from being
    /// hammered without writing the track off for the session.
    fn set_aside(&self, track_id: &str) -> bool {
        self.deferred
            .get(track_id)
            .is_some_and(|until| std::time::Instant::now() < *until)
    }

    /// The queue entry after the one the room is on.
    fn next_track(&self) -> Option<TrackRef> {
        let index = self.remote.as_ref()?.index;
        self.queue.get(index + 1).cloned()
    }

    /// Fetches the next queue entry while the current one plays, so the change of
    /// track costs no network.
    ///
    /// Strictly one fetch at a time — see [`Engine::network_free`].
    fn prefetch_next(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        if !self.network_free() {
            return;
        }
        let Some(next) = self.next_track() else {
            return;
        };

        // Let go of a buffer the room has moved past, rather than holding a
        // track's worth of memory that nothing will ask for again.
        let current_id = self
            .remote
            .as_ref()
            .and_then(|state| state.track.as_ref())
            .map(|track| track.track_id.clone());
        if let Some(held) = self.prefetched.as_ref() {
            let wanted =
                held.track_id == next.track_id || Some(&held.track_id) == current_id.as_ref();
            if !wanted {
                self.prefetched = None;
            }
        }

        if self
            .prefetched
            .as_ref()
            .is_some_and(|held| held.track_id == next.track_id)
            || self.unavailable.contains(&next.track_id)
            || self.set_aside(&next.track_id)
        {
            return;
        }

        self.prefetching = Some(next.track_id.clone());
        let fetch = self.fetch_for(&next.track_id, self.auto_cache, false);
        let internal = internal.clone();
        let track_id = next.track_id.clone();

        tokio::spawn(async move {
            let fetched = fetch_source(&fetch, &next).await.ok();
            let stored = fetched.as_ref().is_some_and(|f| f.stored);
            let _ = internal.send(Internal::Prefetched {
                track_id,
                ready: fetched.map(|f| (f.track, f.source, f.origin)),
                stored,
            });
        });
    }

    /// Starts the next offline download, if the network is not needed for
    /// playback right now.
    fn pump_downloads(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        if !self.network_free() {
            return;
        }
        // Anything that arrived by another route in the meantime — the room played
        // it, or a neighbour's copy came through the ordinary load — is already
        // done.
        while self
            .downloads
            .front()
            .is_some_and(|track| self.cache.has(&track.track_id))
        {
            self.downloads.pop_front();
        }
        let Some(track) = self.downloads.pop_front() else {
            return;
        };

        self.downloading = Some(track.track_id.clone());
        let fetch = self.fetch_for(&track.track_id, true, true);
        let internal = internal.clone();
        let track_id = track.track_id.clone();
        let label = track.to_string();

        tokio::spawn(async move {
            let message = match download(&fetch, &track).await {
                Ok((origin, insertion)) => Internal::Downloaded {
                    track_id,
                    label,
                    origin: Some(origin),
                    error: None,
                    over_limit: insertion.over_limit,
                },
                Err(err) => Internal::Downloaded {
                    track_id,
                    label,
                    origin: None,
                    error: Some(format!("{err:#}")),
                    over_limit: false,
                },
            };
            let _ = internal.send(message);
        });
    }

    /// Re-reads what is cached, for the snapshot and for the room.
    ///
    /// Called only when the cache actually moved: it clones every id, and a
    /// well-used cache holds thousands.
    fn refresh_cache_view(&mut self) {
        self.cached = Arc::new(self.cache.ids());
        self.cache_revision += 1;
        self.share_dirty = true;
    }

    /// Tells the room what this machine can serve, at most every
    /// [`ANNOUNCE_MIN_GAP`].
    fn announce_share(&mut self) {
        if !self.share_dirty {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(last) = self.last_announce
            && now.duration_since(last) < ANNOUNCE_MIN_GAP
        {
            return;
        }

        // Sharing off means an empty offer rather than silence, so a peer that saw
        // an earlier offer knows it has been withdrawn.
        let tracks = match self.share_port {
            Some(_) => (*self.cached).clone(),
            None => Vec::new(),
        };
        if !self.link.announce_share(self.share_port, tracks) {
            self.connected = false;
            return;
        }
        self.share_dirty = false;
        self.last_announce = Some(now);
    }

    fn handle_internal(&mut self, message: Internal, internal: &mpsc::UnboundedSender<Internal>) {
        match message {
            Internal::Station { tracks, error } => {
                self.refilling = false;
                // The wave was switched off while the request was out.
                let Some(station) = self.station.as_mut() else {
                    return;
                };
                if let Some(error) = error {
                    // Retrying on a tick would hammer a broken endpoint four
                    // times a second, so the station stops here.
                    self.station = None;
                    self.notice = Some(format!("волна замолчала: {error}"));
                    self.ask(ymsync_proto::Command::SetStation { id: None });
                    return;
                }
                if tracks.is_empty() {
                    return;
                }

                station.after = tracks.last().map(|t| t.track_id.clone());
                let replace = std::mem::take(&mut station.replace_first);
                let claim = if station.claimed {
                    None
                } else {
                    Some(station.id.clone())
                };

                // Tracks first, then the claim: a replacing queue clears the
                // relay's station, so claiming before it would lose the claim.
                if replace {
                    self.ask(ymsync_proto::Command::SetQueue { tracks, start: 0 });
                } else {
                    self.ask(ymsync_proto::Command::Enqueue { tracks });
                }
                if let Some(id) = claim {
                    self.ask(ymsync_proto::Command::SetStation { id: Some(id) });
                }
            }

            Internal::Prefetched {
                track_id,
                ready,
                stored,
            } => {
                if self.prefetching.as_deref() != Some(track_id.as_str()) {
                    return;
                }
                self.prefetching = None;
                if stored {
                    self.refresh_cache_view();
                }
                // A failure stays quiet on purpose: this track may never be
                // reached, and the ordinary load will report it if it is.
                if let Some((track, source, origin)) = ready {
                    self.prefetched = Some(Prefetched {
                        track_id,
                        track,
                        source,
                        origin,
                    });
                }
            }

            Internal::Downloaded {
                track_id,
                label,
                origin,
                error,
                over_limit,
            } => {
                if self.downloading.as_deref() == Some(track_id.as_str()) {
                    self.downloading = None;
                }
                match error {
                    Some(error) => {
                        self.notice = Some(format!("не удалось скачать {label}: {error}"));
                    }
                    None => {
                        self.refresh_cache_view();
                        let left = self.downloads.len();
                        let source = match origin {
                            Some(Origin::Peer) => " (из локальной сети)",
                            _ => "",
                        };
                        self.notice = Some(if over_limit {
                            format!(
                                "скачал {label}{source}, но кеш уже больше лимита — \
                                 удалите что-нибудь или поднимите cache.limit_gb"
                            )
                        } else if left > 0 {
                            format!("скачал {label}{source}, осталось {left}")
                        } else {
                            format!("скачал {label}{source}")
                        });
                    }
                }
            }

            Internal::Retry { track_id } => {
                // Only if it is still what we want to be playing.
                if self.loading.is_none()
                    && let Some(track) = self.wanted_track()
                    && track.track_id == track_id
                {
                    self.start_load(&track, internal);
                }
            }

            Internal::Loaded {
                track_id,
                error,
                origin,
                stored,
            } => {
                // A newer request overtook this one.
                if self.loading.as_deref() != Some(track_id.as_str()) {
                    return;
                }
                self.loading = None;
                if stored {
                    self.refresh_cache_view();
                }

                if let Some(failure) = error {
                    let attempts = self.attempts.entry(track_id.clone()).or_insert(0);
                    *attempts += 1;
                    let attempt = *attempts;

                    if attempt < MAX_LOAD_ATTEMPTS {
                        self.notice = Some(format!(
                            "попытка {attempt} из {MAX_LOAD_ATTEMPTS} не удалась, повторю — {}",
                            failure.message()
                        ));
                        let internal = internal.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(RETRY_DELAY).await;
                            let _ = internal.send(Internal::Retry { track_id });
                        });
                        return;
                    }

                    match failure {
                        // Accounts differ in what they may play. This peer sits the
                        // track out in silence rather than skipping it for
                        // everyone: the others can hear it perfectly well, and the
                        // relay will move the room on when the track's time is up.
                        LoadFailure::NotLicensed(message) => {
                            self.unavailable.insert(track_id);
                            self.notice = Some(format!("пропускаю на этом аккаунте — {message}"));
                        }
                        // Not the track's fault and not the account's: no internet,
                        // or the participant who has it cannot be reached. Put
                        // aside for a while instead of for the session, so a
                        // firewall opened or Wi-Fi coming back fixes it without a
                        // restart.
                        LoadFailure::Temporary(message) => {
                            self.deferred
                                .insert(track_id.clone(), std::time::Instant::now() + DEFER_AFTER);
                            self.attempts.remove(&track_id);
                            self.notice = Some(format!(
                                "не удалось получить трек, попробую снова через {} с — {message}",
                                DEFER_AFTER.as_secs()
                            ));
                        }
                    }
                    self.player.pause();
                    return;
                }

                self.attempts.remove(&track_id);
                self.deferred.remove(&track_id);
                self.notice = match origin {
                    // Worth saying: it means this track cost nothing and would
                    // have played with the internet unplugged.
                    Some(Origin::Cache) => Some("играю с устройства".to_string()),
                    Some(Origin::Peer) => Some("взял у участника по локальной сети".to_string()),
                    _ => None,
                };
                // Records that this track played, which is what eviction sorts
                // by. Off the loop, since it writes a file.
                if matches!(origin, Some(Origin::Cache)) {
                    let cache = Arc::clone(&self.cache);
                    let id = track_id.clone();
                    tokio::task::spawn_blocking(move || cache.touch(&id));
                }
                // The next correction tick puts the playhead where the room is.
            }
        }
    }

    /// The track this engine should currently have staged: whatever the room is
    /// on.
    fn wanted_track(&self) -> Option<TrackRef> {
        self.remote.as_ref().and_then(|state| state.track.clone())
    }

    fn handle_event(&mut self, event: Event, internal: &mpsc::UnboundedSender<Internal>) {
        match event {
            Event::State(state) => {
                if !accept_seq(self.remote.as_ref().map(|s| s.seq), state.seq) {
                    return;
                }

                // A room with no station means someone replaced the queue
                // outright, which ends the wave — but only once our own claim
                // has been confirmed. Switching the wave on sends `SetQueue`
                // first and the claim second, and that `SetQueue` comes back
                // with `station: None`; reading our own echo as "the wave was
                // turned off" made the feeder forget it was feeding, so the
                // station stopped dead after its first batch.
                if state.station.is_none()
                    && self.station.as_ref().is_some_and(|station| station.claimed)
                {
                    self.station = None;
                    self.notice = Some("волна выключена".to_string());
                }

                if let Some(track) = state.track.clone() {
                    let staged = self.player.current_track_id();
                    let is_staged = staged.as_deref() == Some(track.track_id.as_str());
                    let is_loading = self.loading.as_deref() == Some(track.track_id.as_str());
                    if !is_staged
                        && !is_loading
                        && !self.unavailable.contains(&track.track_id)
                        && !self.set_aside(&track.track_id)
                    {
                        self.notice = Some(format!("загружаю {track}"));
                        self.start_load(&track, internal);
                    }
                }
                self.remote = Some(state);
            }

            Event::Queue { revision, tracks } => {
                // Taken as-is, with no monotonicity check of the kind `seq` gets
                // in `accept_seq`. That is safe for one reason only: the relay is
                // the sole author and sends `Queue` before the `State` that
                // refers to it, over one ordered connection. If a queue ever
                // gains a second author, this needs the same guard.
                self.revision = revision;
                self.queue = tracks;
            }

            Event::Peer { joined, peers } => {
                self.peers = peers;
                self.notice = Some(
                    if joined {
                        "участник подключился"
                    } else {
                        "участник отключился"
                    }
                    .to_string(),
                );
            }

            Event::Station { id, yours } => {
                if yours {
                    if let Some(station) = self.station.as_mut() {
                        station.claimed = true;
                    }
                } else if self.station.take().is_some() {
                    self.notice = Some(match id {
                        Some(_) => "волну в этой комнате уже ведёт другой участник".to_string(),
                        None => "волна выключена".to_string(),
                    });
                }
            }

            Event::Shares { peers } => {
                // A track that was being sat out because this account cannot play
                // it may now be reachable from somebody who can: a neighbour's
                // copy plays regardless of what this account is licensed for. So a
                // new map is a reason to try again.
                let newly_offered: Vec<String> = peers
                    .iter()
                    .flat_map(|share| share.tracks.iter())
                    .filter(|id| {
                        self.unavailable.contains(id.as_str())
                            || self.deferred.contains_key(id.as_str())
                    })
                    .cloned()
                    .collect();
                for id in newly_offered {
                    self.unavailable.remove(&id);
                    self.deferred.remove(&id);
                    self.attempts.remove(&id);
                }
                self.shares = peers;
            }

            Event::Error { code, message } => {
                self.notice = Some(format!("релей сообщил об ошибке ({code}): {message}"));
            }
        }
    }

    /// One pass: keep a station fed, keep the room told what we have, fetch the
    /// next track, download what was asked for, then pull the playhead onto the
    /// room's.
    fn tick(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        self.feed_station(internal);
        self.announce_share();
        self.prefetch_next(internal);
        // After the prefetch, deliberately: the next track the room will play
        // matters more than filling the offline library.
        self.pump_downloads(internal);
        self.correct();
    }

    /// Tops the room's queue up while this peer is feeding a station.
    ///
    /// Ahead of every playback guard on purpose: a station has to be refilled
    /// even while a track is loading, and especially once the queue has run out.
    fn feed_station(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        if self.station.is_none() {
            return;
        }
        let index = self.remote.as_ref().map_or(0, |state| state.index);
        if index + STATION_LEAD >= self.queue.len() {
            self.request_station_tracks(internal);
        }
    }

    /// Pulls the local playhead onto the room's.
    fn correct(&mut self) {
        // A download is in flight, so the sink still holds the previous track:
        // both the playhead and any drift figure would be meaningless.
        if self.loading.is_some() {
            self.drift_ms = None;
            return;
        }
        let Some(remote) = self.remote.clone() else {
            return;
        };
        let Some(track) = remote.track.as_ref() else {
            self.player.pause();
            return;
        };
        // Still fetching, or sitting this track out because this account cannot
        // play it.
        if self.player.current_track_id().as_deref() != Some(track.track_id.as_str()) {
            self.drift_ms = None;
            return;
        }
        if !trust_clock(self.link.clock().rtt_ms(), &self.params) {
            return;
        }
        let Some(now_server_ms) = self.link.clock().now_server_ms() else {
            return;
        };

        let target_ms = target_position_ms(&remote, now_server_ms);
        // Past the end: the room has finished this track and the relay is about
        // to move on, so stop rather than chase a position that does not exist.
        if track.duration_ms > 0 && target_ms >= track.duration_ms as i64 {
            self.player.pause();
            self.drift_ms = None;
            return;
        }

        // The backend may report its playhead with a constant lag; cancel it
        // rather than chase it, so drift reflects what is actually audible.
        let local_ms = self.player.position_ms() as i64 + self.position_bias_ms;
        self.drift_ms = Some(local_ms - target_ms);

        match decide(
            target_ms,
            local_ms,
            remote.playing,
            self.player.is_playing(),
            &self.params,
        ) {
            Correction::None => {}
            Correction::Pause => self.player.pause(),
            Correction::Seek { to_ms } => {
                // A resume always seeks; a plain correction waits out the
                // cooldown, so a playhead the backend reports with a constant lag
                // is not chased on every tick.
                if !cooling_down(self.last_seek, std::time::Instant::now()) {
                    self.seek_now(to_ms, "не удалось выровнять позицию");
                }
            }
            Correction::Resume { to_ms } => {
                self.seek_now(to_ms, "не удалось выставить позицию");
                self.player.play();
            }
        }
    }

    fn seek_now(&mut self, to_ms: u64, failure: &str) {
        self.last_seek = Some(std::time::Instant::now());
        // `to_ms` is where the audio should be; ask the player for the reading
        // that puts it there.
        let requested = requested_position(to_ms, self.position_bias_ms);
        if let Err(err) = self.player.seek_ms(requested) {
            self.notice = Some(format!("{failure}: {err:#}"));
        }
    }

    /// Drops a notice once it has been on screen long enough. Tracked here rather
    /// than at each assignment so every message gets the behaviour for free.
    fn expire_notice(&mut self) {
        if self.notice != self.notice_seen {
            self.notice_seen = self.notice.clone();
            self.notice_since = self.notice.as_ref().map(|_| std::time::Instant::now());
            return;
        }
        if let Some(since) = self.notice_since
            && since.elapsed() >= NOTICE_TTL
        {
            self.notice = None;
            self.notice_seen = None;
            self.notice_since = None;
        }
    }

    fn publish(&mut self) {
        self.expire_notice();

        let track = self.wanted_track();
        let duration_ms = track.as_ref().map_or(0, |t| t.duration_ms);
        // A drained sink can report slightly past the end; never show more than
        // the track actually has.
        let position_ms = match duration_ms {
            0 => self.player.position_ms(),
            duration => self.player.position_ms().min(duration),
        };

        let _ = self.snapshots.send(Snapshot {
            connected: self.connected,
            peers: self.peers,
            queue: self.queue.clone(),
            queue_revision: self.revision,
            index: self.remote.as_ref().map_or(0, |state| state.index),
            track,
            position_ms,
            duration_ms,
            playing: self.player.is_playing(),
            loading: self.loading.is_some(),
            volume: self.player.volume(),
            station: self
                .remote
                .as_ref()
                .and_then(|state| state.station.clone()),
            // Only once the relay has confirmed the claim: between asking and
            // being answered the station may still go to a quicker peer, and the
            // front ends hang their "switch the wave off" control on this.
            feeding: self.station.as_ref().is_some_and(|station| station.claimed),
            drift_ms: self.drift_ms,
            rtt_ms: self.link.clock().rtt_ms(),
            offset_ms: self.link.clock().offset_ms(),
            cached: Arc::clone(&self.cached),
            cache_revision: self.cache_revision,
            cache_bytes: self.cache.total_bytes(),
            cache_limit_bytes: self.cache.limit_bytes(),
            downloading: self.downloading.clone(),
            download_queue: self.downloads.len(),
            sharing_peers: self
                .shares
                .iter()
                .filter(|share| !share.tracks.is_empty())
                .count(),
            on_lan: Arc::new(self.lan_ids()),
            share_port: self.share_port,
            hosting: self.hosting.clone(),
            notice: self.notice.clone(),
        });
    }

    /// Every track id the room can supply without the internet, deduplicated.
    fn lan_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .shares
            .iter()
            .flat_map(|share| share.tracks.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// Everything a background fetch needs, in a form that can be moved into a
/// spawned task without borrowing the engine.
#[derive(Clone)]
struct Fetch {
    api: Arc<YandexMusic>,
    http: reqwest::Client,
    cache: Arc<Cache>,
    player: Arc<dyn Playback>,
    /// The room's secret, which the peers' file servers check.
    room_token: String,
    /// Endpoints that say they hold this track.
    peers: Vec<String>,
    /// Keep the bytes on disk even when they came from Yandex.
    store: bool,
    /// Mark what is stored as a deliberate download, safe from eviction.
    pinned: bool,
}

/// A track ready to be handed to the player.
struct Fetched {
    track: TrackRef,
    source: AudioSource,
    origin: Origin,
    /// Whether this fetch put something on disk.
    stored: bool,
}

/// Resolves a track and produces whatever this backend wants to be handed.
///
/// The order of preference, and why it is that order:
///
/// 1. **This device's cache.** No network at all, which is exactly what makes
///    offline listening work — and note that the metadata comes from the sidecar,
///    so not even a lookup is needed.
/// 2. **A peer on the local network.** Faster than the internet, spends no Yandex
///    request (and so cannot trip the concurrency limit), and plays tracks this
///    account is not licensed for, because the bytes come from an account that is.
/// 3. **Yandex.**
///
/// `want` is the track as the room's queue describes it, which is why step 2 needs
/// no lookup either: a peer's file server carries audio, not metadata.
async fn fetch_source(fetch: &Fetch, want: &TrackRef) -> Result<Fetched, LoadFailure> {
    let id = want.track_id.as_str();

    if let Some(cached) = fetch.cache.track(id) {
        return match from_cache(fetch, id).await {
            Ok(source) => Ok(Fetched {
                track: cached,
                source,
                origin: Origin::Cache,
                stored: false,
            }),
            Err(err) => Err(LoadFailure::Temporary(format!("{err:#}"))),
        };
    }

    // Somebody in the room says they hold this track.
    let mut peer_error = None;
    if !fetch.peers.is_empty() {
        match from_peers(fetch, want).await {
            Ok((source, stored)) => {
                return Ok(Fetched {
                    track: want.clone(),
                    source,
                    origin: Origin::Peer,
                    stored,
                });
            }
            // Not fatal — the peer may have evicted it, or gone — but not to be
            // swallowed either. With no internet this *is* the reason nothing
            // played, and the fix is usually a firewall on the other machine, so
            // it has to reach the person looking at the screen.
            Err(err) => {
                warn!(track = id, "не удалось забрать у участника: {err:#}");
                peer_error = Some(format!("{err:#}"));
            }
        }
    }

    let (track, url) = match yandex_stream(&fetch.api, id).await {
        Ok(pair) => pair,
        Err(err) => return Err(LoadFailure::from_yandex(&err, peer_error)),
    };

    // Straight through, without ever holding the whole track, when nothing needs
    // the bytes: that is how a streaming backend normally plays.
    if !fetch.player.needs_bytes() && !fetch.store {
        return Ok(Fetched {
            track,
            source: AudioSource::Url(url),
            origin: Origin::Yandex,
            stored: false,
        });
    }

    let data = match fetch.api.fetch_track(&url).await {
        Ok(data) => data,
        Err(err) => return Err(LoadFailure::Temporary(format!("{err:#}"))),
    };
    let stored = fetch.store && store(fetch, &track, &data).await.is_some();

    let source = if fetch.player.needs_bytes() {
        AudioSource::Bytes(data)
    } else {
        // A streaming backend plays the file that was just written; if the write
        // did not happen, the signed URL still works.
        match fetch.cache.file_url(id) {
            Some(file) => AudioSource::Url(file),
            None => AudioSource::Url(url),
        }
    };
    Ok(Fetched {
        track,
        source,
        origin: Origin::Yandex,
        stored,
    })
}

/// Produces a source for a track already on this disk.
async fn from_cache(fetch: &Fetch, track_id: &str) -> Result<AudioSource> {
    if !fetch.player.needs_bytes() {
        // ExoPlayer opens a local file itself, so nothing is read here at all.
        return fetch
            .cache
            .file_url(track_id)
            .map(AudioSource::Url)
            .with_context(|| format!("трек {track_id} исчез из кеша"));
    }
    let cache = Arc::clone(&fetch.cache);
    let id = track_id.to_string();
    let data = tokio::task::spawn_blocking(move || cache.read(&id))
        .await
        .context("задача чтения кеша упала")??;
    Ok(AudioSource::Bytes(data))
}

/// Pulls a track from whoever in the room has it. Returns the source and whether
/// it was stored.
///
/// Bytes that came from the room are always kept, whatever `store` says. Two
/// reasons: fetching them *was* the download the user asked for, and a streaming
/// backend has nothing to play from until they are a file on disk.
async fn from_peers(fetch: &Fetch, want: &TrackRef) -> Result<(AudioSource, bool)> {
    let mut last: Option<anyhow::Error> = None;
    for endpoint in &fetch.peers {
        let pulled = share::fetch_from_peer(
            &fetch.http,
            endpoint,
            &fetch.room_token,
            &want.track_id,
        )
        .await;
        let data = match pulled {
            Ok(data) => data,
            Err(err) => {
                last = Some(err);
                continue;
            }
        };

        let stored = store(fetch, want, &data).await.is_some();
        let source = match fetch.cache.file_url(&want.track_id) {
            Some(file) if !fetch.player.needs_bytes() => AudioSource::Url(file),
            // Either this backend wants bytes anyway, or the write failed — in
            // which case a streaming backend has nothing to open and this errors
            // out below.
            _ if fetch.player.needs_bytes() => AudioSource::Bytes(data),
            _ => {
                last = Some(anyhow!(
                    "трек получен от участника, но не удалось сохранить его на устройство"
                ));
                continue;
            }
        };
        return Ok((source, stored));
    }
    Err(last.unwrap_or_else(|| anyhow!("никто в комнате не раздаёт этот трек")))
}

/// Writes a track to the cache, reporting failure as `None` rather than an error.
///
/// A cache that cannot be written is a nuisance, not a reason to stop the music:
/// every caller has bytes in hand and can play them regardless.
async fn store(fetch: &Fetch, track: &TrackRef, data: &Bytes) -> Option<Insertion> {
    let cache = Arc::clone(&fetch.cache);
    let track = track.clone();
    let data = data.clone();
    let pinned = fetch.pinned;

    let result = tokio::task::spawn_blocking(move || cache.insert(&track, &data, pinned)).await;
    match result {
        Ok(Ok(insertion)) => Some(insertion),
        Ok(Err(err)) => {
            debug!("could not cache a track: {err:#}");
            None
        }
        Err(err) => {
            debug!("the cache write task failed: {err}");
            None
        }
    }
}

/// Resolves a track on Yandex, checking this account may play it.
async fn yandex_stream(api: &YandexMusic, track_id: &str) -> Result<(TrackRef, String)> {
    let track = api.track(track_id).await?;
    if !track.available {
        return Err(anyhow::Error::new(NotLicensed));
    }
    let url = api.stream_url(track_id).await?;
    Ok((track.to_track_ref(), url))
}

/// Gets a track onto this device, without involving the player.
///
/// Same order of preference as [`fetch_source`], for the same reasons — a track
/// the room already holds costs no internet and no Yandex request.
async fn download(fetch: &Fetch, want: &TrackRef) -> Result<(Origin, Insertion)> {
    let id = want.track_id.as_str();

    let mut last: Option<anyhow::Error> = None;
    for endpoint in &fetch.peers {
        match share::fetch_from_peer(&fetch.http, endpoint, &fetch.room_token, id).await {
            Ok(data) => {
                let insertion = store(fetch, want, &data)
                    .await
                    .context("не удалось сохранить трек на устройство")?;
                return Ok((Origin::Peer, insertion));
            }
            Err(err) => last = Some(err),
        }
    }
    if let Some(err) = last {
        debug!(track = id, "no luck on the local network: {err:#}");
    }

    let (track, url) = yandex_stream(&fetch.api, id).await?;
    let data = fetch.api.fetch_track(&url).await?;
    let insertion = store(fetch, &track, &data)
        .await
        .context("не удалось сохранить трек на устройство")?;
    Ok((Origin::Yandex, insertion))
}

/// Hands a resolved track to the backend.
async fn stage_track(
    player: &Arc<dyn Playback>,
    track: TrackRef,
    source: AudioSource,
) -> Result<()> {
    match source {
        AudioSource::Url(_) => player.load(track, source),
        AudioSource::Bytes(_) => {
            let player = Arc::clone(player);
            // Decoding probes the container; keep it off the runtime's workers.
            tokio::task::spawn_blocking(move || player.load(track, source))
                .await
                .context("задача загрузки звука упала")?
        }
    }
}

/// Snapshots can arrive out of order, and a relay restart resets the counter.
fn accept_seq(previous: Option<u64>, incoming: u64) -> bool {
    match previous {
        None => true,
        Some(previous) => {
            incoming > previous || previous.saturating_sub(incoming) > SEQ_RESTART_GAP
        }
    }
}

/// Whether a hard seek happened recently enough to hold off another one.
fn cooling_down(last_seek: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last_seek {
        None => false,
        Some(last) => now.duration_since(last) < SEEK_COOLDOWN,
    }
}

/// Converts a wanted audible position into the position to ask the player for,
/// undoing [`crate::config::SyncConfig::position_bias_ms`].
fn requested_position(audible_ms: u64, bias_ms: i64) -> u64 {
    (audible_ms as i64 - bias_ms).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report that prompted this: a phone with no internet, a PC in the room
    /// holding the track, and a fetch from that PC that failed. Calling it
    /// «недоступен на этом аккаунте» was wrong twice over — the account was fine,
    /// and writing the track off for the session hid the real cause.
    #[test]
    fn a_peer_that_could_not_be_reached_is_a_temporary_failure() {
        let failure = LoadFailure::from_yandex(
            &anyhow!("сеть недоступна"),
            Some("соединение отклонено".to_string()),
        );
        assert!(matches!(failure, LoadFailure::Temporary(_)), "{failure:?}");
        // Both halves are named: the peer failure is the actionable one, and the
        // Yandex failure explains why there was no fallback.
        assert!(failure.message().contains("соединение отклонено"));
        assert!(failure.message().contains("сеть недоступна"));
    }

    /// A licensing refusal only settles the matter when nobody in the room has the
    /// track: a neighbour's copy plays whatever this account is allowed.
    #[test]
    fn licensing_is_final_only_when_nobody_offered_the_track() {
        let alone = LoadFailure::from_yandex(&anyhow::Error::new(NotLicensed), None);
        assert!(matches!(alone, LoadFailure::NotLicensed(_)), "{alone:?}");

        let offered = LoadFailure::from_yandex(
            &anyhow::Error::new(NotLicensed),
            Some("таймаут".to_string()),
        );
        assert!(
            matches!(offered, LoadFailure::Temporary(_)),
            "somebody has it, so this may yet work: {offered:?}"
        );
    }

    /// A dead network is not a verdict on the track, so it must not be confused
    /// with one.
    #[test]
    fn an_unreachable_yandex_is_temporary() {
        let failure = LoadFailure::from_yandex(&anyhow!("operation timed out"), None);
        assert!(matches!(failure, LoadFailure::Temporary(_)), "{failure:?}");
    }

    /// The context survives the labelling, and so does the kind.
    #[test]
    fn labelling_keeps_the_kind_and_the_reason() {
        let labelled = LoadFailure::Temporary("таймаут".to_string()).labelled("Кино — Группа крови");
        assert!(matches!(labelled, LoadFailure::Temporary(_)));
        assert_eq!(labelled.message(), "Кино — Группа крови: таймаут");

        let denied = LoadFailure::NotLicensed("нельзя".to_string()).labelled("Трек");
        assert!(matches!(denied, LoadFailure::NotLicensed(_)));
        assert_eq!(denied.message(), "Трек: нельзя");
    }

    #[test]
    fn the_first_snapshot_is_always_accepted() {
        assert!(accept_seq(None, 0));
        assert!(accept_seq(None, 9_999));
    }

    #[test]
    fn newer_snapshots_are_accepted_and_stale_ones_dropped() {
        assert!(accept_seq(Some(10), 11));
        assert!(!accept_seq(Some(10), 10));
        assert!(!accept_seq(Some(10), 9));
    }

    #[test]
    fn a_restarted_relay_is_followed_again() {
        assert!(accept_seq(Some(5_000), 1));
        assert!(!accept_seq(Some(5_000), 4_990));
    }

    #[test]
    fn the_first_correction_is_never_held_back() {
        assert!(!cooling_down(None, std::time::Instant::now()));
    }

    /// ExoPlayer reports a playhead that trails the position it was just seeked
    /// to, so without a cooldown the engine seeks four times a second for ever.
    #[test]
    fn a_second_seek_waits_out_the_cooldown() {
        let now = std::time::Instant::now();
        let just_now = now - Duration::from_millis(250);
        assert!(cooling_down(Some(just_now), now));

        let long_ago = now - (SEEK_COOLDOWN + Duration::from_millis(1));
        assert!(!cooling_down(Some(long_ago), now));
    }

    /// A player reading 400 ms behind has to be asked for 400 ms less, so the
    /// audio lands where the room actually is.
    #[test]
    fn the_reporting_bias_is_undone_when_seeking() {
        assert_eq!(requested_position(10_000, 400), 9_600);
        assert_eq!(requested_position(10_000, 0), 10_000);
        assert_eq!(requested_position(10_000, -400), 10_400);
        // Never before the start of the track.
        assert_eq!(requested_position(100, 400), 0);
    }
}
