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
use ymsync_proto::{
    ClientMsg, Command, PROTOCOL_VERSION, PeerShare, ServerMsg, TrackRef, unix_ms,
};

const TOKEN: &str = "integration-token";
/// Generous on purpose: every test here spawns its own relay *process*, and the
/// test harness runs them all at once, so a tight ceiling measures how busy the
/// machine is rather than how quickly the relay answers. It is still a ceiling —
/// a relay that never replies fails instead of hanging the run.
const RECV_TIMEOUT: Duration = Duration::from_secs(20);

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
        self.join_as(token, "").await
    }

    async fn join_as(&self, token: &str, name: &str) -> Ws {
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
                name: name.to_string(),
            },
        )
        .await;
        ws
    }

    /// Joins and swallows the handshake frames every test would otherwise repeat:
    /// the welcome, then the sharing map that follows it.
    async fn join(&self) -> Ws {
        let mut ws = self.join_with(TOKEN).await;
        match recv(&mut ws).await {
            ServerMsg::Welcome { protocol, .. } => assert_eq!(protocol, PROTOCOL_VERSION),
            other => panic!("expected a welcome, got {other:?}"),
        }
        match recv(&mut ws).await {
            ServerMsg::Shares { .. } => {}
            other => panic!("expected the sharing map after the welcome, got {other:?}"),
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

/// Next decodable server message, ignoring frames the tests do not care about —
/// the room's list of names among them, which goes out whenever anybody joins
/// or leaves and has tests of its own.
async fn recv(ws: &mut Ws) -> ServerMsg {
    loop {
        match recv_any(ws).await {
            ServerMsg::Roster { .. } => continue,
            other => return other,
        }
    }
}

/// Next decodable server message, whatever it is.
async fn recv_any(ws: &mut Ws) -> ServerMsg {
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
        album: Some("Album".to_string()),
        title: format!("Title {id}"),
        artist: "Artist".to_string(),
        duration_ms: 200_000,
        cover_uri: None,
    }
}

fn set_queue(ids: &[&str]) -> Command {
    Command::SetQueue {
        tracks: ids.iter().map(|id| track(id)).collect(),
        start: 0,
    }
}

/// Short enough that the relay's advance ticker would move past it within a test.
fn short_track(id: &str) -> TrackRef {
    TrackRef {
        duration_ms: 150,
        cover_uri: None,
        ..track(id)
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

/// The next list of names this socket is told, skipping everything else.
async fn recv_roster(ws: &mut Ws) -> Vec<(u64, String)> {
    loop {
        if let ServerMsg::Roster { peers } = recv_any(ws).await {
            return peers.into_iter().map(|entry| (entry.peer, entry.name)).collect();
        }
    }
}

#[tokio::test]
async fn everybody_sees_who_is_in_the_room_and_a_rename() {
    let relay = Relay::start().await;

    let mut anna = relay.join_as(TOKEN, "Аня").await;
    let anna_id = match recv_any(&mut anna).await {
        ServerMsg::Welcome { you, .. } => you,
        other => panic!("expected a welcome, got {other:?}"),
    };
    assert_eq!(recv_roster(&mut anna).await, vec![(anna_id, "Аня".to_string())]);

    // A name is trimmed and stripped of control characters on the way in.
    let mut boris = relay.join_as(TOKEN, "  Боря\n ").await;
    let boris_id = match recv_any(&mut boris).await {
        ServerMsg::Welcome { you, .. } => you,
        other => panic!("expected a welcome, got {other:?}"),
    };
    let both = vec![(anna_id, "Аня".to_string()), (boris_id, "Боря".to_string())];
    assert_eq!(recv_roster(&mut boris).await, both, "the newcomer sees everybody");
    assert_eq!(recv_roster(&mut anna).await, both, "and everybody sees the newcomer");

    send(&mut boris, &ClientMsg::SetName { name: "Борис".to_string() }).await;
    let renamed = vec![(anna_id, "Аня".to_string()), (boris_id, "Борис".to_string())];
    assert_eq!(recv_roster(&mut anna).await, renamed);

    // Leaving takes you off everybody else's list.
    drop(boris);
    assert_eq!(recv_roster(&mut anna).await, vec![(anna_id, "Аня".to_string())]);
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
            name: String::new(),
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

/// The whole point of keeping room state: queue something, walk away, come back
/// to it. Protocol 3 deleted the room with its last peer at first, which lost
/// the queue exactly when a single-listener room was closed and reopened.
#[tokio::test]
async fn a_room_keeps_its_queue_after_the_last_peer_leaves() {
    let relay = Relay::start().await;

    {
        let mut only = relay.join().await;
        command(&mut only, set_queue(&["1", "2", "3"])).await;
        let _queue = recv_ignoring_peers(&mut only).await;
        let _state = recv_ignoring_peers(&mut only).await;
        // Dropping the socket empties the room.
    }

    // Give the relay a moment to notice the socket closed.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut back = relay.join().await;
    match recv(&mut back).await {
        ServerMsg::Queue { tracks, .. } => {
            assert_eq!(tracks.len(), 3, "the queue should have outlived the peer");
            assert_eq!(tracks[2].track_id, "3");
        }
        other => panic!("expected the kept queue, got {other:?}"),
    }
    match recv(&mut back).await {
        ServerMsg::State { state } => {
            assert_eq!(state.track.expect("track").track_id, "1");
            assert!(
                !state.playing,
                "an empty room must freeze, or it plays itself out unheard"
            );
        }
        other => panic!("expected the kept state, got {other:?}"),
    }
}

/// A frozen room must not have advanced while nobody was listening.
#[tokio::test]
async fn an_empty_room_does_not_run_its_queue_down() {
    let relay = Relay::start().await;

    {
        let mut only = relay.join().await;
        // Two very short tracks: were the room still ticking, both would end
        // well inside the wait below.
        command(
            &mut only,
            Command::SetQueue {
                tracks: vec![short_track("1"), short_track("2")],
                start: 0,
            },
        )
        .await;
        let _queue = recv_ignoring_peers(&mut only).await;
        let _state = recv_ignoring_peers(&mut only).await;
    }

    tokio::time::sleep(Duration::from_millis(800)).await;

    let mut back = relay.join().await;
    let _queue = recv(&mut back).await;
    match recv(&mut back).await {
        ServerMsg::State { state } => {
            assert_eq!(state.index, 0, "the room advanced with nobody listening");
            assert!(!state.playing);
        }
        other => panic!("expected state, got {other:?}"),
    }
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

/// A wave has to outlive whoever started it, or a room whose feeder walked away
/// plays out what is queued and then sits in silence for good — which is what
/// happened in real use after leaving and rejoining a room.
#[tokio::test]
async fn a_station_survives_the_peer_that_fed_it_and_is_free_to_take() {
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
            ServerMsg::State { state } => {
                assert_eq!(state.station.as_deref(), Some("wave"));
                assert!(!state.station_unfed, "somebody is feeding it");
            }
            other => panic!("expected the station in state, got {other:?}"),
        }
        // Dropping the socket ends the feeder's session.
    }

    // The room still follows the wave; what it lacks is somebody to resolve the
    // next batch, and that is what the rest of the room is told.
    match recv_ignoring_peers(&mut watcher).await {
        ServerMsg::State { state } => {
            assert_eq!(state.station.as_deref(), Some("wave"));
            assert!(state.station_unfed, "the wave is waiting for a feeder");
        }
        other => panic!("expected an unfed station, got {other:?}"),
    }

    // And the peer that stayed can pick it up — no election, whoever asks first.
    command(
        &mut watcher,
        Command::SetStation {
            id: Some("wave".into()),
        },
    )
    .await;
    let mut fed = false;
    let mut mine = false;
    while !fed || !mine {
        match recv_ignoring_peers(&mut watcher).await {
            ServerMsg::State { state } => {
                assert_eq!(state.station.as_deref(), Some("wave"));
                fed = !state.station_unfed;
            }
            ServerMsg::Station { yours, .. } => mine = yours,
            other => panic!("expected the claim to be granted, got {other:?}"),
        }
    }
}

/// Next sharing map for this socket, skipping the frames these tests do not care
/// about.
async fn recv_shares(ws: &mut Ws) -> Vec<PeerShare> {
    match recv_ignoring_peers(ws).await {
        ServerMsg::Shares { peers } => peers,
        other => panic!("expected a sharing map, got {other:?}"),
    }
}

async fn announce(ws: &mut Ws, port: Option<u16>, tracks: &[&str]) {
    send(
        ws,
        &ClientMsg::Share {
            port,
            tracks: tracks.iter().map(|t| t.to_string()).collect(),
        },
    )
    .await;
}

/// The whole point of the brokerage: a peer says only which port it serves on,
/// and everyone learns a URL they can actually reach it at.
#[tokio::test]
async fn the_relay_turns_an_announced_port_into_a_reachable_address() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    announce(&mut one, Some(8788), &["10", "11"]).await;

    for ws in [&mut one, &mut two] {
        let shares = recv_shares(ws).await;
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].tracks, ["10", "11"]);
        // The tests connect over loopback, so that is the address the relay sees
        // — paired with the port the peer named, never one it guessed.
        assert_eq!(shares[0].endpoint, "http://127.0.0.1:8788");
    }
}

