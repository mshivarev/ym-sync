//! End-to-end tests against the real relay binary.
//!
//! These need neither a Yandex token nor an audio device, so they cover the
//! whole transport path — handshake, authentication, clock echo, master-to-slave
//! fan-out — in CI or on a machine with no sound card.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use ymsync_proto::{ClientMsg, PROTOCOL_VERSION, PlaybackState, Role, ServerMsg, TrackRef, unix_ms};

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

        let child = Command::new(env!("CARGO_BIN_EXE_ymsync-relay"))
            .args(["--bind", &bind, "--token", TOKEN])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the relay");

        let relay = Relay {
            child,
            url: format!("ws://{bind}"),
        };

        for attempt in 0..100 {
            if tokio_tungstenite::connect_async(&relay.url).await.is_ok() {
                return relay;
            }
            let _ = attempt;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the relay never started listening on {bind}");
    }

    async fn join(&self, role: Role, token: &str) -> Ws {
        let (mut ws, _) = tokio_tungstenite::connect_async(&self.url)
            .await
            .expect("connect to the relay");
        send(
            &mut ws,
            &ClientMsg::Hello {
                protocol: PROTOCOL_VERSION,
                room: "test-room".to_string(),
                token: token.to_string(),
                role,
                client: "integration".to_string(),
            },
        )
        .await;
        ws
    }
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).expect("encode");
    ws.send(Message::text(text)).await.expect("send");
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

fn track(id: &str) -> TrackRef {
    TrackRef {
        track_id: id.to_string(),
        album_id: Some("7".to_string()),
        title: format!("Title {id}"),
        artist: "Artist".to_string(),
        duration_ms: 200_000,
    }
}

fn state(seq: u64, position_ms: u64, playing: bool) -> PlaybackState {
    PlaybackState {
        seq,
        track: Some(track("42")),
        index: 0,
        queue_revision: 1,
        position_ms,
        playing,
        at_server_ms: unix_ms(),
    }
}

#[tokio::test]
async fn a_valid_client_is_welcomed() {
    let relay = Relay::start().await;
    let mut ws = relay.join(Role::Master, TOKEN).await;

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
    let mut ws = relay.join(Role::Slave, "not-the-token").await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "unauthorized"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn clock_probes_are_echoed_with_a_server_timestamp() {
    let relay = Relay::start().await;
    let mut ws = relay.join(Role::Master, TOKEN).await;
    let _welcome = recv(&mut ws).await;

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

#[tokio::test]
async fn master_state_reaches_the_slave() {
    let relay = Relay::start().await;

    let mut slave = relay.join(Role::Slave, TOKEN).await;
    match recv(&mut slave).await {
        ServerMsg::Welcome { .. } => {}
        other => panic!("expected a welcome for the slave, got {other:?}"),
    }

    let mut master = relay.join(Role::Master, TOKEN).await;
    match recv(&mut master).await {
        ServerMsg::Welcome { peers, .. } => assert_eq!(peers, 2),
        other => panic!("expected a welcome for the master, got {other:?}"),
    }
    // The slave is told about the new peer.
    match recv(&mut slave).await {
        ServerMsg::Peer { role, joined, .. } => {
            assert_eq!(role, Role::Master);
            assert!(joined);
        }
        other => panic!("expected a peer notice, got {other:?}"),
    }

    send(
        &mut master,
        &ClientMsg::State {
            state: state(1, 61_000, true),
        },
    )
    .await;

    match recv(&mut slave).await {
        ServerMsg::State { state } => {
            assert_eq!(state.seq, 1);
            assert_eq!(state.position_ms, 61_000);
            assert!(state.playing);
            assert_eq!(state.track.expect("track").track_id, "42");
        }
        other => panic!("expected relayed state, got {other:?}"),
    }
}

#[tokio::test]
async fn state_from_a_slave_is_not_relayed() {
    let relay = Relay::start().await;

    let mut listener = relay.join(Role::Slave, TOKEN).await;
    let _welcome = recv(&mut listener).await;

    let mut impostor = relay.join(Role::Slave, TOKEN).await;
    let _welcome = recv(&mut impostor).await;
    // Drain the join notice so it cannot be mistaken for relayed state.
    match recv(&mut listener).await {
        ServerMsg::Peer { .. } => {}
        other => panic!("expected a peer notice, got {other:?}"),
    }

    send(
        &mut impostor,
        &ClientMsg::State {
            state: state(1, 1_000, true),
        },
    )
    .await;

    // Nothing should arrive; a clock probe proves the socket is still healthy.
    send(&mut listener, &ClientMsg::TimeReq { c0: 7 }).await;
    match recv(&mut listener).await {
        ServerMsg::TimeRes { c0, .. } => assert_eq!(c0, 7),
        other => panic!("a slave's state was relayed: {other:?}"),
    }
}

#[tokio::test]
async fn the_master_queue_reaches_the_slave() {
    let relay = Relay::start().await;

    let mut slave = relay.join(Role::Slave, TOKEN).await;
    let _welcome = recv(&mut slave).await;

    let mut master = relay.join(Role::Master, TOKEN).await;
    let _welcome = recv(&mut master).await;
    // Drain the join notice.
    match recv(&mut slave).await {
        ServerMsg::Peer { .. } => {}
        other => panic!("expected a peer notice, got {other:?}"),
    }

    send(
        &mut master,
        &ClientMsg::Queue {
            revision: 4,
            tracks: vec![track("1"), track("2"), track("3")],
        },
    )
    .await;

    match recv(&mut slave).await {
        ServerMsg::Queue { revision, tracks } => {
            assert_eq!(revision, 4);
            assert_eq!(tracks.len(), 3);
            assert_eq!(tracks[2].track_id, "3");
        }
        other => panic!("expected a relayed queue, got {other:?}"),
    }
}

#[tokio::test]
async fn a_queue_from_a_slave_is_not_relayed() {
    let relay = Relay::start().await;

    let mut listener = relay.join(Role::Slave, TOKEN).await;
    let _welcome = recv(&mut listener).await;

    let mut impostor = relay.join(Role::Slave, TOKEN).await;
    let _welcome = recv(&mut impostor).await;
    match recv(&mut listener).await {
        ServerMsg::Peer { .. } => {}
        other => panic!("expected a peer notice, got {other:?}"),
    }

    send(
        &mut impostor,
        &ClientMsg::Queue {
            revision: 1,
            tracks: vec![track("9")],
        },
    )
    .await;

    send(&mut listener, &ClientMsg::TimeReq { c0: 11 }).await;
    match recv(&mut listener).await {
        ServerMsg::TimeRes { c0, .. } => assert_eq!(c0, 11),
        other => panic!("a slave's queue was relayed: {other:?}"),
    }
}

#[tokio::test]
async fn a_second_master_is_turned_away() {
    let relay = Relay::start().await;

    let mut first = relay.join(Role::Master, TOKEN).await;
    match recv(&mut first).await {
        ServerMsg::Welcome { .. } => {}
        other => panic!("expected a welcome, got {other:?}"),
    }

    let mut second = relay.join(Role::Master, TOKEN).await;
    match recv(&mut second).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "master_exists"),
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
            role: Role::Slave,
            client: "integration".to_string(),
        },
    )
    .await;

    match recv(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "protocol"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}
