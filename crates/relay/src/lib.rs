//! Room relay: owns the room's queue and playhead, and answers clock probes.
//!
//! Protocol 3 moved the queue and the playhead here, so that any peer may
//! command the room without two of them fighting over one timeline. Every
//! mutation arrives as a [`Command`], is applied in one place, and goes back out
//! to everybody — including its sender — as the new truth.
//!
//! The relay still knows nothing about Yandex. It has no token and cannot
//! resolve a track, so a client always hands it ready-made [`TrackRef`]s. That
//! is also why an endless station needs a *feeder*: one peer claims the station
//! and keeps pushing batches in. If that peer leaves, the claim is dropped and
//! the queue simply plays itself out.
//!
//! Protocol 4 added one more thing it brokers but never carries: which peer can
//! serve which cached track over the local network. Only the availability map
//! passes through here — the audio goes straight from peer to peer, so hosting a
//! room from a phone does not mean shovelling every track through it.
//!
//! What restarting the relay costs, now that it holds state: the room's queue
//! and position are lost, and peers reconnect to an empty room. Clock probes and
//! room membership recover on their own, as before.
//!
//! This is a library so a player can host a room inside its own process (see
//! [`bind`]) rather than requiring a separate `ymsync-relay`. That is also what
//! makes offline listening work: a machine with no internet hosts a room on
//! itself, so the queue and the playhead have an authority again and nothing in
//! the client has to change.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use ymsync_proto::discovery::{DISCOVERY_PORT, Discovery, MAX_DATAGRAM, MAX_ROOMS_IN_REPLY, RoomBrief};
use ymsync_proto::{
    ClientMsg, Command, PROTOCOL_VERSION, PeerShare, PlaybackState, RepeatMode, RosterEntry,
    ServerMsg, TrackRef, secret_eq, unix_ms,
};

/// A client that says nothing at all for this long is assumed dead. Every peer
/// probes the clock every few seconds, so this only trips on one that vanished
/// without closing the socket — a phone leaving Wi-Fi, or an app killed. Keep it
/// short: until it fires, that peer still counts as present and may still hold
/// the station claim.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a fresh connection has to send its `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_ROOM_LEN: usize = 64;

/// Upper bound on a room's queue. «Мне нравится» runs into the thousands, so
/// this is generous; it exists only so one client cannot exhaust the relay's
/// memory now that the relay is what holds the queue.
const MAX_QUEUE: usize = 5_000;

/// Upper bound on how many cached tracks one peer may offer to serve.
///
/// Generous for the same reason as [`MAX_QUEUE`] — a cache built up over months
/// is meant to be shareable in full — but bounded, because the whole list goes
/// back out to the room whenever it changes.
const MAX_SHARED: usize = 10_000;

/// How often the relay looks for a track that has played out.
///
/// One shared ticker rather than a timer per room: a room's deadline changes on
/// every pause, seek and skip, and cancelling-and-respawning a timer each time
/// is a lifecycle bug waiting to happen. The cost is that a track change lands
/// up to this late — well inside the correction threshold, and identical for
/// every peer, since they all take the new anchor from the same snapshot.
const ADVANCE_TICK: Duration = Duration::from_millis(100);

/// How long a room with nobody in it keeps its queue.
///
/// Long enough that stepping out and coming back finds the music where you left
/// it, short enough that abandoned rooms cannot pile up in memory for ever.
const EMPTY_ROOM_TTL: Duration = Duration::from_secs(12 * 60 * 60);

type WsStream = tokio_tungstenite::WebSocketStream<TcpStream>;
type WsRead = futures_util::stream::SplitStream<WsStream>;

/// A running relay.
///
/// Dropping it stops listening and drops every connection, so a front end can
/// tie hosting to the lifetime of a single value.
pub struct Server {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
    ticker: Option<JoinHandle<()>>,
    /// Answers discovery broadcasts. Absent when discovery is off, or when the
    /// well-known UDP port was already taken.
    beacon: Option<JoinHandle<()>>,
}

impl Server {
    /// The address actually bound. Worth reading even when the caller named one:
    /// port 0 asks the OS to pick, which is how an embedded relay stays out of
    /// the way of whatever else is already on the machine.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether this relay answers «кто держит комнаты» broadcasts.
    pub fn discoverable(&self) -> bool {
        self.beacon.is_some()
    }

