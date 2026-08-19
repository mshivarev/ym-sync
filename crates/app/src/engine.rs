//! The sync engine: one loop that owns the player and the relay link.
//!
//! Every front end (CLI, Tauri, the Android JNI layer) drives the same engine:
//! commands go in through a channel, and state comes out as snapshots on a
//! `watch` channel. Nothing here reads stdin or prints, so the loop is reusable
//! and the UI never blocks on a download.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use ymsync_proto::{
    Correction, Event, PlaybackState, Role, SyncParams, TrackRef, decide, target_position_ms,
    trust_clock, unix_ms,
};

use crate::api::{Track, YandexMusic};
use crate::config::Config;
use crate::link::Link;
use crate::playback::{AudioSource, Playback};

/// A snapshot whose seq is this far below the last one means the master
/// restarted and began a fresh counter, rather than a frame arriving late.
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
/// "slave подключился" would sit in every later snapshot and read as the current
/// state long after that peer left.
const NOTICE_TTL: Duration = Duration::from_secs(8);

/// How close to the end of the queue a station is topped up.
///
/// A station hands out a few tracks at a time, so the next batch has to be asked
/// for before the current one runs out — otherwise playback stops for as long as
/// the request takes.
const STATION_LEAD: usize = 2;

/// What a front end can ask the engine to do. A slave accepts only
/// [`Command::SetVolume`]; everything else is the master's to decide.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Replaces the queue and starts playing at `start`. Any station is dropped.
    SetQueue { tracks: Vec<TrackRef>, start: usize },
    /// Adds tracks to the end of the queue.
    ///
    /// Never interrupts what is playing — that is the whole point of a queue —
    /// but a queue that is empty or has run dry starts on the new tracks.
    Enqueue { tracks: Vec<TrackRef> },
    /// Follows an endless station, topping the queue up as it runs down.
    /// [`crate::api::WAVE_STATION`] is «Моя волна».
    PlayStation { id: String, replace: bool },
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
    pub role: Role,
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
    /// Slave only: how far ahead (+) or behind (-) the master we are.
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
    role: Role,
    api: Arc<YandexMusic>,
    player: Arc<dyn Playback>,
) -> Result<Handle> {
    let room_token = cfg.require_room_token()?;
    let (link, events) = Link::connect(
        &cfg.relay,
        &cfg.room,
        room_token,
        role,
        Duration::from_secs(cfg.sync.clock_probe_secs),
    )
    .await?;

    let peers = link.peers_at_join();
    let (snapshots, snapshot_rx) = watch::channel(Snapshot {
        role,
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
        drift_ms: None,
        rtt_ms: link.clock().rtt_ms(),
        offset_ms: link.clock().offset_ms(),
        notice: None,
    });

    let engine = Engine {
        role,
        api,
        player,
        link,
        params: cfg.sync.params(),
        queue: Vec::new(),
        revision: 0,
        index: 0,
        station: None,
        refilling: false,
        seq: 0,
        remote: None,
        unavailable: HashSet::new(),
        attempts: HashMap::new(),
        loading: None,
        want_play: false,
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

    let heartbeat = Duration::from_millis(cfg.sync.heartbeat_ms);
    let tick = Duration::from_millis(cfg.sync.correction_interval_ms);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(engine.run(command_rx, events, heartbeat, tick));

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

/// An endless station the master is following.
struct Station {
    id: String,
    /// The last track it handed out, so the next request continues from there
    /// instead of replaying the same batch.
    after: Option<String>,
}

struct Engine {
    role: Role,
    api: Arc<YandexMusic>,
    player: Arc<dyn Playback>,
    link: Link,
    params: SyncParams,

    queue: Vec<TrackRef>,
    revision: u64,
    index: usize,

    /// Master: the station feeding the queue, when one is playing.
    station: Option<Station>,
    /// A request for more station tracks is in flight.
    refilling: bool,

    /// Master: outgoing snapshot counter.
    seq: u64,
    /// Slave: the master's last known state.
    remote: Option<PlaybackState>,
    /// Tracks this account cannot play, so we stop retrying them.
    unavailable: HashSet<String>,
    /// Failed attempts per track, cleared once one succeeds.
    attempts: HashMap<String, u8>,

    loading: Option<String>,
    want_play: bool,
    drift_ms: Option<i64>,
    /// Cancels a backend's constant reporting lag; see [`SyncConfig`].
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
        heartbeat: Duration,
        tick: Duration,
    ) -> Result<()> {
        let (internal, mut internal_rx) = mpsc::unbounded_channel::<Internal>();

        let mut heartbeat = tokio::time::interval(heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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

                _ = tick.tick() => match self.role {
                    Role::Master => self.master_tick(&internal),
                    Role::Slave => self.slave_tick(),
                },

                _ = heartbeat.tick() => {
                    if self.role == Role::Master {
                        self.publish_state();
                    }
                }
            }

            self.publish();
        }

        self.player.pause();
        self.link.say_goodbye();
        self.connected = false;
        self.publish();
        Ok(())
    }

    fn handle_command(&mut self, command: Command, internal: &mpsc::UnboundedSender<Internal>) {
        // Volume is a local matter even on a slave; everything else would just
        // be undone by the next correction.
        if self.role == Role::Slave {
            match command {
                Command::SetVolume(volume) => self.player.set_volume(volume),
                _ => {
                    self.notice =
                        Some("ведомый следует за ведущим — команда проигнорирована".to_string());
                }
            }
            return;
        }

        match command {
            Command::SetQueue { tracks, start } => {
                if tracks.is_empty() {
                    self.notice = Some("пустая очередь".to_string());
                    return;
                }
                self.station = None;
                self.queue = tracks;
                self.revision += 1;
                self.unavailable.clear();
                self.attempts.clear();
                self.publish_queue();
                self.play_index(start.min(self.queue.len() - 1), internal);
            }
            Command::Enqueue { tracks } => self.absorb(tracks, internal),
            Command::PlayStation { id, replace } => {
                if replace {
                    self.queue.clear();
                    self.index = 0;
                    self.want_play = false;
                    self.player.pause();
                    self.unavailable.clear();
                    self.attempts.clear();
                    self.revision += 1;
                    self.publish_queue();
                    // The queue is empty until the first batch lands, and the
                    // slave has to be told to stop with us rather than play on.
                    self.publish_state();
                }
                self.notice = Some("настраиваю волну…".to_string());
                self.station = Some(Station { id, after: None });
                self.request_station_tracks(internal);
            }
            Command::PlayIndex(index) => self.play_index(index, internal),
            Command::TogglePause => {
                if self.player.is_playing() {
                    self.player.pause();
                    self.want_play = false;
                } else {
                    self.player.play();
                    self.want_play = true;
                }
                self.publish_state();
            }
            Command::SeekTo(ms) => self.seek(ms),
            Command::SeekBy(delta_ms) => {
                let target = (self.player.position_ms() as i64 + delta_ms).max(0) as u64;
                self.seek(target);
            }
            Command::Next => self.step(1, internal),
            Command::Prev => self.step(-1, internal),
            Command::SetVolume(volume) => self.player.set_volume(volume),
            // Handled by the loop.
            Command::Shutdown => {}
        }
    }

    fn seek(&mut self, ms: u64) {
        // Dragging past the end means "this track is done": clamping lets the
        // auto-advance pick it up instead of leaving a bogus playhead behind.
        let target = match self.wanted_track().map(|t| t.duration_ms) {
            Some(duration) if duration > 0 => ms.min(duration),
            _ => ms,
        };
        match self.player.seek_ms(target) {
            Ok(()) => self.publish_state(),
            Err(err) => self.notice = Some(format!("переход не удался: {err:#}")),
        }
    }

    fn step(&mut self, delta: i64, internal: &mpsc::UnboundedSender<Internal>) {
        if self.queue.is_empty() {
            return;
        }
        let target = self.index as i64 + delta;
        if target < 0 || target as usize >= self.queue.len() {
            self.notice = Some(
                if delta > 0 {
                    "конец очереди"
                } else {
                    "начало очереди"
                }
                .to_string(),
            );
            return;
        }
        self.play_index(target as usize, internal);
    }

    fn play_index(&mut self, index: usize, internal: &mpsc::UnboundedSender<Internal>) {
        let Some(track) = self.queue.get(index).cloned() else {
            self.notice = Some("в очереди нет такого трека".to_string());
            return;
        };
        self.index = index;
        self.want_play = true;
        self.start_load(&track, internal);
        self.publish_state();
    }

    /// Adds tracks to the end of the queue.
    ///
    /// The queue only starts playing when there is nothing to interrupt: adding
    /// to a queue that is already going must leave the current track alone.
    fn absorb(&mut self, tracks: Vec<TrackRef>, internal: &mpsc::UnboundedSender<Internal>) {
        if tracks.is_empty() {
            self.notice = Some("нечего добавить в очередь".to_string());
            return;
        }
        let added = tracks.len();
        let start = self.queue.len();
        let take_over = idle(
            start,
            self.want_play,
            self.player.is_finished(),
            self.loading.is_some(),
        );

        self.queue.extend(tracks);
        self.revision += 1;
        self.publish_queue();

        if take_over {
            self.play_index(start, internal);
        } else {
            self.notice = Some(format!("добавлено в очередь: {added}"));
            // Carries the new revision, so a slave knows its queue is stale.
            self.publish_state();
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

    fn publish_queue(&mut self) {
        if !self.link.publish_queue(self.revision, self.queue.clone()) {
            self.connected = false;
        }
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
                // The queue was replaced while the request was out.
                if self.station.is_none() {
                    return;
                }
                if let Some(error) = error {
                    // Retrying on a tick would hammer a broken endpoint four
                    // times a second, so the station stops here.
                    self.station = None;
                    self.notice = Some(format!("волна замолчала: {error}"));
                    return;
                }
                if let Some(station) = self.station.as_mut() {
                    station.after = tracks.last().map(|t| t.track_id.clone());
                }
                self.absorb(tracks, internal);
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

                    // Accounts differ in what they may play, so giving up on one
                    // track is expected rather than fatal.
                    self.unavailable.insert(track_id);
                    self.notice = Some(format!("пропускаю — {error}"));
                    self.player.pause();
                    if self.role == Role::Master && self.index + 1 < self.queue.len() {
                        self.play_index(self.index + 1, internal);
                    }
                    return;
                }

                self.attempts.remove(&track_id);
                self.notice = None;
                if self.role == Role::Master {
                    if self.want_play {
                        self.player.play();
                    }
                    self.publish_state();
                }
                // A slave is positioned by the next correction tick.
            }
        }
    }

    /// The track this engine should currently have staged.
    fn wanted_track(&self) -> Option<TrackRef> {
        match self.role {
            Role::Master => self.queue.get(self.index).cloned(),
            Role::Slave => self.remote.as_ref().and_then(|s| s.track.clone()),
        }
    }

    fn handle_event(&mut self, event: Event, internal: &mpsc::UnboundedSender<Internal>) {
        match event {
            Event::State(state) => {
                if self.role != Role::Slave {
                    return;
                }
                if !accept_seq(self.remote.as_ref().map(|s| s.seq), state.seq) {
                    return;
                }
                self.index = state.index;

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
                if self.role == Role::Slave {
                    self.revision = revision;
                    self.queue = tracks;
                }
            }

            Event::Peer {
                role,
                joined,
                peers,
            } => {
                self.peers = peers;
                self.notice = Some(format!(
                    "{role} {}",
                    if joined {
                        "подключился"
                    } else {
                        "отключился"
                    }
                ));
                // The relay stores nothing, so a fresh peer has no queue until
                // the master repeats it.
                if joined && self.role == Role::Master && !self.queue.is_empty() {
                    self.publish_queue();
                    self.publish_state();
                }
            }

            Event::Error { code, message } => {
                self.notice = Some(format!("релей сообщил об ошибке ({code}): {message}"));
            }
        }
    }

    /// Master: move to the next track once the current one plays out.
    fn master_tick(&mut self, internal: &mpsc::UnboundedSender<Internal>) {
        // Ahead of the other guards: a station has to be topped up even while a
        // track is loading, and especially once the queue has run dry.
        if self.station.is_some() && self.index + STATION_LEAD >= self.queue.len() {
            self.request_station_tracks(internal);
        }
        if self.loading.is_some() || !self.want_play || self.queue.is_empty() {
            return;
        }
        if !self.player.is_finished() {
            return;
        }
        if self.index + 1 < self.queue.len() {
            self.play_index(self.index + 1, internal);
        } else {
            self.want_play = false;
            // A station refills itself, so its queue is not really over.
            if self.station.is_none() {
                self.notice = Some("очередь закончилась".to_string());
            }
            self.publish_state();
        }
    }

    /// Slave: pull our playhead back onto the master's.
    fn slave_tick(&mut self) {
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
        // Still fetching, or sitting this track out.
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
        // Past the end: the master has finished this track, so stop rather than
        // chase a position that does not exist.
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

    /// Master: tell the room where the playhead is.
    fn publish_state(&mut self) {
        if self.role != Role::Master {
            return;
        }
        self.seq += 1;

        let track = self.queue.get(self.index).cloned();
        // While a download is in flight the sink still holds the previous track,
        // so reporting its position would send the slave chasing a ghost.
        let loading = self.loading.is_some();
        let finished = !loading && self.player.is_finished();
        let position_ms = match (loading, finished) {
            (true, _) => 0,
            (_, true) => track.as_ref().map_or(0, |t| t.duration_ms),
            _ => self.player.position_ms(),
        };

        let state = PlaybackState {
            seq: self.seq,
            track,
            index: self.index,
            queue_revision: self.revision,
            position_ms,
            playing: !loading && self.player.is_playing(),
            at_server_ms: self.link.clock().now_server_ms().unwrap_or_else(unix_ms),
        };
        if !self.link.publish(state) {
            self.connected = false;
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
            role: self.role,
            connected: self.connected,
            peers: self.peers,
            queue: self.queue.clone(),
            queue_revision: self.revision,
            index: self.index,
            track,
            position_ms,
            duration_ms,
            playing: self.player.is_playing(),
            loading: self.loading.is_some(),
            volume: self.player.volume(),
            drift_ms: self.drift_ms,
            rtt_ms: self.link.clock().rtt_ms(),
            offset_ms: self.link.clock().offset_ms(),
            notice: self.notice.clone(),
        });
    }
}

