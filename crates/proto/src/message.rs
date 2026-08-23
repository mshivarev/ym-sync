//! JSON messages exchanged between players and the relay.

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to the messages below. The relay rejects
/// clients that do not match.
///
/// * 1 — single track per state snapshot.
/// * 2 — queues: `ClientMsg::Queue` plus `index`/`queue_revision` in
///   [`PlaybackState`].
/// * 3 — the relay owns the queue and the playhead. Clients no longer publish
///   state; they send [`Command`]s and follow what comes back. Roles are gone:
///   every peer plays and every peer may command.
/// * 4 — cached tracks are shared over the local network:
///   [`ClientMsg::Share`] announces which tracks a peer will serve and on what
///   port, and [`ServerMsg::Shares`] hands the room the resulting map. The bytes
///   themselves never touch the relay.
pub const PROTOCOL_VERSION: u16 = 4;

/// Identifies a track well enough for another account to resolve it.
///
/// `track_id` is the authoritative key: it is the same numeric id on every
/// account, so a peer does not have to match on artist/title strings. The
/// human-readable fields are for logging and for the unavailable-track message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRef {
    pub track_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
}

impl std::fmt::Display for TrackRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} — {} [{}]", self.artist, self.title, self.track_id)
    }
}

/// Where one peer's cached tracks can be fetched from, as the relay sees it.
///
/// `endpoint` is composed by the relay, not announced by the peer: a machine with
/// a VPN, a Hyper-V switch and a Wi-Fi adapter cannot reliably say which of its
/// own addresses the others can reach, whereas the relay is already holding a
/// socket that demonstrably works. So a peer announces only the port its file
/// server listens on, and the relay pairs it with the address that connection
/// arrived on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerShare {
    /// The relay's id for the peer. Meaningful only within one room, and only
    /// until that peer reconnects.
    pub peer: u64,
    /// Base URL of the peer's file server, e.g. `http://192.168.1.10:8788`.
    pub endpoint: String,
    /// Track ids this peer will serve.
    pub tracks: Vec<String>,
}

/// The room's playhead, as the relay sees it.
///
/// This is an *anchor*, not a running clock: `position_ms` is where the playhead
/// stood at `at_server_ms`, and a playing room advances from there on its own.
/// So the relay only has to speak when something changes, and every peer can
/// work out where it ought to be at any instant (see
/// [`crate::sync::target_position_ms`]).
///
/// The current track is repeated here rather than only referenced by `index`, so
/// a peer whose queue copy is stale still knows what to load; the queue itself
/// arrives separately (see [`ServerMsg::Queue`]) because resending a hundred
/// tracks on every pause would be pure waste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Assigned by the relay, one higher on every accepted mutation. Lets a peer
    /// drop snapshots that arrive out of order.
    pub seq: u64,
    pub track: Option<TrackRef>,
    /// Position of `track` within the room's queue.
    pub index: usize,
    /// Which queue `index` refers to. A peer whose stored queue has a different
    /// revision knows its copy is stale.
    pub queue_revision: u64,
    pub position_ms: u64,
    pub playing: bool,
    /// The relay's own clock at the instant this anchor was set. Unlike protocol
    /// 2, where a master estimated relay time, this is read directly — so the
    /// anchor carries no clock-estimation error at all.
    pub at_server_ms: i64,
    /// Set while some peer is feeding an endless station into the queue. Carried
    /// so a joining peer can show that the wave is on; the id is the station's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
}

/// A mutation any peer may ask the relay to make.
///
/// Deliberately explicit rather than convenient: there is no `toggle_pause`,
/// because with several peers commanding at once a toggle races — two people
/// pressing pause together would cancel each other out. The caller decides from
/// the last snapshot whether it means [`Command::Pause`] or [`Command::Resume`].
///
/// Volume is absent on purpose: it is a property of a listener's own speakers,
/// never of the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub enum Command {
    /// Throws the current queue away and starts at `start`.
    SetQueue { tracks: Vec<TrackRef>, start: usize },
    /// Adds to the end of the queue, leaving what is playing alone.
    Enqueue { tracks: Vec<TrackRef> },
    PlayIndex { index: usize },
    Next,
    Prev,
    Pause,
    Resume,
    Seek { position_ms: u64 },
    /// Claims or releases the endless station. The claiming peer becomes its
    /// feeder: only it can resolve more tracks, since the relay holds no Yandex
    /// credentials. `None` releases the claim.
    SetStation { id: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        protocol: u16,
        room: String,
        token: String,
        client: String,
    },
    /// Clock probe. `c0` is the client's wall clock at send time and is echoed
    /// back untouched.
    TimeReq {
        c0: i64,
    },
    Do {
        command: Command,
    },
    /// Announces what this peer can serve to the others over the local network.
    /// `port` is where its file server listens; `None` withdraws the offer.
    ///
    /// The address is left to the relay — see [`PeerShare::endpoint`].
    Share {
        port: Option<u16>,
        tracks: Vec<String>,
    },
    Bye,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        protocol: u16,
        server_ms: i64,
        peers: usize,
    },
    /// Reply to [`ClientMsg::TimeReq`]: `c0` echoed, `s` is the relay's wall
    /// clock when it handled the probe.
    TimeRes {
        c0: i64,
        s: i64,
    },
    State {
        state: PlaybackState,
    },
    Queue {
        revision: u64,
        tracks: Vec<TrackRef>,
    },
    Peer {
        joined: bool,
        peers: usize,
    },
    /// Sent to the peer that asked, not broadcast: it answers "are you the one
    /// feeding the station?". A peer that loses the claim stops feeding.
    Station {
        id: Option<String>,
        yours: bool,
    },
    /// The room's whole sharing map, broadcast whenever it changes.
    ///
    /// A peer's own entry is included: it costs a few bytes and keeps one frame
    /// correct for everybody, rather than each peer needing its own version.
    Shares {
        peers: Vec<PeerShare>,
    },
    Error {
        code: String,
        message: String,
    },
}