    /// Stops listening and drops every peer, waiting for the tasks to finish.
    pub async fn stop(&mut self) {
        // The connections are told first: one that notices here closes its
        // socket, rather than having it severed underneath it.
        let _ = self.shutdown.send(true);
        for task in [self.accept.take(), self.ticker.take(), self.beacon.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in [
            self.accept.as_ref(),
            self.ticker.as_ref(),
            self.beacon.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            task.abort();
        }
    }
}

/// Binds a relay and serves it in the background.
///
/// `bind_addr` is the usual `host:port`; port 0 asks the OS for a free one,
/// which [`Server::local_addr`] then reports.
///
/// Answers discovery broadcasts as well — see [`bind_with`] to turn that off.
pub async fn bind(bind_addr: &str, token: impl Into<String>) -> Result<Server> {
    bind_with(bind_addr, token, true).await
}

/// [`bind`], with a say over discovery.
///
/// Turning it off leaves the room reachable for anyone told its address by hand,
/// and invisible to `ymsync rooms`.
pub async fn bind_with(
    bind_addr: &str,
    token: impl Into<String>,
    discoverable: bool,
) -> Result<Server> {
    let token = token.into();
    if token.trim().is_empty() {
        bail!(
            "не задан токен комнаты: без общего секрета релей не поднимается, \
             иначе в комнату войдёт кто угодно из сети"
        );
    }

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("cannot listen on {bind_addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading the relay's own address")?;
    info!(bind = %local_addr, protocol = PROTOCOL_VERSION, "relay listening");

    let (shutdown, shutdown_rx) = watch::channel(false);
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    let beacon = if discoverable {
        start_beacon(local_addr, Arc::clone(&rooms), shutdown_rx.clone()).await
    } else {
        None
    };

    let ticker = tokio::spawn(advance_ended_tracks(
        Arc::clone(&rooms),
        shutdown_rx.clone(),
    ));
    let accept = tokio::spawn(accept_loop(listener, Arc::new(token), rooms, shutdown_rx));

    Ok(Server {
        local_addr,
        shutdown,
        accept: Some(accept),
        ticker: Some(ticker),
        beacon,
    })
}

/// Brings up the discovery responder, or explains why it stayed down.
///
/// Never fatal. A relay that cannot be found is still a relay, and the two ways
/// this fails are both ordinary: the well-known UDP port already belongs to
/// another relay on this machine, or the room is bound to loopback and there is
/// nobody out there to answer.
async fn start_beacon(
    local_addr: SocketAddr,
    rooms: Rooms,
    shutdown: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    if local_addr.ip().is_loopback() {
        info!(
            "комната привязана к loopback — обнаружение выключено: снаружи к ней \
             всё равно не подключиться"
        );
        return None;
    }

    match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await {
        Ok(socket) => {
            info!(port = DISCOVERY_PORT, "answering discovery broadcasts");
            Some(tokio::spawn(answer_discovery(
                socket,
                local_addr.port(),
                instance_id(),
                rooms,
                shutdown,
            )))
        }
        Err(err) => {
            warn!(
                port = DISCOVERY_PORT,
                "обнаружение выключено, порт занят (обычно — другим релеем на этой \
                 машине): {err}"
            );
            None
        }
    }
}

/// Identifies this relay process in its discovery answers.
///
/// Built from the process id and the moment of asking rather than from a random
/// generator, which would mean a dependency for something that only has to be
/// unlikely to collide on one local network. Two relays on one machine differ by
/// pid; two on different machines differ by start time.
fn instance_id() -> u64 {
    ((std::process::id() as u64) << 32) ^ (unix_ms() as u64 & 0xffff_ffff)
}

/// Tells anyone who asks which rooms this relay holds.
///
/// The answer carries names and listener counts and nothing else — see
/// [`ymsync_proto::discovery`] for what that deliberately leaves out.
async fn answer_discovery(
    socket: UdpSocket,
    tcp_port: u16,
    id: u64,
    rooms: Rooms,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut buffer = vec![0u8; MAX_DATAGRAM];
    loop {
        let (len, from) = tokio::select! {
            received = socket.recv_from(&mut buffer) => match received {
                Ok(pair) => pair,
                // A datagram nobody was listening for can fail the *next* recv on
                // Windows (ICMP port unreachable surfaces as an error on the
                // socket). Carrying on is right: the socket is still usable.
                Err(err) => {
                    debug!(%err, "discovery socket hiccup");
                    continue;
                }
            },
            _ = shutdown.changed() => return,
        };

        match serde_json::from_slice::<Discovery>(&buffer[..len]) {
            Ok(Discovery::Query { protocol }) => {
                debug!(%from, protocol, "discovery query");
                let list = room_list(&rooms).await;
                let named = list.len();
                let reply = encode_discovery(&Discovery::Rooms {
                    protocol: PROTOCOL_VERSION,
                    id,
                    port: tcp_port,
                    rooms: list,
                });
                match socket.send_to(&reply, from).await {
                    Ok(sent) => debug!(%from, rooms = named, bytes = sent, "answered discovery"),
                    Err(err) => debug!(%from, %err, "cannot answer discovery"),
                }
            }
            // Our own answer, seen because a client on this machine broadcast the
            // question; or somebody else's traffic on the same port.
            Ok(Discovery::Rooms { .. }) => {}
            Err(err) => debug!(%from, %err, "undecodable discovery datagram"),
        }
    }
}

/// The rooms this relay is willing to name, sorted so that two consecutive
/// answers to the same question look identical.
async fn room_list(rooms: &Rooms) -> Vec<RoomBrief> {
    let guard = rooms.lock().await;
    let mut list: Vec<RoomBrief> = guard
        .iter()
        .map(|(name, room)| RoomBrief {
            name: name.clone(),
            listeners: room.peers.len(),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list.truncate(MAX_ROOMS_IN_REPLY);
    list
}

fn encode_discovery(msg: &Discovery) -> Vec<u8> {
    serde_json::to_vec(msg).unwrap_or_else(|_| b"{}".to_vec())
}

async fn accept_loop(
    listener: TcpListener,
    token: Arc<String>,
    rooms: Rooms,
    shutdown: watch::Receiver<bool>,
) {
    let next_id = AtomicU64::new(1);
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(%err, "accept failed");
                continue;
            }
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let rooms = Arc::clone(&rooms);
        let token = Arc::clone(&token);
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            // A connection would outlive the accept loop, so each one watches
            // for the shutdown itself.
            let served = tokio::select! {
                result = serve(stream, addr, id, rooms, token) => result,
                _ = shutdown.changed() => Ok(()),
            };
            if let Err(err) = served {
                debug!(peer = id, %addr, %err, "connection closed");
            }
        });
    }
}

struct Peer {
    tx: mpsc::UnboundedSender<Message>,
    /// What this peer offers to serve the others over the local network.
    ///
    /// The `peer` field inside repeats the map key, which costs a few bytes and
    /// makes rebroadcasting the room's map a plain `collect`.
    share: Option<PeerShare>,
    /// The listener's chosen name, as the room's list shows it.
    name: String,
}

/// Longest name kept, in characters. A name is a label in a list, not a message.
const MAX_NAME_CHARS: usize = 32;

/// A name as the room will show it: trimmed, without control characters, and
/// cut to [`MAX_NAME_CHARS`].
fn clean_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

/// What a mutation touched, so we send only the frames that changed. The queue
/// is the expensive one: a thousand tracks must not go out on every pause.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct Changed {
    state: bool,
    queue: bool,
}

impl Changed {
    const NOTHING: Self = Self {
        state: false,
        queue: false,
    };
    const STATE: Self = Self {
        state: true,
        queue: false,
    };
    const BOTH: Self = Self {
        state: true,
        queue: true,
    };
}

/// The room's authoritative playback state.
///
/// `position_ms` together with `at_server_ms` is an *anchor*, not a running
/// clock: while `playing`, the playhead advances from it by itself, so the relay
/// only speaks when something actually changes.
#[derive(Default)]
struct RoomState {
    seq: u64,
    queue: Vec<TrackRef>,
    queue_revision: u64,
    index: usize,
    position_ms: u64,
    playing: bool,
    at_server_ms: i64,
    /// The station being fed into this queue, and the peer feeding it. Only that
    /// peer can resolve more tracks, since the relay holds no credentials.
    ///
    /// The owner is optional because the station outlives it: when the feeder
    /// leaves, the room still follows the wave and simply has nobody topping it
    /// up, which is what [`PlaybackState::station_unfed`] tells everyone.
    station: Option<(String, Option<u64>)>,
    repeat: RepeatMode,
    shuffle: bool,
    /// For shuffling. A plain xorshift: the order only has to look random to a
    /// listener, and a dependency for that would be out of all proportion.
    rng: u64,
}