/// The sender is included, exactly as it is for a command: what the relay says is
/// the truth even for whoever caused it.
#[tokio::test]
async fn withdrawing_an_offer_empties_the_map_for_everyone() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    announce(&mut one, Some(8788), &["10"]).await;
    for ws in [&mut one, &mut two] {
        assert_eq!(recv_shares(ws).await.len(), 1);
    }

    announce(&mut one, None, &[]).await;
    for ws in [&mut one, &mut two] {
        assert!(recv_shares(ws).await.is_empty(), "the offer was withdrawn");
    }
}

/// Peers re-announce whenever their cache moves, and downloading a hundred tracks
/// one at a time must not put a hundred identical frames on every socket.
#[tokio::test]
async fn an_announcement_that_changes_nothing_is_not_rebroadcast() {
    let relay = Relay::start().await;
    let mut one = relay.join().await;
    let mut two = relay.join().await;

    announce(&mut one, Some(8788), &["10"]).await;
    assert_eq!(recv_shares(&mut two).await.len(), 1);

    announce(&mut one, Some(8788), &["10"]).await;
    assert_quiet(&mut two, 11).await;

    // A real change still goes out.
    announce(&mut one, Some(8788), &["10", "12"]).await;
    assert_eq!(recv_shares(&mut two).await[0].tracks, ["10", "12"]);
}

