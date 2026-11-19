//! Losing the relay connection mid-song and getting it back.
//!
//! A VPN switching on or off drops every TCP connection on the machine, the one
//! to a relay this very app hosts included. The engine used to give up there and
//! close the room; now it dials again and the room carries on. In between, the
//! relay saw its only listener leave and froze the room, which the engine has to
//! undo, or the music stays stopped even though everything is connected again.
//!
//! A proxy sits between the engine and the relay so a test can cut the
//! connection the way the network does, while the relay itself stays up.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use ymsync::cache::Cache;
use ymsync::config::Config;
use ymsync::engine::{self, Command, Snapshot, Wiring};
use ymsync::likes::Likes;
use ymsync::playback::{AudioSource, Playback};
use ymsync_proto::TrackRef;

const TOKEN: &str = "relink-token";
const WAIT: Duration = Duration::from_secs(30);

/// Plays nothing, but keeps time like a player would.
#[derive(Default)]
struct FakePlayer {
    current: Mutex<Option<String>>,
    playing: Mutex<bool>,
    position: AtomicU64,
}

impl Playback for FakePlayer {
    fn needs_bytes(&self) -> bool {
        true
    }
    fn load(&self, track: TrackRef, _source: AudioSource) -> Result<()> {
        *self.current.lock().unwrap() = Some(track.track_id);
        *self.playing.lock().unwrap() = false;
        Ok(())
    }
    fn play(&self) {
        *self.playing.lock().unwrap() = true;
    }
    fn pause(&self) {
        *self.playing.lock().unwrap() = false;
    }
    fn seek_ms(&self, ms: u64) -> Result<()> {
        self.position.store(ms, Ordering::SeqCst);
        Ok(())
    }
    fn position_ms(&self) -> u64 {
        self.position.load(Ordering::SeqCst)
    }
    fn is_playing(&self) -> bool {
        *self.playing.lock().unwrap()
    }
    fn is_finished(&self) -> bool {
        false
    }
    fn current_track_id(&self) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
    fn set_volume(&self, _volume: f32) {}
    fn volume(&self) -> f32 {
        1.0
    }
}

/// Forwards to the relay, and cuts every connection when `cut` is bumped —
/// as a VPN coming up does — while still taking new ones.
async fn proxy(relay: std::net::SocketAddr, cut: watch::Receiver<u64>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else { return };
            let mut cut = cut.clone();
            cut.borrow_and_update();
            tokio::spawn(async move {
                let Ok(mut upstream) = TcpStream::connect(relay).await else { return };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                    _ = cut.changed() => {}
                }
            });
        }
    });
    addr
}

fn track(id: &str) -> TrackRef {
    TrackRef {
        track_id: id.to_string(),
        album_id: None,
        album: None,
        title: format!("Трек {id}"),
        artist: "Исполнитель".to_string(),
        duration_ms: 600_000,
        cover_uri: None,
        added_by: None,
    }
}

async fn wait_until(
    snapshots: &mut watch::Receiver<Snapshot>,
    what: &str,
    mut ok: impl FnMut(&Snapshot) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if ok(&snapshots.borrow_and_update()) {
            return;
        }
        tokio::time::timeout_at(deadline, snapshots.changed())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|_| panic!("the engine stopped while waiting for {what}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_connection_is_dialled_again_and_the_room_plays_on() {
    let dir = std::env::temp_dir().join(format!("ymsync-relink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = Arc::new(Cache::open(&dir, 0).unwrap());
    // On this disk, so the track loads with no Yandex and the room can start.
    let song = track("100");
    cache.insert(&song, &[0u8; 4096], true).unwrap();
    let likes = Likes::open(&dir);

    let relay = ymsync_relay::bind("127.0.0.1:0", TOKEN).await.unwrap();
    let (cut, cut_rx) = watch::channel(0u64);
    let via = proxy(relay.local_addr(), cut_rx).await;

    let cfg = Config {
        room_token: TOKEN.to_string(),
        room: "relink".to_string(),
        ..Config::default()
    };
    let handle = engine::spawn(
        &cfg,
        Wiring {
            api: None,
            player: Arc::new(FakePlayer::default()),
            cache,
            likes,
            relay_url: format!("ws://{via}"),
            relink_urls: None,
            share_port: None,
            hot: Default::default(),
            hosting: None,
        },
    )
    .await
    .expect("connect");
    let mut snapshots = handle.subscribe();

    handle.send(Command::SetQueue {
        tracks: vec![song.clone()],
        start: 0,
    });
    wait_until(&mut snapshots, "the room to play", |s| s.playing && s.track.is_some()).await;

    // The network goes away under the connection.
    cut.send_modify(|n| *n += 1);
    wait_until(&mut snapshots, "the loss to show", |s| !s.connected).await;

    // ...and comes back on its own, with the room playing again, not frozen.
    wait_until(&mut snapshots, "the engine to reconnect", |s| s.connected).await;
    // The relay is the judge of whether the room plays: ask it directly. The
    // player's own state would still read «playing» from before the cut.
    let (_observer, mut events) = ymsync::link::Link::connect(
        &format!("ws://{}", relay.local_addr()),
        "relink",
        TOKEN,
        "observer",
        Duration::from_secs(5),
    )
    .await
    .expect("observer connects");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timed out waiting for the room to play again")
            .expect("the observer lost the relay");
        if let ymsync_proto::Event::State(state) = event
            && state.playing
            && state.track.as_ref().is_some_and(|t| t.track_id == "100")
        {
            break;
        }
    }
    wait_until(&mut snapshots, "the player to play", |s| s.connected && s.playing).await;

    handle.send(Command::Shutdown);
    handle.join().await.expect("a clean shutdown");
    drop(relay);
    let _ = std::fs::remove_dir_all(&dir);
}