impl RoomState {
    /// A number in `0..bound`, for picking shuffled positions.
    fn random_below(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        if self.rng == 0 {
            // Seeded on first use from the clock; never zero, which xorshift
            // would never leave.
            self.rng = (ymsync_proto::unix_ms() as u64) | 1;
        }
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x % bound as u64) as usize
    }

    /// Puts the tracks after the current one in random order. What is playing
    /// stays where it is: shuffling must never interrupt it.
    fn shuffle_upcoming(&mut self) {
        let start = (self.index + 1).min(self.queue.len());
        // Fisher–Yates over the upcoming part only.
        for i in (start + 1..self.queue.len()).rev() {
            let j = start + self.random_below(i - start + 1);
            self.queue.swap(i, j);
        }
    }

    fn snapshot(&self) -> PlaybackState {
        PlaybackState {
            seq: self.seq,
            track: self.queue.get(self.index).cloned(),
            index: self.index,
            queue_revision: self.queue_revision,
            position_ms: self.position_ms,
            playing: self.playing,
            at_server_ms: self.at_server_ms,
            station: self.station.as_ref().map(|(id, _)| id.clone()),
            station_unfed: matches!(&self.station, Some((_, None))),
            repeat: self.repeat,
            shuffle: self.shuffle,
        }
    }

    fn current_duration_ms(&self) -> Option<u64> {
        self.queue.get(self.index).map(|track| track.duration_ms)
    }

    /// Where the playhead stands at `now`, in relay time.
    fn derived_position_ms(&self, now: i64) -> u64 {
        if self.playing {
            self.position_ms + (now - self.at_server_ms).max(0) as u64
        } else {
            self.position_ms
        }
    }

    /// Whether the queue has run itself out: stopped, on a track that has played
    /// to its end. A batch arriving from a station has to pick this up, or the
    /// wave would land in silence and stay there.
    fn played_out(&self) -> bool {
        !self.playing
            && self
                .current_duration_ms()
                .is_some_and(|duration| self.position_ms >= duration)
    }

    fn start_at(&mut self, index: usize, now: i64) {
        self.index = index;
        self.position_ms = 0;
        self.playing = true;
        self.at_server_ms = now;
    }

    /// Advances when the current track has played to its end. Returns whether
    /// anything changed. Called from the shared ticker.
    ///
    /// The end is taken from Yandex's `duration_ms` rather than from any peer's
    /// decoder: it is the one figure all peers already agree on. Where the
    /// metadata is a little short, the tail is clipped instead of the room
    /// drifting apart, which is the better of the two failures.
    fn advance_if_ended(&mut self, now: i64) -> bool {
        if !self.playing {
            return false;
        }
        let Some(duration) = self.current_duration_ms() else {
            return false;
        };
        if self.derived_position_ms(now) < duration {
            return false;
        }

        if self.repeat == RepeatMode::One {
            // The same track from the top, until somebody skips it.
            self.start_at(self.index, now);
        } else if self.index + 1 < self.queue.len() {
            self.start_at(self.index + 1, now);
        } else if self.repeat == RepeatMode::All && !self.queue.is_empty() {
            // Round again, in the same order — shuffled or not. Reshuffling here
            // would change the queue from the ticker, which only ever speaks the
            // playhead; turning shuffle off and on gives a new order.
            self.start_at(0, now);
        } else {
            // Stop on the last track rather than past it, so a station's next
            // batch has somewhere to take over from.
            self.playing = false;
            self.position_ms = duration;
            self.at_server_ms = now;
        }
        self.seq += 1;
        true
    }

    /// Applies one peer's command. `Err` carries an error code for that peer;
    /// the room is left untouched in that case.
    fn apply(&mut self, command: Command, from: u64, now: i64) -> Result<Changed, &'static str> {
        let changed = match command {
            Command::SetQueue { tracks, start } => {
                if tracks.is_empty() {
                    return Err("empty_queue");
                }
                if tracks.len() > MAX_QUEUE {
                    return Err("queue_too_long");
                }
                // An explicit queue ends whatever station was feeding this room:
                // the peer asking for these tracks means these tracks.
                self.station = None;
                self.queue = tracks;
                self.queue_revision += 1;
                self.start_at(start.min(self.queue.len() - 1), now);
                Changed::BOTH
            }
            Command::Enqueue { tracks } => {
                if tracks.is_empty() {
                    return Err("empty_queue");
                }
                if self.queue.len() + tracks.len() > MAX_QUEUE {
                    return Err("queue_too_long");
                }
                let take_over = self.queue.is_empty() || self.played_out();
                let start = self.queue.len();
                if self.shuffle && !take_over {
                    // A shuffled room scatters new tracks among the upcoming ones
                    // instead of lining them up at the end.
                    for track in tracks {
                        let first = (self.index + 1).min(self.queue.len());
                        let at = first + self.random_below(self.queue.len() - first + 1);
                        self.queue.insert(at, track);
                    }
                } else {
                    self.queue.extend(tracks);
                }
                self.queue_revision += 1;
                if take_over {
                    self.start_at(start, now);
                }
                Changed::BOTH
            }
            Command::PlayNext { tracks } => {
                if tracks.is_empty() {
                    return Err("empty_queue");
                }
                if self.queue.len() + tracks.len() > MAX_QUEUE {
                    return Err("queue_too_long");
                }
                // Nothing playing or the queue played out: «next» is simply «now».
                let take_over = self.queue.is_empty() || self.played_out();
                let at = if take_over {
                    self.queue.len()
                } else {
                    (self.index + 1).min(self.queue.len())
                };
                let count = tracks.len();
                self.queue.splice(at..at, tracks);
                self.queue_revision += 1;
                if take_over {
                    self.start_at(at, now);
                }
                debug_assert!(at + count <= self.queue.len());
                Changed::BOTH
            }
            Command::SetRepeat { mode } => {
                if self.repeat == mode {
                    return Ok(Changed::NOTHING);
                }
                self.repeat = mode;
                Changed::STATE
            }
            Command::SetShuffle { on } => {
                if self.shuffle == on {
                    return Ok(Changed::NOTHING);
                }
                self.shuffle = on;
                if on {
                    self.shuffle_upcoming();
                    self.queue_revision += 1;
                    Changed::BOTH
                } else {
                    // Turning it off keeps the order as it now stands: tracks are
                    // not yanked back into place under the listener.
                    Changed::STATE
                }
            }
            Command::PlayIndex { index } => {
                if index >= self.queue.len() {
                    return Err("no_such_index");
                }
                self.start_at(index, now);
                Changed::STATE
            }
            Command::Next => {
                if self.queue.is_empty() {
                    return Err("empty_queue");
                }
                if self.index + 1 >= self.queue.len() {
                    // With repeat on, «next» after the last track is the first.
                    if self.repeat == RepeatMode::All {
                        self.start_at(0, now);
                        return Ok(self.bump(Changed::STATE));
                    }
                    return Err("end_of_queue");
                }
                self.start_at(self.index + 1, now);
                Changed::STATE
            }
            Command::Prev => {
                if self.queue.is_empty() {
                    return Err("empty_queue");
                }
                // At the top of the queue this restarts the track, which is what
                // every music player does.
                self.start_at(self.index.saturating_sub(1), now);
                Changed::STATE
            }
            Command::Pause => {
                if !self.playing {
                    return Ok(Changed::NOTHING);
                }
                self.position_ms = self.derived_position_ms(now);
                self.playing = false;
                self.at_server_ms = now;
                Changed::STATE
            }
            Command::Resume => {
                if self.playing {
                    return Ok(Changed::NOTHING);
                }
                if self.queue.is_empty() {
                    return Err("empty_queue");
                }
                self.playing = true;
                self.at_server_ms = now;
                Changed::STATE
            }
            Command::Seek { position_ms } => {
                let Some(duration) = self.current_duration_ms() else {
                    return Err("empty_queue");
                };
                self.position_ms = position_ms.min(duration);
                self.at_server_ms = now;
                Changed::STATE
            }
            Command::SetStation { id } => match id {
                Some(id) => {
                    // A station nobody feeds is free for the taking: that is how
                    // a wave survives the peer that started it. Only a *live*
                    // feeder makes the claim somebody else's.
                    if self
                        .station
                        .as_ref()
                        .is_some_and(|(_, owner)| owner.is_some_and(|owner| owner != from))
                    {
                        return Err("station_taken");
                    }
                    self.station = Some((id, Some(from)));
                    Changed::STATE
                }
                None => {
                    // Only the feeder may switch the wave off. A peer that never
                    // fed it saying "off" is a stale echo, not a decision.
                    if self
                        .station
                        .as_ref()
                        .is_none_or(|(_, owner)| *owner != Some(from))
                    {
                        return Ok(Changed::NOTHING);
                    }
                    self.station = None;
                    Changed::STATE
                }
            },
        };

        Ok(self.bump(changed))
    }

    /// Every accepted change to the playhead moves `seq` on, so peers can tell a
    /// newer snapshot from an older one arriving late.
    fn bump(&mut self, changed: Changed) -> Changed {
        if changed.state {
            self.seq += 1;
        }
        changed
    }
}

