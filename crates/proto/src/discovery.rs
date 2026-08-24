//! Finding rooms on the local network.
//!
//! One UDP question, one UDP answer. A client broadcasts [`Discovery::Query`] and
//! every relay that hears it replies, unicast, with [`Discovery::Rooms`].
//!
//! # Why not mDNS
//!
//! DNS-SD would be the standard answer, and it brings a service registry, a
//! dependency, and platform quirks — a multicast lock on Android, its own
//! firewall hole on Windows, a daemon that may or may not be running. What is
//! actually needed here is much smaller: «кто в этой сети держит комнату», asked
//! once when somebody presses a button. A broadcast and a reply are about a
//! hundred lines and behave identically on all three platforms.
//!
//! # What a stranger learns
//!
//! Anyone on the same network can send the query, so the answer is deliberately
//! thin: room names, how many people are listening, and the port to connect to.
//! No token, no hash of one, and nothing about what is playing. Joining still
//! needs the room's shared secret, which never travels here.

use serde::{Deserialize, Serialize};

/// UDP port the responder listens on.
///
/// Deliberately the same number as the relay's default TCP port: one number to
/// remember and one hole to open in a firewall. UDP and TCP ports are separate
/// namespaces, so nothing collides.
pub const DISCOVERY_PORT: u16 = 8787;

/// Largest datagram either side sends or accepts.
///
/// A reply carries only names and counts, so this is roomy; it exists so a
/// stranger cannot make either side allocate a large buffer.
pub const MAX_DATAGRAM: usize = 4096;

/// Most rooms one reply will describe.
///
/// A relay with more than this many live rooms is not a home setup, and a reply
/// has to fit in one datagram.
pub const MAX_ROOMS_IN_REPLY: usize = 32;

/// The two datagrams of the discovery exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Discovery {
    /// Broadcast by a client looking for rooms. `protocol` is what the asker
    /// speaks, and is answered whatever its value: telling a client that the
    /// room it found is a version away is more useful than staying silent.
    Query { protocol: u16 },
    /// Sent straight back to whoever asked.
    ///
    /// `port` is the relay's TCP port, which is not necessarily the port this
    /// answer came from: discovery has one well-known port, while a relay may be
    /// listening anywhere.
    ///
    /// `rooms` is empty for a relay nobody has used yet — a room comes into being
    /// when the first client names one — and that answer is still worth having:
    /// this is the relay you would connect to, and the room appears when you do.
    Rooms {
        protocol: u16,
        /// Identifies the answering process for as long as it runs.
        ///
        /// A relay on the asker's own machine answers twice, once per address it
        /// was reached at, and without this the copies look like two relays. Not a
        /// secret and not stable across restarts: it exists so an asker can tell
        /// «тот же релей, другой адрес» from «другая машина с комнатой того же
        /// имени».
        id: u64,
        port: u16,
        rooms: Vec<RoomBrief>,
    },
}

/// One room, as a relay is willing to describe it to anybody who asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomBrief {
    pub name: String,
    /// How many peers are connected right now. A room kept alive by
    /// [`crate::PROTOCOL_VERSION`]'s empty-room grace period reports 0, and is
    /// still worth showing: its queue is what you would rejoin.
    pub listeners: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    #[test]
    fn a_query_is_a_short_tagged_datagram() {
        let msg = Discovery::Query {
            protocol: PROTOCOL_VERSION,
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            text,
            format!(r#"{{"t":"query","protocol":{PROTOCOL_VERSION}}}"#)
        );
        assert_eq!(msg, serde_json::from_str::<Discovery>(&text).unwrap());
    }

    #[test]
    fn an_answer_roundtrips_through_json() {
        let msg = Discovery::Rooms {
            protocol: PROTOCOL_VERSION,
            id: 42,
            port: 8787,
            rooms: vec![
                RoomBrief {
                    name: "home".into(),
                    listeners: 2,
                },
                RoomBrief {
                    name: "кухня".into(),
                    listeners: 0,
                },
            ],
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(msg, serde_json::from_str::<Discovery>(&text).unwrap());
    }

    /// A relay holding nothing still answers, and that answer is the whole point
    /// of the feature: you have just started a relay and want to find it. Rooms
    /// only exist once a client names one.
    #[test]
    fn an_empty_room_list_is_representable() {
        let msg = Discovery::Rooms {
            protocol: PROTOCOL_VERSION,
            id: 7,
            port: 9000,
            rooms: Vec::new(),
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"t":"rooms","protocol":{PROTOCOL_VERSION},"id":7,"port":9000,"rooms":[]}}"#
            )
        );
        assert_eq!(msg, serde_json::from_str::<Discovery>(&text).unwrap());
    }

    /// The reply must never grow to the point where it stops fitting in one
    /// datagram, which is what [`MAX_ROOMS_IN_REPLY`] is for.
    #[test]
    fn a_full_reply_fits_in_one_datagram() {
        let rooms = (0..MAX_ROOMS_IN_REPLY)
            .map(|i| RoomBrief {
                // Room names are capped at 64 characters by the relay.
                name: "r".repeat(64) + &i.to_string(),
                listeners: 99,
            })
            .collect();
        let msg = Discovery::Rooms {
            protocol: PROTOCOL_VERSION,
            id: u64::MAX,
            port: 8787,
            rooms,
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert!(
            text.len() < MAX_DATAGRAM,
            "{} bytes does not fit in {MAX_DATAGRAM}",
            text.len()
        );
    }

    /// Datagrams come off the network, so nonsense must be an error rather than
    /// something that decodes into a surprising default.
    #[test]
    fn rubbish_does_not_decode() {
        for text in [
            "",
            "{}",
            r#"{"t":"query"}"#,
            r#"{"t":"rooms","protocol":4}"#,
            r#"{"t":"whatever","protocol":4}"#,
        ] {
            assert!(
                serde_json::from_str::<Discovery>(text).is_err(),
                "{text} should not decode"
            );
        }
    }
}
