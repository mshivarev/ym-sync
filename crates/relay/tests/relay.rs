//! End-to-end tests against the real relay binary.
//!
//! These need neither a Yandex token nor an audio device, so they cover the
//! whole transport path — handshake, authentication, clock echo, command
//! fan-out, state replay on join — in CI or on a machine with no sound card.

use std::process::{Child, Command as OsCommand, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use ymsync_proto::{ClientMsg, Command, PROTOCOL_VERSION, ServerMsg, TrackRef, unix_ms};

const TOKEN: &str = "integration-token";
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Kills the relay even if a test panics.
struct Relay {
    child: Child,
    url: String,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Relay {
    async fn start() -> Relay {
        // Ask the OS for a free port, then hand it to the relay.
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
            probe.local_addr().expect("local addr").port()
        };
        let bind = format!("127.0.0.1:{port}");

        let child = OsCommand::new(env!("CARGO_BIN_EXE_ymsync-relay"))
            .args(["--bind", &bind, "--token", TOKEN])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the relay");

        let relay = Relay {
            child,
            url: format!("ws://{bind}"),
        };

        for _ in 0..100 {
            if tokio_tungstenite::connect_async(&relay.url).await.is_ok() {
                return relay;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the relay never started listening on {bind}");
    }

    async fn join_with(&self, token: &str) -> Ws {
        let (mut ws, _) = tokio_tungstenite::connect_async(&self.url)
            .await
            .expect("connect to the relay");
        send(
            &mut ws,
            &ClientMsg::Hello {
                protocol: PROTOCOL_VERSION,
                room: "test-room".to_string(),
                token: token.to_string(),
                client: "integration".to_string(),
            },
        )
        .await;
        ws
    }

    /// Joins and swallows the welcome, which every test would otherwise repeat.
    async fn join(&self) -> Ws {
        let mut ws = self.join_with(TOKEN).await;
        match recv(&mut ws).await {
            ServerMsg::Welcome { protocol, .. } => assert_eq!(protocol, PROTOCOL_VERSION),
            other => panic!("expected a welcome, got {other:?}"),
        }
        ws
    }
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).expect("encode");
    ws.send(Message::text(text)).await.expect("send");
}

async fn command(ws: &mut Ws, command: Command) {
    send(ws, &ClientMsg::Do { command }).await;
}

/// Next decodable server message, ignoring frames the tests do not care about.
async fn recv(ws: &mut Ws) -> ServerMsg {
    let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("timed out waiting for the relay")
            .expect("the relay closed the connection")
            .expect("read error");
        if let Message::Text(text) = frame {
            return serde_json::from_str(text.as_str()).expect("decode server message");
        }
    }
}

/// Membership notices arrive whenever anyone comes or goes, which is noise for
/// most of these tests.
async fn recv_ignoring_peers(ws: &mut Ws) -> ServerMsg {
    loop {
        match recv(ws).await {
            ServerMsg::Peer { .. } => continue,
            other => return other,
        }
    }
}

/// Proves nothing is queued up for this socket: a probe sent now comes back
/// first, so anything the relay had to say would have arrived before it.
async fn assert_quiet(ws: &mut Ws, tag: i64) {
    send(ws, &ClientMsg::TimeReq { c0: tag }).await;
    match recv_ignoring_peers(ws).await {
        ServerMsg::TimeRes { c0, .. } => assert_eq!(c0, tag),
        other => panic!("expected silence, got {other:?}"),
    }
}

fn track(id: &str) -> TrackRef {
    TrackRef {
        track_id: id.to_string(),
        album_id: Some("7".to_string()),
        title: format!("Title {id}"),
        artist: "Artist".to_string(),
        duration_ms: 200_000,
    }
}

fn set_queue(ids: &[&str]) -> Command {
    Command::SetQueue {
        tracks: ids.iter().map(|id| track(id)).collect(),
        start: 0,
    }
}

#[tokio::test]
async fn a_valid_client_is_welcomed() {
    let relay = Relay::start().await;
    let mut ws = relay.join_with(TOKEN).await;

    match recv(&mut ws).await {
        ServerMsg::Welcome { protocol, peers, .. } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(peers, 1);
        }
        other => panic!("expected a welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn a_wrong_token_is_rejected() {
    let relay = Relay::start().await;
    let mut ws = relay.join_with("not-the-token").await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "unauthorized"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_protocol_mismatch_is_reported() {
    let relay = Relay::start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&relay.url)
        .await
        .expect("connect");
    send(
        &mut ws,
        &ClientMsg::Hello {
            protocol: PROTOCOL_VERSION + 1,
            room: "test-room".to_string(),
            token: TOKEN.to_string(),
            client: "integration".to_string(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "protocol"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn clock_probes_are_echoed_with_a_server_timestamp() {
    let relay = Relay::start().await;
    let mut ws = relay.join().await;

    let before = unix_ms();
    send(&mut ws, &ClientMsg::TimeReq { c0: 12_345 }).await;
    let after_send = unix_ms();

    match recv(&mut ws).await {
        ServerMsg::TimeRes { c0, s } => {
            assert_eq!(c0, 12_345, "c0 must come back untouched");
            // The relay stamps `s` while handling the probe, so it sits inside
            // the window we just measured (allowing for clock granularity).
            assert!(
                s >= before - 1_000 && s <= after_send + 5_000,
                "server stamp {s} outside [{before}, {after_send}]"
            );
        }
        other => panic!("expected a time response, got {other:?}"),
    }
}

/// Protocol 2 turned a second master away. Protocol 3 has no roles at all, so
/// the room simply grows.
#[tokio::test]
async fn peers_are_never_turned_away_for_wanting_to_command() {
    let relay = Relay::start().await;

    let _first = relay.join().await;
    let mut second = relay.join_with(TOKEN).await;
    match recv(&mut second).await {
        ServerMsg::Welcome { peers, .. } => assert_eq!(peers, 2),
        other => panic!("the second peer was refused: {other:?}"),
    }

    let mut third = relay.join_with(TOKEN).await;
    match recv(&mut third).await {
        ServerMsg::Welcome { peers, .. } => assert_eq!(peers, 3),
        other => panic!("the third peer was refused: {other:?}"),
    }
}

#[tokio::test]
async fn a_queue_from_one_peer_reaches_all_of_them_including_its_sender() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    command(&mut one, set_queue(&["1", "2", "3"])).await;

    for (name, ws) in [("sender", &mut one), ("other", &mut two)] {
        match recv_ignoring_peers(ws).await {
            ServerMsg::Queue { revision, tracks } => {
                assert_eq!(revision, 1, "{name}");
                assert_eq!(tracks.len(), 3, "{name}");
                assert_eq!(tracks[2].track_id, "3", "{name}");
            }
            other => panic!("{name} expected a queue, got {other:?}"),
        }
        match recv_ignoring_peers(ws).await {
            ServerMsg::State { state } => {
                assert_eq!(state.index, 0, "{name}");
                assert!(state.playing, "{name}");
                assert_eq!(state.track.expect("track").track_id, "1", "{name}");
                assert_eq!(state.queue_revision, 1, "{name}");
            }
            other => panic!("{name} expected state, got {other:?}"),
        }
    }
}

/// The point of protocol 3: the peer that did not start the music can still
/// drive it.
#[tokio::test]
async fn any_peer_may_pause_the_room() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    command(&mut one, set_queue(&["1"])).await;
    for ws in [&mut one, &mut two] {
        let _queue = recv_ignoring_peers(ws).await;
        let _state = recv_ignoring_peers(ws).await;
    }

    // The peer that did not queue anything pauses the room.
    command(&mut two, Command::Pause).await;

    for (name, ws) in [("pauser", &mut two), ("other", &mut one)] {
        match recv_ignoring_peers(ws).await {
            ServerMsg::State { state } => assert!(!state.playing, "{name} still sees it playing"),
            other => panic!("{name} expected state, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn every_accepted_command_moves_the_sequence_on() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    command(&mut one, set_queue(&["1", "2"])).await;
    let _queue = recv_ignoring_peers(&mut one).await;
    let first = match recv_ignoring_peers(&mut one).await {
        ServerMsg::State { state } => state.seq,
        other => panic!("expected state, got {other:?}"),
    };

    // Alternating peers, to show they share one counter rather than each having
    // their own.
    command(&mut two, Command::Pause).await;
    command(&mut one, Command::Resume).await;

    let mut seen = Vec::new();
    while seen.len() < 2 {
        if let ServerMsg::State { state } = recv_ignoring_peers(&mut one).await {
            seen.push(state.seq);
        }
    }
    assert_eq!(seen, vec![first + 1, first + 2]);
}

#[tokio::test]
async fn a_joining_peer_is_given_the_room_as_it_stands() {
    let relay = Relay::start().await;
    let mut early = relay.join().await;

    command(&mut early, set_queue(&["1", "2"])).await;
    let _queue = recv_ignoring_peers(&mut early).await;
    let _state = recv_ignoring_peers(&mut early).await;
    command(&mut early, Command::Seek { position_ms: 5_000 }).await;
    let _state = recv_ignoring_peers(&mut early).await;

    // A newcomer must not have to wait for the next change to learn all this.
    let mut late = relay.join().await;
    match recv(&mut late).await {
        ServerMsg::Queue { tracks, .. } => assert_eq!(tracks.len(), 2),
        other => panic!("expected the queue on join, got {other:?}"),
    }
    match recv(&mut late).await {
        ServerMsg::State { state } => {
            assert_eq!(state.position_ms, 5_000);
            assert_eq!(state.track.expect("track").track_id, "1");
        }
        other => panic!("expected state on join, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_room_tells_a_newcomer_nothing_to_play() {
    let relay = Relay::start().await;
    let mut ws = relay.join().await;
    // No queue has ever been set, so there is nothing to replay.
    assert_quiet(&mut ws, 3).await;
}

#[tokio::test]
async fn a_refused_command_answers_only_its_sender() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    command(&mut one, set_queue(&["only"])).await;
    for ws in [&mut one, &mut two] {
        let _queue = recv_ignoring_peers(ws).await;
        let _state = recv_ignoring_peers(ws).await;
    }

    // One track in the queue, so there is nothing to skip to.
    command(&mut one, Command::Next).await;
    match recv_ignoring_peers(&mut one).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "end_of_queue"),
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_quiet(&mut two, 5).await;
}

#[tokio::test]
async fn the_station_goes_to_whoever_claims_it_first() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    command(
        &mut one,
        Command::SetStation {
            id: Some("wave".into()),
        },
    )
    .await;
    // The claimant learns it is the feeder; the state carries the live station.
    let mut told_yours = false;
    let mut told_state = false;
    while !(told_yours && told_state) {
        match recv_ignoring_peers(&mut one).await {
            ServerMsg::Station { id, yours } => {
                assert_eq!(id.as_deref(), Some("wave"));
                assert!(yours);
                told_yours = true;
            }
            ServerMsg::State { state } => {
                assert_eq!(state.station.as_deref(), Some("wave"));
                told_state = true;
            }
            other => panic!("unexpected frame for the claimant: {other:?}"),
        }
    }

    // Drain what the second peer saw of that, then let it try to take over.
    let _state = recv_ignoring_peers(&mut two).await;
    command(
        &mut two,
        Command::SetStation {
            id: Some("wave".into()),
        },
    )
    .await;

    let mut refused = false;
    let mut not_yours = false;
    while !(refused && not_yours) {
        match recv_ignoring_peers(&mut two).await {
            ServerMsg::Station { yours, .. } => {
                assert!(!yours);
                not_yours = true;
            }
            ServerMsg::Error { code, .. } => {
                assert_eq!(code, "station_taken");
                refused = true;
            }
            other => panic!("unexpected frame for the loser: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_station_claim_dies_with_the_peer_that_held_it() {
    let relay = Relay::start().await;
    let mut watcher = relay.join().await;

    {
        let mut feeder = relay.join().await;
        command(
            &mut feeder,
            Command::SetStation {
                id: Some("wave".into()),
            },
        )
        .await;
        match recv_ignoring_peers(&mut watcher).await {
            ServerMsg::State { state } => assert_eq!(state.station.as_deref(), Some("wave")),
            other => panic!("expected the station in state, got {other:?}"),
        }
        // Dropping the socket ends the feeder's session.
    }

    // The wave stops topping up rather than leaving the queue to run dry with
    // nobody able to refill it.
    match recv_ignoring_peers(&mut watcher).await {
        ServerMsg::State { state } => assert_eq!(state.station, None),
        other => panic!("expected the station to be dropped, got {other:?}"),
    }
}