#[derive(Default)]
struct Room {
    peers: HashMap<u64, Peer>,
    state: RoomState,
    /// Set while nobody is connected. The room and its queue are kept so a
    /// listener who steps out can come back to them, but not for ever — see
    /// [`EMPTY_ROOM_TTL`].
    empty_since: Option<std::time::Instant>,
}

impl Room {
    /// Sends a frame to every peer, the sender included: the relay's answer is
    /// the truth even for whoever asked for it.
    fn tell_everyone(&self, msg: &ServerMsg) {
        let frame = encode(msg);
        for peer in self.peers.values() {
            let _ = peer.tx.send(frame.clone());
        }
    }

    fn publish(&self, changed: Changed) {
        if changed.queue {
            self.tell_everyone(&ServerMsg::Queue {
                revision: self.state.queue_revision,
                tracks: self.state.queue.clone(),
            });
        }
        if changed.state {
            self.tell_everyone(&ServerMsg::State {
                state: self.state.snapshot(),
            });
        }
    }

    /// Everyone currently offering to serve cached tracks.
    fn shares(&self) -> Vec<PeerShare> {
        let mut shares: Vec<PeerShare> =
            self.peers.values().filter_map(|p| p.share.clone()).collect();
        // Ordered so an unchanged room produces an identical frame; without it
        // the map would come out in `HashMap` order and look different every
        // time, which the clients would take for a change.
        shares.sort_by_key(|share| share.peer);
        shares
    }

    /// Everybody in the room, in the order they arrived.
    fn roster(&self) -> Vec<RosterEntry> {
        let mut roster: Vec<RosterEntry> = self
            .peers
            .iter()
            .map(|(id, peer)| RosterEntry {
                peer: *id,
                name: peer.name.clone(),
            })
            .collect();
        // Ids are handed out in arrival order, so this is also «who came first».
        roster.sort_by_key(|entry| entry.peer);
        roster
    }

    /// Tells the room who is in it. Whole, like the sharing map: it only moves
    /// when somebody joins, leaves or renames themselves.
    fn publish_roster(&self) {
        self.tell_everyone(&ServerMsg::Roster {
            peers: self.roster(),
        });
    }

    /// Tells the room who can serve what. Sent whole rather than as a diff: the
    /// list only moves when somebody finishes a download or leaves, so there is
    /// nothing to gain from tracking deltas.
    fn publish_shares(&self) {
        self.tell_everyone(&ServerMsg::Shares {
            peers: self.shares(),
        });
    }
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

fn error_message(code: &str) -> &'static str {
    match code {
        "empty_queue" => "в комнате нечего играть",
        "queue_too_long" => "очередь не может быть такой длинной",
        "no_such_index" => "в очереди нет такого номера",
        "end_of_queue" => "это последний трек в очереди",
        "station_taken" => "волну в этой комнате уже ведёт другой участник",
        "too_many_shares" => "слишком много треков в одном объявлении о раздаче",
        _ => "команда отклонена",
    }
}

/// Moves every room on when its current track has played out, and forgets rooms
/// that have stood empty for too long. See [`ADVANCE_TICK`] for why this is one
/// shared ticker.
async fn advance_ended_tracks(rooms: Rooms, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(ADVANCE_TICK);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => return,
        }
        let now = unix_ms();
        let mut guard = rooms.lock().await;

        for (name, room) in guard.iter_mut() {
            // An empty room is frozen on purpose; never run its queue down with
            // nobody listening.
            if room.peers.is_empty() {
                continue;
            }
            if room.state.advance_if_ended(now) {
                debug!(room = %name, index = room.state.index, playing = room.state.playing, "track ended");
                room.publish(Changed::STATE);
            }
        }

        guard.retain(|name, room| {
            let expired = room
                .empty_since
                .is_some_and(|since| since.elapsed() >= EMPTY_ROOM_TTL);
            if expired {
                info!(room = %name, "empty for too long; forgetting its queue");
            }
            !expired
        });
    }
}

async fn serve(
    stream: TcpStream,
    addr: SocketAddr,
    id: u64,
    rooms: Rooms,
    token: Arc<String>,
) -> Result<()> {
    // Nagle would add tens of milliseconds to the small, latency-critical state
    // and clock frames.
    let _ = stream.set_nodelay(true);

    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .context("websocket handshake")?;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let result = session(&mut ws_rx, &tx, id, addr, &rooms, &token).await;

    // Dropping our sender ends the writer loop once everything queued (an error
    // frame, a peer notification) has actually been flushed.
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    result
}

