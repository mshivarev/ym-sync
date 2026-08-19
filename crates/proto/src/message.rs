//! JSON messages exchanged between players and the relay.

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to the messages below. The relay rejects
/// clients that do not match.
///
/// * 1 — single track per state snapshot.
/// * 2 — queues: [`ClientMsg::Queue`] plus `index`/`queue_revision` in
///   [`PlaybackState`].
pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Plays whatever the user asks for and broadcasts its playback state.
    Master,
    /// Follows the master's state.
    Slave,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Master => f.write_str("master"),
            Role::Slave => f.write_str("slave"),
        }
    }
}

/// Identifies a track well enough for another account to resolve it.
///
/// `track_id` is the authoritative key: it is the same numeric id on every
/// account, so the slave does not have to match on artist/title strings. The
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

/// A snapshot of the master's playhead.
///
/// The current track is repeated here rather than only referenced by `index`,
/// so a slave that has not yet received the queue can still play along; the
/// queue itself arrives separately (see [`ClientMsg::Queue`]) because resending
/// a hundred tracks every second would be pure waste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Monotonically increasing per master session; lets the slave drop
    /// reordered or stale snapshots.
    pub seq: u64,
    pub track: Option<TrackRef>,
    /// Position of `track` within the master's queue.
    pub index: usize,
    /// Which queue `index` refers to. A slave whose stored queue has a
    /// different revision knows its copy is stale.
    pub queue_revision: u64,
    pub position_ms: u64,
    pub playing: bool,
    /// The master's estimate of the *relay's* clock at the instant
    /// `position_ms` was sampled. Both sides convert to relay time, so neither
    /// needs the other's clock to be correct.
    pub at_server_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        protocol: u16,
        room: String,
        token: String,
        role: Role,
        client: String,
    },
    /// Clock probe. `c0` is the client's wall clock at send time and is echoed
    /// back untouched.
    TimeReq {
        c0: i64,
    },
    State {
        state: PlaybackState,
    },
    /// The master's full queue. Sent when it changes and whenever a peer joins,
    /// since the relay keeps no state to replay.
    Queue {
        revision: u64,
        tracks: Vec<TrackRef>,
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
        role: Role,
        joined: bool,
        peers: usize,
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
    Queue { revision: u64, tracks: Vec<TrackRef> },
    Peer { role: Role, joined: bool, peers: usize },
    Error { code: String, message: String },
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
        }
    }

    #[test]
    fn state_roundtrips_through_json() {
        let msg = ClientMsg::State {
            state: sample_state(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ClientMsg>(&text).unwrap());
    }

    #[test]
    fn messages_are_internally_tagged() {
        let text = serde_json::to_string(&ClientMsg::TimeReq { c0: 5 }).unwrap();
        assert_eq!(text, r#"{"t":"time_req","c0":5}"#);
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
    fn queue_roundtrips_through_json() {
        let msg = ClientMsg::Queue {
            revision: 5,
            tracks: vec![sample_track("1"), sample_track("2")],
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<ClientMsg>(&text).unwrap());
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
}
