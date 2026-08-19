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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use ymsync_proto::{
    Correction, Event, PlaybackState, SyncParams, TrackRef, decide, target_position_ms, trust_clock,
};

use crate::api::{Track, YandexMusic};
use crate::config::Config;
use crate::link::Link;
use crate::playback::{AudioSource, Playback};

/// A snapshot whose seq is this far below the last one means the relay restarted
/// and began a fresh counter, rather than a frame arriving late.
const SEQ_RESTART_GAP: u64 = 64;

/// A load can fail for transient reasons — Yandex rate-limits concurrent stream
/// requests per account, and networks blip — so a track is only written off
/// after this many attempts.
const MAX_LOAD_ATTEMPTS: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

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
    /// Last thing worth telling the user about.
    pub notice: Option<String>,
}

/// Handle to a running engine. Dropping every clone of the command sender ends
/// the loop.
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Receiver<Snapshot>,
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

    /// Waits for the loop to finish — after [`Command::Shutdown`], or when the
    /// relay connection dies.
    pub async fn join(self) -> Result<()> {
        self.task.await.context("движок аварийно завершился")?
    }
}

/// Connects to the relay and starts the loop.
pub async fn spawn(
    cfg: &Config,
    api: Arc<YandexMusic>,
    player: Arc<dyn Playback>,
) -> Result<Handle> {
    let room_token = cfg.require_room_token()?;
    let (link, events) = Link::connect(
        &cfg.relay,
        &cfg.room,
        room_token,
        Duration::from_secs(cfg.sync.clock_probe_secs),
    )
    .await?;

    let peers = link.peers_at_join();
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
        notice: None,
    });

    let engine = Engine {
        api,
        player,
        link,
        params: cfg.sync.params(),
        queue: Vec::new(),
        revision: 0,
        station: None,
        refilling: false,
        remote: None,
        unavailable: HashSet::new(),
        attempts: HashMap::new(),
        loading: None,
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
        task,
    })
}

/// Result of a background track fetch.
enum Internal {
    Loaded {
        track_id: String,
        error: Option<String>,
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

struct Engine {
    api: Arc<YandexMusic>,
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
    /// Failed attempts per track, cleared once one succeeds.
    attempts: HashMap<String, u8>,

    loading: Option<String>,
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

            // The only command that never leaves this machine.
            Command::SetVolume(volume) => self.player.set_volume(volume),

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

        let api = Arc::clone(&self.api);
        let player = Arc::clone(&self.player);
        let internal = internal.clone();
        let track_id = track.track_id.clone();
        let label = track.to_string();

        tokio::spawn(async move {
            let error = load_track(&api, &player, &track_id)
                .await
                .err()
                .map(|err| format!("{label}: {err:#}"));
            let _ = internal.send(Internal::Loaded { track_id, error });
        });
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

            Internal::Retry { track_id } => {
                // Only if it is still what we want to be playing.
                if self.loading.is_none()
                    && let Some(track) = self.wanted_track()
                    && track.track_id == track_id
                {
                    self.start_load(&track, internal);
                }
            }

            Internal::Loaded { track_id, error } => {
                // A newer request overtook this one.
                if self.loading.as_deref() != Some(track_id.as_str()) {
                    return;
                }
                self.loading = None;

                if let Some(error) = error {
                    let attempts = self.attempts.entry(track_id.clone()).or_insert(0);
                    *attempts += 1;
                    let attempt = *attempts;

                    if attempt < MAX_LOAD_ATTEMPTS {
                        self.notice = Some(format!(
                            "попытка {attempt} из {MAX_LOAD_ATTEMPTS} не удалась, повторю — {error}"
                        ));
                        let internal = internal.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(RETRY_DELAY).await;
                            let _ = internal.send(Internal::Retry { track_id });
                        });
                        return;
                    }

                    // Accounts differ in what they may play. This peer sits the
                    // track out in silence rather than skipping it for everyone:
                    // the others can hear it perfectly well, and the relay will
                    // move the room on when the track's time is up.
                    self.unavailable.insert(track_id);
                    self.notice = Some(format!("пропускаю на этом аккаунте — {error}"));
                    self.player.pause();
                    return;
                }

                self.attempts.remove(&track_id);
                self.notice = None;
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

                // Someone replaced the queue outright, which ends any station —
                // including one we were feeding.
                if state.station.is_none() && self.station.is_some() {
                    self.station = None;
                    self.notice = Some("волна выключена".to_string());
                }

                if let Some(track) = state.track.clone() {
                    let staged = self.player.current_track_id();
                    let is_staged = staged.as_deref() == Some(track.track_id.as_str());
                    let is_loading = self.loading.as_deref() == Some(track.track_id.as_str());
                    if !is_staged && !is_loading && !self.unavailable.contains(&track.track_id) {
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

            Event::Error { code, message } => {
                self.notice = Some(format!("релей сообщил об ошибке ({code}): {message}"));
            }
        }
    }

    /// One pass: keep a station fed, then pull the playhead onto the room's.
    fn tick(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        self.feed_station(internal);
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
            notice: self.notice.clone(),
        });
    }
}

/// Resolves, downloads and stages one track.
async fn load_track(api: &YandexMusic, player: &Arc<dyn Playback>, track_id: &str) -> Result<()> {
    let track = api.track(track_id).await?;
    if !track.available {
        bail!("недоступен на этом аккаунте");
    }
    let url = api.stream_url(track_id).await?;
    let track_ref = track.to_track_ref();

    if !player.needs_bytes() {
        // A streaming backend (ExoPlayer) fetches the signed URL itself.
        return player.load(track_ref, AudioSource::Url(url));
    }

    let data = api.fetch_track(&url).await?;
    let player = Arc::clone(player);
    // Decoding probes the container; keep it off the runtime's worker threads.
    tokio::task::spawn_blocking(move || player.load(track_ref, AudioSource::Bytes(data)))
        .await
        .context("задача загрузки звука упала")?
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
