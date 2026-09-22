//! Joining a room that is already playing, with the real client link.
//!
//! The relay replays the room to a newcomer — the sharing map, the queue, the
//! state — and it does so the instant it accepts the connection. That is *during*
//! the client's clock handshake, and the handshake used to read those frames and
//! throw them away, which showed up in real use as an empty queue after leaving
//! and rejoining a room: the relay never resends a queue that has not changed, and
//! a room fed by a wave whose feeder has left never changes it again.
//!
//! These tests need no Yandex token, no audio device and no network beyond
//! loopback: `Link` plus an embedded relay is the whole of it.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use ymsync::link::Link;
use ymsync_proto::{Command, Event, TrackRef};

const TOKEN: &str = "rejoin-token";
const ROOM: &str = "rejoin-room";
/// Clock probes are part of connecting, so this only has to be long enough not
/// to slow the tests down.
const PROBE_EVERY: Duration = Duration::from_secs(5);
/// Generous: every test starts its own relay and the harness runs them together,
/// so a tight ceiling would measure how busy the machine is.
const WAIT: Duration = Duration::from_secs(20);

fn track(id: &str) -> TrackRef {
    TrackRef {
        track_id: id.to_string(),
        album_id: None,
        album: None,
        title: format!("Трек {id}"),
        artist: "Исполнитель".to_string(),
        duration_ms: 180_000,
        cover_uri: None,
    }
}

async fn connect(relay: &str) -> (Link, UnboundedReceiver<Event>) {
    Link::connect(relay, ROOM, TOKEN, PROBE_EVERY)
        .await
        .expect("connect to the relay")
}

/// Waits for the first event `pick` accepts, so a test can name what it is
/// waiting for instead of counting frames.
async fn wait_for<T>(
    events: &mut UnboundedReceiver<Event>,
    what: &str,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("the link died while waiting for {what}"));
        if let Some(found) = pick(event) {
            return found;
        }
    }
}

/// The bug as it was reported: put a queue in a room, leave, come back, and the
/// queue is nowhere to be seen even though the room is still playing it.
#[tokio::test]
async fn a_returning_peer_is_given_the_queue_it_left_behind() {
    let mut relay = ymsync_relay::bind("127.0.0.1:0", TOKEN)
        .await
        .expect("bind the relay");
    let url = format!("ws://{}", relay.local_addr());

    {
        let (link, mut events) = connect(&url).await;
        let _ = link.send_command(Command::SetQueue {
            tracks: vec![track("1"), track("2"), track("3")],
            start: 0,
        });
        // Wait for the relay to have taken it, so leaving cannot race the command.
        let queued = wait_for(&mut events, "the queue we just set", |event| match event {
            Event::Queue { tracks, .. } => Some(tracks.len()),
            _ => None,
        })
        .await;
        assert_eq!(queued, 3);
    }

    // Back again, on a new connection, exactly as pressing «Подключиться» twice.
    let (_link, mut events) = connect(&url).await;
    let tracks = wait_for(&mut events, "the room's queue on rejoin", |event| match event {
        Event::Queue { tracks, .. } => Some(tracks),
        _ => None,
    })
    .await;
    assert_eq!(
        tracks.iter().map(|t| t.track_id.as_str()).collect::<Vec<_>>(),
        ["1", "2", "3"],
        "the queue the room is playing has to arrive on join"
    );

    let state = wait_for(&mut events, "the room's state on rejoin", |event| match event {
        Event::State(state) => Some(state),
        _ => None,
    })
    .await;
    assert_eq!(state.track.map(|t| t.track_id), Some("1".to_string()));
    relay.stop().await;
}

/// The other half of the report: the wave was on, and after rejoining nothing
/// new ever played. The room has to say it is still following a station that
/// nobody feeds, which is what makes a client pick the feeding back up.
#[tokio::test]
async fn a_room_whose_feeder_left_says_the_wave_needs_one() {
    let mut relay = ymsync_relay::bind("127.0.0.1:0", TOKEN)
        .await
        .expect("bind the relay");
    let url = format!("ws://{}", relay.local_addr());

    {
        let (feeder, mut events) = connect(&url).await;
        let _ = feeder.send_command(Command::SetQueue {
            tracks: vec![track("1")],
            start: 0,
        });
        let _ = feeder.send_command(Command::SetStation {
            id: Some("wave".to_string()),
        });
        // The claim is confirmed to the claimant alone.
        let mine = wait_for(&mut events, "the station claim", |event| match event {
            Event::Station { yours, .. } => Some(yours),
            _ => None,
        })
        .await;
        assert!(mine, "the first peer to claim a free station gets it");
    }

    let (_link, mut events) = connect(&url).await;
    let state = wait_for(&mut events, "the room's state on rejoin", |event| match event {
        Event::State(state) => Some(state),
        _ => None,
    })
    .await;
    assert_eq!(state.station.as_deref(), Some("wave"), "the wave is still on");
    assert!(
        state.station_unfed,
        "the wave has nobody resolving batches for it"
    );
    relay.stop().await;
}

/// Whoever asks first gets the free claim, and the room stops advertising it as
/// needing a feeder — otherwise every peer in the room would keep taking it from
/// each other.
#[tokio::test]
async fn picking_up_an_unfed_wave_makes_it_fed_again() {
    let mut relay = ymsync_relay::bind("127.0.0.1:0", TOKEN)
        .await
        .expect("bind the relay");
    let url = format!("ws://{}", relay.local_addr());

    {
        let (feeder, mut events) = connect(&url).await;
        let _ = feeder.send_command(Command::SetQueue {
            tracks: vec![track("1")],
            start: 0,
        });
        let _ = feeder.send_command(Command::SetStation {
            id: Some("wave".to_string()),
        });
        wait_for(&mut events, "the station claim", |event| match event {
            Event::Station { yours: true, .. } => Some(()),
            _ => None,
        })
        .await;
    }

    let (link, mut events) = connect(&url).await;
    wait_for(&mut events, "the unfed wave", |event| match event {
        Event::State(state) if state.station_unfed => Some(()),
        _ => None,
    })
    .await;

    let _ = link.send_command(Command::SetStation {
        id: Some("wave".to_string()),
    });

    // The new state goes to the whole room and the confirmation only to the
    // claimant, in that order, so both are waited for at once rather than one
    // after the other — looking for the second would swallow the first.
    let mut claimed = false;
    let mut fed_state = None;
    while !claimed || fed_state.is_none() {
        match wait_for(&mut events, "the answer to our claim", |event| match event {
            Event::Station { yours, .. } => Some(Ok(yours)),
            Event::State(state) if state.station.is_some() && !state.station_unfed => {
                Some(Err(state))
            }
            _ => None,
        })
        .await
        {
            Ok(yours) => {
                assert!(yours, "an unfed wave is free for the taking");
                claimed = true;
            }
            Err(state) => fed_state = Some(state),
        }
    }

    let state = fed_state.expect("a state with the wave fed again");
    assert_eq!(state.station.as_deref(), Some("wave"));
    relay.stop().await;
}