async fn session(
    ws_rx: &mut WsRead,
    tx: &mpsc::UnboundedSender<Message>,
    id: u64,
    addr: SocketAddr,
    rooms: &Rooms,
    token: &str,
) -> Result<()> {
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, next_client_msg(ws_rx)).await {
        Err(_) => {
            reject(tx, "timeout", "no hello within 10s");
            return Ok(());
        }
        Ok(None) => return Ok(()),
        Ok(Some(msg)) => msg,
    };

    let (room_name, client, name) = match hello {
        ClientMsg::Hello {
            protocol,
            room,
            token: given,
            client,
            name,
        } => {
            if protocol != PROTOCOL_VERSION {
                reject(
                    tx,
                    "protocol",
                    &format!("relay speaks protocol {PROTOCOL_VERSION}, client speaks {protocol}"),
                );
                return Ok(());
            }
            if !secret_eq(token, &given) {
                warn!(peer = id, %addr, "rejected: bad room token");
                reject(tx, "unauthorized", "bad room token");
                return Ok(());
            }
            if room.is_empty() || room.len() > MAX_ROOM_LEN {
                reject(tx, "bad_room", "room must be 1..=64 characters");
                return Ok(());
            }
            (room, client, clean_name(&name))
        }
        _ => {
            reject(tx, "expected_hello", "first message must be hello");
            return Ok(());
        }
    };

    // Join. Every peer is equal now, so nobody is ever turned away for the role
    // it wants; it is simply brought up to date with the room.
    let peers = {
        let mut guard = rooms.lock().await;
        let room = guard.entry(room_name.clone()).or_default();
        room.peers.insert(
            id,
            Peer {
                tx: tx.clone(),
                share: None,
                name,
            },
        );
        // Someone is listening again, so the room is no longer up for reaping.
        room.empty_since = None;
        let peers = room.peers.len();

        for (other_id, peer) in &room.peers {
            if *other_id != id {
                let _ = peer.tx.send(encode(&ServerMsg::Peer {
                    joined: true,
                    peers,
                }));
            }
        }

        let _ = tx.send(encode(&ServerMsg::Welcome {
            protocol: PROTOCOL_VERSION,
            server_ms: unix_ms(),
            peers,
            you: id,
        }));
        // Everybody, the newcomer included, gets the list with the newcomer in it.
        room.publish_roster();
        // Who can serve cached tracks, before anything that could start a load:
        // the state below is what makes a peer fetch a track, and by then it
        // should already know whether a neighbour has it. Sent even when empty,
        // so joining an idle room means "the map is empty", not "no map yet".
        let _ = tx.send(encode(&ServerMsg::Shares {
            peers: room.shares(),
        }));
        // Then hand the newcomer the room as it stands. The queue goes before
        // the state so the state's `index` already means something on arrival.
        if !room.state.queue.is_empty() {
            let _ = tx.send(encode(&ServerMsg::Queue {
                revision: room.state.queue_revision,
                tracks: room.state.queue.clone(),
            }));
            let _ = tx.send(encode(&ServerMsg::State {
                state: room.state.snapshot(),
            }));
        }
        peers
    };
    info!(peer = id, %addr, room = %room_name, client = %client, peers, "joined");

    let outcome = pump(ws_rx, tx, id, addr, &room_name, rooms).await;

    // Leave.
    let mut guard = rooms.lock().await;
    if let Some(room) = guard.get_mut(&room_name) {
        // A peer that has gone cannot serve anything, and its endpoint is stale
        // the moment its socket closes.
        let was_sharing = room
            .peers
            .remove(&id)
            .is_some_and(|peer| peer.share.is_some());

        // The wave outlives its feeder. Dropping the claim outright used to mean
        // the room played out what was queued and then sat in silence for good —
        // and rejoining did not help, because nothing said the wave was still
        // meant to be on. The station stays, marked as needing a feeder, and the
        // next peer to notice picks it up.
        let orphaned_station = match &mut room.state.station {
            Some((_, owner)) if *owner == Some(id) => {
                *owner = None;
                room.state.seq += 1;
                true
            }
            _ => false,
        };
        if orphaned_station {
            info!(peer = id, room = %room_name, "station feeder left; wave waits for another");
        }

        let peers = room.peers.len();
        room.tell_everyone(&ServerMsg::Peer {
            joined: false,
            peers,
        });
        room.publish_roster();
        if orphaned_station {
            room.publish(Changed::STATE);
        }
        if was_sharing {
            room.publish_shares();
        }

        // The room outlives its last listener: someone who queued «Мне
        // нравится», closed the app and came back would otherwise find an empty
        // room. Protocol 2 could delete it here because the relay held nothing;
        // now the queue lives here and is worth keeping.
        //
        // The playhead is frozen on the way out, or the ticker below would run
        // the whole queue down while nobody is listening, and the room would be
        // somewhere else entirely by the time anyone returns.
        if room.peers.is_empty() {
            if room.state.playing {
                room.state.position_ms = room.state.derived_position_ms(unix_ms());
                room.state.playing = false;
                room.state.at_server_ms = unix_ms();
                room.state.seq += 1;
            }
            room.empty_since = Some(std::time::Instant::now());
            info!(room = %room_name, "room is empty; queue kept, playhead frozen");
        }
    }
    info!(peer = id, room = %room_name, "left");

    outcome
}

async fn pump(
    ws_rx: &mut WsRead,
    tx: &mpsc::UnboundedSender<Message>,
    id: u64,
    addr: SocketAddr,
    room_name: &str,
    rooms: &Rooms,
) -> Result<()> {
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, next_client_msg(ws_rx)).await {
            Err(_) => {
                debug!(peer = id, "idle timeout");
                return Ok(());
            }
            Ok(None) => return Ok(()),
            Ok(Some(msg)) => msg,
        };

        match msg {
            // Answered inline rather than through the room, so the timestamp is
            // taken as close as possible to the read.
            ClientMsg::TimeReq { c0 } => {
                let _ = tx.send(encode(&ServerMsg::TimeRes { c0, s: unix_ms() }));
            }
            ClientMsg::Do { command } => {
                let station_claim = matches!(command, Command::SetStation { .. });
                let mut guard = rooms.lock().await;
                let Some(room) = guard.get_mut(room_name) else {
                    continue;
                };
                match room.state.apply(command, id, unix_ms()) {
                    Ok(changed) => {
                        room.publish(changed);
                        if station_claim {
                            // Only the claimant needs to know it won: it is the
                            // one that has to start feeding.
                            let _ = tx.send(encode(&ServerMsg::Station {
                                id: room.state.station.as_ref().map(|(id, _)| id.clone()),
                                yours: true,
                            }));
                        }
                    }
                    Err(code) => {
                        debug!(peer = id, code, "command rejected");
                        if station_claim {
                            let _ = tx.send(encode(&ServerMsg::Station {
                                id: room.state.station.as_ref().map(|(id, _)| id.clone()),
                                yours: false,
                            }));
                        }
                        reject(tx, code, error_message(code));
                    }
                }
            }
            ClientMsg::Share { port, tracks } => {
                if tracks.len() > MAX_SHARED {
                    reject(tx, "too_many_shares", error_message("too_many_shares"));
                    continue;
                }
                let mut guard = rooms.lock().await;
                let Some(room) = guard.get_mut(room_name) else {
                    continue;
                };
                let offer = port.map(|port| PeerShare {
                    peer: id,
                    endpoint: endpoint_for(addr, port),
                    tracks,
                });
                let Some(peer) = room.peers.get_mut(&id) else {
                    continue;
                };
                // Peers re-announce whenever their cache moves, and an
                // announcement that says nothing new must not put a frame on
                // every other peer's socket.
                if peer.share == offer {
                    continue;
                }
                debug!(
                    peer = id,
                    serving = offer.as_ref().map_or(0, |o| o.tracks.len()),
                    "sharing offer updated"
                );
                peer.share = offer;
                room.publish_shares();
            }
            ClientMsg::SetName { name } => {
                let name = clean_name(&name);
                let mut guard = rooms.lock().await;
                let Some(room) = guard.get_mut(room_name) else {
                    continue;
                };
                let Some(peer) = room.peers.get_mut(&id) else {
                    continue;
                };
                if peer.name == name {
                    continue;
                }
                peer.name = name;
                room.publish_roster();
            }
            ClientMsg::Bye => return Ok(()),
            ClientMsg::Hello { .. } => {
                debug!(peer = id, "ignoring duplicate hello");
            }
        }
    }
}

/// Reads until the next decodable [`ClientMsg`], skipping frames we do not use.
/// Returns `None` when the peer goes away.
async fn next_client_msg(ws_rx: &mut WsRead) -> Option<ClientMsg> {
    loop {
        match ws_rx.next().await? {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(text.as_str()) {
                Ok(msg) => return Some(msg),
                Err(err) => debug!(%err, "undecodable frame"),
            },
            Ok(Message::Close(_)) => return None,
            // Ping/Pong are answered by tungstenite itself.
            Ok(_) => {}
            Err(err) => {
                debug!(%err, "read error");
                return None;
            }
        }
    }
}