/// What a player's session loop reacts to. Clock probes are handled inside the
/// session and never surface here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    State(PlaybackState),
    Queue {
        revision: u64,
        tracks: Vec<TrackRef>,
    },
    Peer {
        joined: bool,
        peers: usize,
    },
    Station {
        id: Option<String>,
        yours: bool,
    },
    /// Who in the room can serve which cached tracks over the local network.
    Shares {
        peers: Vec<PeerShare>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track(id: &str) -> TrackRef {
        TrackRef {
            track_id: id.into(),
            album_id: Some("100".into()),
            title: format!("Title {id}"),
            artist: "Artist".into(),
            duration_ms: 210_000,
        }
    }

    fn sample_state() -> PlaybackState {
        PlaybackState {
            seq: 7,
            track: Some(sample_track("42")),
            index: 1,
            queue_revision: 3,
            position_ms: 12_345,
            playing: true,
            at_server_ms: 1_700_000_000_000,
            station: None,
        }
    }

    #[test]
    fn state_roundtrips_through_json() {
        let msg = ServerMsg::State {
            state: sample_state(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ServerMsg>(&text).unwrap());
    }

    #[test]
    fn messages_are_internally_tagged() {
        let text = serde_json::to_string(&ClientMsg::TimeReq { c0: 5 }).unwrap();
        assert_eq!(text, r#"{"t":"time_req","c0":5}"#);
    }

    #[test]
    fn commands_carry_their_own_tag() {
        let text = serde_json::to_string(&ClientMsg::Do {
            command: Command::Seek { position_ms: 9_000 },
        })
        .unwrap();
        assert_eq!(text, r#"{"t":"do","command":{"c":"seek","position_ms":9000}}"#);
    }

    #[test]
    fn every_command_roundtrips() {
        let commands = [
            Command::SetQueue {
                tracks: vec![sample_track("1")],
                start: 0,
            },
            Command::Enqueue {
                tracks: vec![sample_track("2")],
            },
            Command::PlayIndex { index: 3 },
            Command::Next,
            Command::Prev,
            Command::Pause,
            Command::Resume,
            Command::Seek { position_ms: 1 },
            Command::SetStation {
                id: Some("wave".into()),
            },
            Command::SetStation { id: None },
        ];
        for command in commands {
            let text = serde_json::to_string(&command).unwrap();
            assert_eq!(command, serde_json::from_str::<Command>(&text).unwrap(), "{text}");
        }
    }

    #[test]
    fn absent_album_id_is_omitted() {
        let mut state = sample_state();
        state.track.as_mut().unwrap().album_id = None;
        let text = serde_json::to_string(&state).unwrap();
        assert!(!text.contains("album_id"), "{text}");
        assert_eq!(state, serde_json::from_str::<PlaybackState>(&text).unwrap());
    }

    #[test]
    fn a_silent_station_is_omitted_but_a_live_one_survives() {
        let text = serde_json::to_string(&sample_state()).unwrap();
        assert!(!text.contains("station"), "{text}");

        let mut state = sample_state();
        state.station = Some("wave".into());
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(state, serde_json::from_str::<PlaybackState>(&text).unwrap());
    }

    #[test]
    fn queue_roundtrips_through_json() {
        let msg = ServerMsg::Queue {
            revision: 5,
            tracks: vec![sample_track("1"), sample_track("2")],
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ServerMsg>(&text).unwrap());
    }

    #[test]
    fn an_empty_queue_is_representable() {
        let msg = ServerMsg::Queue {
            revision: 0,
            tracks: Vec::new(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(text, r#"{"t":"queue","revision":0,"tracks":[]}"#);
        assert_eq!(msg, serde_json::from_str::<ServerMsg>(&text).unwrap());
    }

    #[test]
    fn a_share_announcement_carries_only_the_port() {
        let msg = ClientMsg::Share {
            port: Some(8788),
            tracks: vec!["1".into(), "2".into()],
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(text, r#"{"t":"share","port":8788,"tracks":["1","2"]}"#);
        assert!(
            !text.contains("192.") && !text.contains("endpoint"),
            "the peer must not name its own address: {text}"
        );
        assert_eq!(msg, serde_json::from_str::<ClientMsg>(&text).unwrap());
    }

    #[test]
    fn withdrawing_an_offer_roundtrips() {
        let msg = ClientMsg::Share {
            port: None,
            tracks: Vec::new(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ClientMsg>(&text).unwrap());
    }

    #[test]
    fn the_sharing_map_roundtrips_through_json() {
        let msg = ServerMsg::Shares {
            peers: vec![
                PeerShare {
                    peer: 1,
                    endpoint: "http://192.168.1.10:8788".into(),
                    tracks: vec!["42".into()],
                },
                // IPv6 endpoints have to survive the round trip too, brackets
                // and all.
                PeerShare {
                    peer: 2,
                    endpoint: "http://[fe80::1]:8788".into(),
                    tracks: Vec::new(),
                },
            ],
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ServerMsg>(&text).unwrap());
    }

    #[test]
    fn an_empty_sharing_map_is_representable() {
        let msg = ServerMsg::Shares { peers: Vec::new() };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(text, r#"{"t":"shares","peers":[]}"#);
        assert_eq!(msg, serde_json::from_str::<ServerMsg>(&text).unwrap());
    }
}