/// Resolves, downloads and stages one track.
async fn load_track(
    api: &YandexMusic,
    player: &Arc<dyn Playback>,
    track_id: &str,
) -> Result<()> {
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

/// Snapshots can arrive out of order, and a master restart resets the counter.
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

/// Whether tracks appended to the queue should start playing at once.
///
/// Adding to a queue must never cut off what is playing. It should, though, pick
/// up a queue that is empty or has played itself out — otherwise a station's next
/// batch would arrive to silence.
fn idle(queued: usize, want_play: bool, finished: bool, loading: bool) -> bool {
    if loading || want_play {
        return false;
    }
    queued == 0 || finished
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
    fn a_restarted_master_is_followed_again() {
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
    /// audio lands where the master actually is.
    #[test]
    fn the_reporting_bias_is_undone_when_seeking() {
        assert_eq!(requested_position(10_000, 400), 9_600);
        assert_eq!(requested_position(10_000, 0), 10_000);
        assert_eq!(requested_position(10_000, -400), 10_400);
        // Never before the start of the track.
        assert_eq!(requested_position(100, 400), 0);
    }

    /// The whole point of «В очередь»: a track added while something is playing
    /// waits its turn instead of taking over.
    #[test]
    fn adding_to_a_playing_queue_does_not_interrupt_it() {
        assert!(!idle(5, true, false, false), "playing");
        assert!(!idle(5, true, true, false), "between tracks");
        assert!(!idle(5, false, false, false), "paused mid-track");
        assert!(!idle(0, true, false, true), "a load is in flight");
    }

    #[test]
    fn an_empty_or_finished_queue_picks_up_the_new_tracks() {
        assert!(idle(0, false, true, false), "nothing queued yet");
        assert!(idle(0, false, false, false), "nothing queued, nothing staged");
        assert!(idle(5, false, true, false), "the queue played itself out");
    }
}
