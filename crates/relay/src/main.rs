//! Room relay: forwards one master's playback state to the slaves in its room
//! and answers clock probes.
//!
//! The relay is deliberately dumb. It keeps no playback state of its own, so
//! restarting it costs nothing: clients reconnect, re-probe the clock and carry
//! on. Its only two jobs are being a shared time reference and fanning out
//! state.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use ymsync_proto::{ClientMsg, PROTOCOL_VERSION, Role, ServerMsg, unix_ms};

/// A client that says nothing at all for this long is assumed dead. Both roles
/// probe the clock every few seconds, so this only trips on a peer that vanished
/// without closing the socket — a phone leaving Wi-Fi, or an app killed. Keep it
/// short: until it fires, that peer still counts as present.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a fresh connection has to send its `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_ROOM_LEN: usize = 64;

type WsStream = tokio_tungstenite::WebSocketStream<TcpStream>;
type WsRead = futures_util::stream::SplitStream<WsStream>;

#[derive(Parser, Debug)]
#[command(
    name = "ymsync-relay",
    about = "Relays Yandex Music playback state from a master player to its slaves"
)]
struct Args {
    /// Address to listen on. Keep it on localhost or a LAN address; put a TLS
    /// reverse proxy in front before exposing it to the internet.
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: String,

    /// Shared secret every client must present. Falls back to the
    /// YMSYNC_ROOM_TOKEN environment variable.
    #[arg(long)]
    token: Option<String>,
}

struct Peer {
    role: Role,
    tx: mpsc::UnboundedSender<Message>,
}

#[derive(Default)]
struct Room {
    peers: HashMap<u64, Peer>,
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ymsync_relay=info")),
        )
        .init();

    let args = Args::parse();
    let token = args
        .token
        .or_else(|| std::env::var("YMSYNC_ROOM_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .context(
            "не задан токен комнаты: передайте --token <секрет> или переменную \
             YMSYNC_ROOM_TOKEN. То же значение впишите игрокам в room_token",
        )?;
    let token = Arc::new(token);

    let listener = TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("cannot listen on {}", args.bind))?;
    info!(bind = %args.bind, protocol = PROTOCOL_VERSION, "relay listening");

    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
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
        tokio::spawn(async move {
            if let Err(err) = serve(stream, addr, id, rooms, token).await {
                debug!(peer = id, %addr, %err, "connection closed");
            }
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

    let (room_name, role, client) = match hello {
        ClientMsg::Hello {
            protocol,
            room,
            token: given,
            role,
            client,
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
            (room, role, client)
        }
        _ => {
            reject(tx, "expected_hello", "first message must be hello");
            return Ok(());
        }
    };

    // Join. Two masters in one room would fight over the same slaves, so the
    // second one is turned away rather than silently interleaving state.
    let peers = {
        let mut guard = rooms.lock().await;
        let master_taken = guard
            .get(&room_name)
            .is_some_and(|room| room.peers.values().any(|peer| peer.role == Role::Master));
        if role == Role::Master && master_taken {
            drop(guard);
            warn!(peer = id, %addr, room = %room_name, "rejected: room already has a master");
            reject(tx, "master_exists", "this room already has a master");
            return Ok(());
        }

        let room = guard.entry(room_name.clone()).or_default();
        room.peers.insert(
            id,
            Peer {
                role,
                tx: tx.clone(),
            },
        );
        let peers = room.peers.len();
        for (other_id, peer) in &room.peers {
            if *other_id != id {
                let _ = peer.tx.send(encode(&ServerMsg::Peer {
                    role,
                    joined: true,
                    peers,
                }));
            }
        }
        peers
    };
    info!(peer = id, %addr, room = %room_name, %role, client = %client, peers, "joined");

    let _ = tx.send(encode(&ServerMsg::Welcome {
        protocol: PROTOCOL_VERSION,
        server_ms: unix_ms(),
        peers,
    }));

    let outcome = pump(ws_rx, tx, id, role, &room_name, rooms).await;

    // Leave.
    let mut guard = rooms.lock().await;
    if let Some(room) = guard.get_mut(&room_name) {
        room.peers.remove(&id);
        let peers = room.peers.len();
        for peer in room.peers.values() {
            let _ = peer.tx.send(encode(&ServerMsg::Peer {
                role,
                joined: false,
                peers,
            }));
        }
        if room.peers.is_empty() {
            guard.remove(&room_name);
        }
    }
    info!(peer = id, room = %room_name, %role, "left");

    outcome
}

async fn pump(
    ws_rx: &mut WsRead,
    tx: &mpsc::UnboundedSender<Message>,
    id: u64,
    role: Role,
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
            ClientMsg::State { state } => {
                if role != Role::Master {
                    debug!(peer = id, "ignoring state from a slave");
                    continue;
                }
                broadcast(rooms, room_name, id, &ServerMsg::State { state }).await;
            }
            ClientMsg::Queue { revision, tracks } => {
                if role != Role::Master {
                    debug!(peer = id, "ignoring a queue from a slave");
                    continue;
                }
                broadcast(rooms, room_name, id, &ServerMsg::Queue { revision, tracks }).await;
            }
            ClientMsg::Bye => return Ok(()),
            ClientMsg::Hello { .. } => {
                debug!(peer = id, "ignoring duplicate hello");
            }
        }
    }
}

/// Delivers a frame to everyone in the room except its sender.
async fn broadcast(rooms: &Rooms, room_name: &str, sender: u64, msg: &ServerMsg) {
    let frame = encode(msg);
    let guard = rooms.lock().await;
    if let Some(room) = guard.get(room_name) {
        for (peer_id, peer) in &room.peers {
            if *peer_id != sender {
                let _ = peer.tx.send(frame.clone());
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

fn encode(msg: &ServerMsg) -> Message {
    // ServerMsg is plain data, so this cannot fail in practice; sending an
    // empty object keeps the signature infallible rather than panicking.
    Message::text(serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string()))
}

/// Length-independent-time comparison, so a wrong token leaks nothing about the
/// right one through response timing.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_comparison_accepts_only_the_exact_token() {
        assert!(secret_eq("hunter2", "hunter2"));
        assert!(!secret_eq("hunter2", "hunter3"));
        assert!(!secret_eq("hunter2", "hunter"));
        assert!(!secret_eq("hunter2", ""));
        assert!(secret_eq("", ""));
    }

    #[test]
    fn server_messages_encode_as_text_frames() {
        let msg = encode(&ServerMsg::TimeRes { c0: 1, s: 2 });
        let Message::Text(text) = msg else {
            panic!("expected a text frame");
        };
        assert_eq!(text.as_str(), r#"{"t":"time_res","c0":1,"s":2}"#);
    }
}