/// An endpoint is only good while its owner is connected; a peer that has gone
/// cannot serve anything, and would otherwise be left in the map as a URL that
/// times out.
#[tokio::test]
async fn a_departing_peer_is_dropped_from_the_map() {
    let relay = Relay::start().await;
    let mut watcher = relay.join().await;

    {
        let mut sharer = relay.join().await;
        announce(&mut sharer, Some(8788), &["10"]).await;
        assert_eq!(recv_shares(&mut watcher).await.len(), 1);
    }

    assert!(recv_shares(&mut watcher).await.is_empty());
}

/// A peer that never offered anything has nothing to withdraw, so its departure
/// must not put a pointless frame on every other socket.
#[tokio::test]
async fn a_departing_peer_that_shared_nothing_is_silent() {
    let relay = Relay::start().await;
    let mut watcher = relay.join().await;

    drop(relay.join().await);

    assert_quiet(&mut watcher, 12).await;
}

#[tokio::test]
async fn an_over_long_announcement_is_refused() {
    let relay = Relay::start().await;
    let mut ws = relay.join().await;

    let many: Vec<String> = (0..10_001).map(|n| n.to_string()).collect();
    send(
        &mut ws,
        &ClientMsg::Share {
            port: Some(8788),
            tracks: many,
        },
    )
    .await;

    match recv_ignoring_peers(&mut ws).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "too_many_shares"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// Hosting a room from inside the player is the same relay, bound in-process.
/// This is what makes offline listening work, so it is worth proving that a
/// client cannot tell the difference.
#[tokio::test]
async fn an_embedded_relay_serves_an_ordinary_client() {
    let mut server = ymsync_relay::bind("127.0.0.1:0", TOKEN)
        .await
        .expect("bind an in-process relay");
    let url = format!("ws://{}", server.local_addr());

    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect to the embedded relay");
    send(
        &mut ws,
        &ClientMsg::Hello {
            protocol: PROTOCOL_VERSION,
            room: "embedded".to_string(),
            token: TOKEN.to_string(),
            client: "integration".to_string(),
            name: String::new(),
        },
    )
    .await;
    match recv(&mut ws).await {
        ServerMsg::Welcome { protocol, peers, .. } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(peers, 1);
        }
        other => panic!("expected a welcome, got {other:?}"),
    }

    // And it really is a room: it takes a queue and answers with one.
    let _shares = recv(&mut ws).await;
    command(&mut ws, set_queue(&["1", "2"])).await;
    match recv_ignoring_peers(&mut ws).await {
        ServerMsg::Queue { tracks, .. } => assert_eq!(tracks.len(), 2),
        other => panic!("expected a queue, got {other:?}"),
    }

    server.stop().await;
}

/// Port 0 asks the OS for a free port, which is how an embedded relay keeps out
/// of the way of anything already listening.
#[tokio::test]
async fn an_embedded_relay_reports_the_port_it_was_given() {
    let server = ymsync_relay::bind("127.0.0.1:0", TOKEN).await.expect("bind");
    assert_ne!(server.local_addr().port(), 0);
}

#[tokio::test]
async fn an_embedded_relay_refuses_to_start_without_a_token() {
    assert!(ymsync_relay::bind("127.0.0.1:0", "   ").await.is_err());
}

/// Dropping the handle has to close the listener, or a front end that stops
/// hosting would leave the port occupied until the process exits.
#[tokio::test]
async fn dropping_the_handle_stops_the_listener() {
    let server = ymsync_relay::bind("127.0.0.1:0", TOKEN).await.expect("bind");
    let url = format!("ws://{}", server.local_addr());
    drop(server);

    for _ in 0..100 {
        if tokio_tungstenite::connect_async(&url).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the relay kept listening on {url} after its handle was dropped");
}