fn reject(tx: &mpsc::UnboundedSender<Message>, code: &str, message: &str) {
    let _ = tx.send(encode(&ServerMsg::Error {
        code: code.to_string(),
        message: message.to_string(),
    }));
}

/// Pairs the address a peer's own connection arrives on with the port it says
/// its file server listens on.
///
/// The peer never names an address itself, and this is why: a machine with a VPN
/// adapter, a Hyper-V switch and Wi-Fi has several, and picking the one the rest
/// of the room can actually reach is guesswork from the inside. From here it is
/// not a guess — the socket carrying this very announcement came from it.
///
/// Two consequences worth knowing. A peer that reached the relay over loopback
/// gets a loopback endpoint, which is right for two players on one machine and
/// useless for anybody else — so an app hosting a room dials its own LAN
/// address rather than `127.0.0.1`. And a link-local IPv6 peer gets an endpoint
/// without its zone index, since a URL has nowhere to put one; on such a network
/// IPv4 is the path that works.
fn endpoint_for(addr: SocketAddr, port: u16) -> String {
    // Formatting a `SocketAddr` rather than the bare IP is what brackets an IPv6
    // literal, which a URL requires.
    format!("http://{}", SocketAddr::new(addr.ip(), port))
}

fn encode(msg: &ServerMsg) -> Message {
    // ServerMsg is plain data, so this cannot fail in practice; sending an
    // empty object keeps the signature infallible rather than panicking.
    Message::text(serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, duration_ms: u64) -> TrackRef {
        TrackRef {
            track_id: id.into(),
            album_id: None,
            album: None,
            title: format!("t{id}"),
            artist: "a".into(),
            duration_ms,
            cover_uri: None,
        }
    }

    /// Rooms as the discovery answer sees them: only membership matters, so the
    /// peers can be empty maps.
    async fn rooms_named(names: &[&str]) -> Rooms {
        let mut map = HashMap::new();
        for name in names {
            map.insert((*name).to_string(), Room::default());
        }
        Arc::new(Mutex::new(map))
    }

    fn room_with_two_tracks(now: i64) -> RoomState {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetQueue {
                    tracks: vec![track("1", 10_000), track("2", 20_000)],
                    start: 0,
                },
                1,
                now,
            )
            .unwrap();
        state
    }

    #[test]
    fn setting_a_queue_starts_playing_it() {
        let state = room_with_two_tracks(1_000);
        assert_eq!(state.index, 0);
        assert!(state.playing);
        assert_eq!(state.position_ms, 0);
        assert_eq!(state.at_server_ms, 1_000);
        assert_eq!(state.queue_revision, 1);
    }

    #[test]
    fn commands_from_different_peers_share_one_sequence() {
        let mut state = room_with_two_tracks(1_000);
        let after_queue = state.seq;
        state.apply(Command::Pause, 7, 2_000).unwrap();
        state.apply(Command::Resume, 9, 3_000).unwrap();
        assert_eq!(state.seq, after_queue + 2, "every peer bumps the same seq");
    }

    #[test]
    fn pause_freezes_the_derived_position() {
        let mut state = room_with_two_tracks(1_000);
        state.apply(Command::Pause, 1, 4_000).unwrap();
        assert!(!state.playing);
        assert_eq!(state.position_ms, 3_000);
        assert_eq!(state.derived_position_ms(60_000), 3_000);
    }

    #[test]
    fn resume_keeps_the_position_and_re_anchors() {
        let mut state = room_with_two_tracks(1_000);
        state.apply(Command::Pause, 1, 4_000).unwrap();
        state.apply(Command::Resume, 2, 9_000).unwrap();
        assert_eq!(state.position_ms, 3_000);
        assert_eq!(state.at_server_ms, 9_000);
        assert_eq!(state.derived_position_ms(10_000), 4_000);
    }

    #[test]
    fn pausing_twice_changes_nothing_the_second_time() {
        let mut state = room_with_two_tracks(1_000);
        state.apply(Command::Pause, 1, 4_000).unwrap();
        let seq = state.seq;
        assert_eq!(state.apply(Command::Pause, 2, 5_000), Ok(Changed::NOTHING));
        assert_eq!(state.seq, seq, "a no-op must not bump seq");
    }

    #[test]
    fn seek_is_clamped_to_the_track() {
        let mut state = room_with_two_tracks(1_000);
        state
            .apply(Command::Seek { position_ms: 99_000 }, 1, 2_000)
            .unwrap();
        assert_eq!(state.position_ms, 10_000);
    }

    #[test]
    fn enqueue_appends_without_disturbing_playback() {
        let mut state = room_with_two_tracks(1_000);
        let changed = state
            .apply(
                Command::Enqueue {
                    tracks: vec![track("3", 30_000)],
                },
                2,
                5_000,
            )
            .unwrap();
        assert_eq!(changed, Changed::BOTH);
        assert_eq!(state.queue.len(), 3);
        assert_eq!(state.index, 0, "what was playing keeps playing");
        assert_eq!(state.at_server_ms, 1_000, "the anchor is left alone");
    }

    #[test]
    fn enqueue_into_an_empty_room_starts_playing() {
        let mut state = RoomState::default();
        state
            .apply(
                Command::Enqueue {
                    tracks: vec![track("1", 10_000)],
                },
                1,
                500,
            )
            .unwrap();
        assert!(state.playing);
        assert_eq!(state.index, 0);
    }

    #[test]
    fn enqueue_picks_up_a_queue_that_played_itself_out() {
        let mut state = room_with_two_tracks(0);
        // Run both tracks to the end.
        assert!(state.advance_if_ended(10_000));
        assert!(state.advance_if_ended(30_000));
        assert!(!state.playing, "the queue is spent");

        state
            .apply(
                Command::Enqueue {
                    tracks: vec![track("3", 30_000)],
                },
                2,
                40_000,
            )
            .unwrap();
        assert!(state.playing, "a station batch must not land in silence");
        assert_eq!(state.index, 2);
        assert_eq!(state.position_ms, 0);
    }

    #[test]
    fn a_track_ends_into_the_next_one() {
        let mut state = room_with_two_tracks(0);
        assert!(!state.advance_if_ended(9_999));
        assert!(state.advance_if_ended(10_000));
        assert_eq!(state.index, 1);
        assert_eq!(state.position_ms, 0);
        assert!(state.playing);
    }

    #[test]
    fn the_last_track_stops_on_itself() {
        let mut state = room_with_two_tracks(0);
        state.advance_if_ended(10_000);
        assert!(state.advance_if_ended(30_000));
        assert!(!state.playing);
        assert_eq!(state.index, 1, "we stay on the last track, not past it");
        assert_eq!(state.position_ms, 20_000);
        assert!(state.played_out());
    }

    fn ids(state: &RoomState) -> Vec<&str> {
        state.queue.iter().map(|t| t.track_id.as_str()).collect()
    }

    fn room_of(count: usize) -> RoomState {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetQueue {
                    tracks: (1..=count).map(|i| track(&i.to_string(), 10_000)).collect(),
                    start: 0,
                },
                1,
                0,
            )
            .unwrap();
        state
    }

    #[test]
    fn play_next_goes_right_after_the_current_track_without_interrupting_it() {
        let mut state = room_of(3);
        state.apply(Command::Next, 1, 1_000).unwrap(); // now on «2», anchored at 1 000
        let changed = state
            .apply(Command::PlayNext { tracks: vec![track("x", 5_000), track("y", 5_000)] }, 1, 2_000)
            .unwrap();
        assert_eq!(ids(&state), ["1", "2", "x", "y", "3"]);
        assert_eq!(state.index, 1, "what plays stays put");
        assert!(changed.queue);
        // The playhead's anchor is untouched: «2» keeps playing from where it was.
        assert!(state.playing);
        assert_eq!(state.position_ms, 0);
        assert_eq!(state.at_server_ms, 1_000);
    }

    #[test]
    fn play_next_on_an_empty_room_simply_plays() {
        let mut state = RoomState::default();
        state.apply(Command::PlayNext { tracks: vec![track("x", 5_000)] }, 1, 0).unwrap();
        assert_eq!(ids(&state), ["x"]);
        assert!(state.playing);
    }

    #[test]
    fn repeat_one_plays_the_same_track_again() {
        let mut state = room_with_two_tracks(0);
        state.apply(Command::SetRepeat { mode: RepeatMode::One }, 1, 0).unwrap();
        assert!(state.advance_if_ended(10_000));
        assert_eq!(state.index, 0);
        assert!(state.playing);
        assert_eq!(state.position_ms, 0);
        // A skip is still a skip.
        state.apply(Command::Next, 1, 11_000).unwrap();
        assert_eq!(state.index, 1);
    }

    #[test]
    fn repeat_all_goes_round_after_the_last_track() {
        let mut state = room_with_two_tracks(0);
        state.apply(Command::SetRepeat { mode: RepeatMode::All }, 1, 0).unwrap();
        state.advance_if_ended(10_000);
        assert!(state.advance_if_ended(30_000));
        assert_eq!(state.index, 0, "back to the first track");
        assert!(state.playing);
        // And «next» on the last track goes round too, instead of refusing.
        state.apply(Command::Next, 1, 31_000).unwrap();
        state.apply(Command::Next, 1, 32_000).unwrap();
        assert_eq!(state.index, 0);
    }

    #[test]
    fn the_modes_reach_everybody_in_the_snapshot() {
        let mut state = room_with_two_tracks(0);
        state.apply(Command::SetRepeat { mode: RepeatMode::All }, 1, 0).unwrap();
        state.apply(Command::SetShuffle { on: true }, 1, 0).unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.repeat, RepeatMode::All);
        assert!(snapshot.shuffle);
        // Setting what is already set changes nothing and says nothing.
        assert_eq!(state.apply(Command::SetRepeat { mode: RepeatMode::All }, 1, 0), Ok(Changed::NOTHING));
    }

    #[test]
    fn shuffle_reorders_only_what_is_still_to_come() {
        let mut state = room_of(30);
        state.apply(Command::PlayIndex { index: 4 }, 1, 0).unwrap();
        let before: Vec<String> = state.queue.iter().map(|t| t.track_id.clone()).collect();

        let changed = state.apply(Command::SetShuffle { on: true }, 1, 0).unwrap();
        assert!(changed.queue);

        // Played and playing tracks are where they were.
        assert_eq!(ids(&state)[..5], ["1", "2", "3", "4", "5"]);
        assert_eq!(state.index, 4);
        // The rest are the same tracks, very probably not in the same order.
        let mut after: Vec<String> = state.queue.iter().map(|t| t.track_id.clone()).collect();
        assert_ne!(after, before, "25 tracks shuffled into their own order is vanishingly unlikely");
        after.sort();
        let mut sorted = before.clone();
        sorted.sort();
        assert_eq!(after, sorted, "nothing lost, nothing doubled");
    }

    #[test]
    fn tracks_added_while_shuffled_land_among_the_upcoming_ones() {
        let mut state = room_of(3);
        state.apply(Command::SetShuffle { on: true }, 1, 0).unwrap();
        state
            .apply(Command::Enqueue { tracks: (0..20).map(|i| track(&format!("n{i}"), 1)).collect() }, 1, 0)
            .unwrap();
        assert_eq!(state.queue.len(), 23);
        assert_eq!(state.queue[0].track_id, "1", "the playing track is not moved");
        // Not all twenty lined up at the end, as an ordinary enqueue would.
        let tail: Vec<&str> = ids(&state)[3..].to_vec();
        assert!(tail.iter().any(|id| !id.starts_with('n')) || ids(&state)[1..3].iter().any(|id| id.starts_with('n')));
    }

    #[test]
    fn next_at_the_end_is_refused_rather_than_silently_ignored() {
        let mut state = room_with_two_tracks(0);
        state.apply(Command::Next, 1, 1_000).unwrap();
        assert_eq!(state.apply(Command::Next, 1, 2_000), Err("end_of_queue"));
    }

    #[test]
    fn prev_at_the_top_restarts_the_track() {
        let mut state = room_with_two_tracks(0);
        state.apply(Command::Seek { position_ms: 5_000 }, 1, 1_000).unwrap();
        state.apply(Command::Prev, 1, 2_000).unwrap();
        assert_eq!(state.index, 0);
        assert_eq!(state.position_ms, 0);
    }

    #[test]
    fn an_explicit_queue_ends_the_station() {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                1,
                0,
            )
            .unwrap();
        assert!(state.station.is_some());
        state
            .apply(
                Command::SetQueue {
                    tracks: vec![track("1", 1_000)],
                    start: 0,
                },
                1,
                0,
            )
            .unwrap();
        assert!(state.station.is_none(), "asking for tracks ends the wave");
    }

    #[test]
    fn a_second_peer_cannot_take_the_station() {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                1,
                0,
            )
            .unwrap();
        assert_eq!(
            state.apply(
                Command::SetStation {
                    id: Some("wave".into())
                },
                2,
                0
            ),
            Err("station_taken")
        );
    }

    #[test]
    fn the_owner_may_restate_and_release_its_station() {
        let mut state = RoomState::default();
        let claim = Command::SetStation {
            id: Some("wave".into()),
        };
        state.apply(claim.clone(), 1, 0).unwrap();
        state.apply(claim, 1, 0).unwrap();
        state.apply(Command::SetStation { id: None }, 1, 0).unwrap();
        assert!(state.station.is_none());
    }

    #[test]
    fn releasing_someone_elses_station_does_nothing() {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                1,
                0,
            )
            .unwrap();
        assert_eq!(
            state.apply(Command::SetStation { id: None }, 2, 0),
            Ok(Changed::NOTHING)
        );
        assert!(state.station.is_some());
    }

    /// The state a room is left in when its feeder disconnects, and what any peer
    /// may then do about it: the wave is still on, so somebody has to be able to
    /// take over resolving its batches.
    #[test]
    fn a_station_without_a_feeder_is_free_to_take() {
        let mut state = RoomState::default();
        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                1,
                0,
            )
            .unwrap();
        assert!(!state.snapshot().station_unfed);

        // What the leave path does when the feeder's socket closes.
        state.station = Some(("wave".to_string(), None));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.station.as_deref(), Some("wave"));
        assert!(snapshot.station_unfed, "the room still follows the wave");

        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                2,
                0,
            )
            .expect("a wave nobody feeds is not taken");
        assert_eq!(state.station, Some(("wave".to_string(), Some(2))));
        assert!(!state.snapshot().station_unfed);
    }

    /// Only whoever actually feeds the wave may switch it off. A peer that has
    /// just been refused the claim also sends `None` on its way out, and that
    /// must not turn the wave off for the room.
    #[test]
    fn a_peer_that_never_fed_the_station_cannot_switch_it_off() {
        let mut state = RoomState {
            station: Some(("wave".to_string(), None)),
            ..RoomState::default()
        };
        assert_eq!(
            state.apply(Command::SetStation { id: None }, 7, 0),
            Ok(Changed::NOTHING)
        );
        assert!(state.station.is_some(), "an unfed wave is still on");
    }

    #[test]
    fn an_empty_room_refuses_what_needs_a_track() {
        let mut state = RoomState::default();
        assert_eq!(state.apply(Command::Next, 1, 0), Err("empty_queue"));
        assert_eq!(state.apply(Command::Prev, 1, 0), Err("empty_queue"));
        assert_eq!(state.apply(Command::Resume, 1, 0), Err("empty_queue"));
        assert_eq!(
            state.apply(Command::Seek { position_ms: 1 }, 1, 0),
            Err("empty_queue")
        );
        assert_eq!(
            state.apply(Command::PlayIndex { index: 0 }, 1, 0),
            Err("no_such_index")
        );
        assert_eq!(
            state.apply(
                Command::SetQueue {
                    tracks: vec![],
                    start: 0
                },
                1,
                0
            ),
            Err("empty_queue")
        );
    }

    #[test]
    fn the_snapshot_carries_the_current_track_and_station() {
        let mut state = room_with_two_tracks(1_000);
        state
            .apply(
                Command::SetStation {
                    id: Some("wave".into()),
                },
                1,
                1_000,
            )
            .unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.track.unwrap().track_id, "1");
        assert_eq!(snapshot.index, 0);
        assert_eq!(snapshot.station.as_deref(), Some("wave"));
    }

    #[test]
    fn secret_comparison_accepts_only_the_exact_token() {
        assert!(secret_eq("hunter2", "hunter2"));
        assert!(!secret_eq("hunter2", "hunter3"));
        assert!(!secret_eq("hunter2", "hunter"));
        assert!(!secret_eq("", "x"));
    }

    /// The peer announces a port; the address comes from the socket the relay is
    /// already holding, so a machine with several interfaces never has to guess
    /// which one the room can reach.
    #[test]
    fn an_endpoint_is_the_observed_address_plus_the_announced_port() {
        let seen: SocketAddr = "192.168.1.10:54321".parse().unwrap();
        assert_eq!(endpoint_for(seen, 8788), "http://192.168.1.10:8788");
    }

    /// A URL needs an IPv6 literal in brackets, which is why the whole
    /// `SocketAddr` is formatted rather than the address and port separately.
    #[test]
    fn an_ipv6_endpoint_is_bracketed() {
        let seen: SocketAddr = "[2001:db8::5]:54321".parse().unwrap();
        assert_eq!(endpoint_for(seen, 8788), "http://[2001:db8::5]:8788");
    }

    fn peer_offering(id: u64, tracks: &[&str]) -> Peer {
        let (tx, _rx) = mpsc::unbounded_channel();
        Peer {
            tx,
            share: Some(PeerShare {
                peer: id,
                endpoint: format!("http://10.0.0.{id}:8788"),
                tracks: tracks.iter().map(|t| t.to_string()).collect(),
            }),
            name: String::new(),
        }
    }

    #[test]
    fn only_peers_that_offer_something_appear_in_the_map() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut room = Room::default();
        room.peers.insert(1, peer_offering(1, &["10", "11"]));
        room.peers.insert(2, Peer { tx, share: None, name: String::new() });

        let shares = room.shares();
        assert_eq!(shares.len(), 1, "a peer with no cache offers nothing");
        assert_eq!(shares[0].peer, 1);
        assert_eq!(shares[0].tracks, ["10", "11"]);
    }

    /// `HashMap` iteration order varies between runs, so an unordered map would
    /// produce a different frame every time and every peer would read it as a
    /// change.
    #[test]
    fn the_map_is_ordered_by_peer() {
        let mut room = Room::default();
        for id in [7, 2, 5] {
            room.peers.insert(id, peer_offering(id, &["1"]));
        }
        let ids: Vec<u64> = room.shares().into_iter().map(|s| s.peer).collect();
        assert_eq!(ids, [2, 5, 7]);
    }

    #[test]
    fn a_room_with_no_offers_has_an_empty_map() {
        assert!(Room::default().shares().is_empty());
    }

    /// Two searches in a row must produce the same list, so the answer is sorted
    /// rather than left in `HashMap` order.
    #[tokio::test]
    async fn the_discovery_answer_names_rooms_in_order() {
        let rooms = rooms_named(&["кухня", "home", "гараж"]).await;
        let names: Vec<String> = room_list(&rooms)
            .await
            .into_iter()
            .map(|brief| brief.name)
            .collect();
        assert_eq!(names, ["home", "гараж", "кухня"]);
    }

    #[tokio::test]
    async fn the_discovery_answer_counts_listeners() {
        let rooms = rooms_named(&["home"]).await;
        {
            let mut guard = rooms.lock().await;
            let room = guard.get_mut("home").unwrap();
            for id in 1..=3 {
                let (tx, _rx) = mpsc::unbounded_channel();
                room.peers.insert(id, Peer { tx, share: None, name: String::new() });
            }
        }
        let list = room_list(&rooms).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].listeners, 3);
    }

    /// An empty room is still worth naming: its queue is what somebody coming
    /// back would rejoin.
    #[tokio::test]
    async fn a_room_nobody_is_in_is_still_announced() {
        let rooms = rooms_named(&["home"]).await;
        let list = room_list(&rooms).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].listeners, 0);
    }

    /// The whole answer has to fit in one datagram, so the list is capped.
    #[tokio::test]
    async fn the_answer_is_capped() {
        let names: Vec<String> = (0..MAX_ROOMS_IN_REPLY + 10)
            .map(|i| format!("room-{i:03}"))
            .collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        let rooms = rooms_named(&borrowed).await;
        assert_eq!(room_list(&rooms).await.len(), MAX_ROOMS_IN_REPLY);
    }

    /// A room bound to loopback answers nobody: there is no one out there who
    /// could connect to it anyway, and claiming otherwise would put an
    /// unreachable address in somebody's list.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_loopback_room_is_not_discoverable() {
        let server = bind_with("127.0.0.1:0", "secret", true)
            .await
            .expect("bind");
        assert!(!server.discoverable());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_can_be_turned_off() {
        let server = bind_with("127.0.0.1:0", "secret", false)
            .await
            .expect("bind");
        assert!(!server.discoverable());
    }
}
