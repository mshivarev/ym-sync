//! Finding rooms on the local network.
//!
//! One broadcast question, then whatever answers arrive before the deadline. The
//! protocol is in [`ymsync_proto::discovery`]; this is the asking half.
//!
//! Nothing here needs a Yandex token, a room token or even a working internet
//! connection, so a front end can offer «найти комнаты» before anything is
//! configured — which is the point, since the answer is what you would type into
//! the configuration.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tracing::debug;
use ymsync_proto::discovery::{DISCOVERY_PORT, Discovery, MAX_DATAGRAM};
use ymsync_proto::PROTOCOL_VERSION;

/// How long to wait for answers by default.
///
/// A LAN round trip is a millisecond or two, so this is almost entirely patience
/// for a device that was asleep. Long enough not to miss a phone, short enough
/// that a button press feels like it did something.
pub const DEFAULT_WAIT: Duration = Duration::from_millis(700);

/// A room somebody on this network is holding — or a relay holding none yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FoundRoom {
    /// Ready to paste into `relay`: `ws://<address>:<port>`.
    pub relay: String,
    /// Empty when that relay holds no rooms yet. Connect with whatever room name
    /// you have configured and it will be created — which is the normal state of
    /// a relay you have only just started.
    pub room: String,
    pub listeners: usize,
    /// What that relay speaks.
    pub protocol: u16,
    /// False when the versions differ. Such a room is still listed rather than
    /// hidden: «обнови машины» is a better answer than an empty list.
    pub compatible: bool,
}

/// Asks the network which rooms are out there.
///
/// Both a broadcast and a loopback copy of the question go out: a relay running
/// on this very machine is the most likely thing to find, and a broadcast does
/// not always come back to its own host.
pub async fn find_rooms(wait: Duration) -> Result<Vec<FoundRoom>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))
        .await
        .context("не удалось открыть сокет для поиска комнат")?;
    socket
        .set_broadcast(true)
        .context("не удалось включить широковещание")?;

    let question = serde_json::to_vec(&Discovery::Query {
        protocol: PROTOCOL_VERSION,
    })
    .context("encoding the discovery query")?;

    let targets = [
        SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
        SocketAddr::from((Ipv4Addr::LOCALHOST, DISCOVERY_PORT)),
    ];
    let mut sent = 0;
    for target in targets {
        match socket.send_to(&question, target).await {
            Ok(_) => sent += 1,
            // A machine with no network at all cannot broadcast; the loopback
            // copy may still find a room hosted right here.
            Err(err) => debug!(%target, %err, "cannot ask for rooms"),
        }
    }
    if sent == 0 {
        anyhow::bail!("сеть недоступна: некуда отправить запрос");
    }

    Ok(collect(&socket, wait).await)
}

/// Gathers answers until the deadline, keeping one entry per room.
async fn collect(socket: &UdpSocket, wait: Duration) -> Vec<FoundRoom> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut buffer = vec![0u8; MAX_DATAGRAM];
    // Keyed by the answering process and the room name, not by address: a relay on
    // this machine answers both the broadcast and the loopback copy, and those two
    // are one relay at two addresses rather than two relays.
    let mut found: HashMap<(u64, String), FoundRoom> = HashMap::new();

    loop {
        let received = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await;
        let (len, from) = match received {
            // Deadline: everyone who was going to answer has.
            Err(_) => break,
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => {
                debug!(%err, "discovery socket hiccup");
                continue;
            }
        };

        let Ok(Discovery::Rooms {
            protocol,
            id,
            port,
            rooms,
        }) = serde_json::from_slice::<Discovery>(&buffer[..len])
        else {
            debug!(%from, len, "not an answer we understand");
            continue;
        };
        debug!(%from, id, rooms = rooms.len(), "answer");

        // The relay's address is where the answer came from, not anything it
        // claimed — the same reason the sharing map is built from observed
        // addresses. Only the port is taken on trust, because discovery has one
        // well-known port and the relay may listen on another.
        let relay = relay_url(from.ip(), port);
        let compatible = protocol == PROTOCOL_VERSION;

        // A relay holding nothing is still the answer to "куда подключаться": a
        // room is created when the first client names one.
        if rooms.is_empty() {
            remember(
                &mut found,
                (id, String::new()),
                FoundRoom {
                    relay,
                    room: String::new(),
                    listeners: 0,
                    protocol,
                    compatible,
                },
            );
            continue;
        }

        for brief in rooms {
            remember(
                &mut found,
                (id, brief.name.clone()),
                FoundRoom {
                    relay: relay.clone(),
                    room: brief.name,
                    listeners: brief.listeners,
                    protocol,
                    compatible,
                },
            );
        }
    }

    let mut rooms: Vec<FoundRoom> = found.into_values().collect();
    // Usable rooms first, then the busiest, then by name so the list holds still
    // between two searches.
    rooms.sort_by(|a, b| {
        b.compatible
            .cmp(&a.compatible)
            .then(b.listeners.cmp(&a.listeners))
            .then(a.room.cmp(&b.room))
            .then(a.relay.cmp(&b.relay))
    });
    rooms
}

/// Records an answer, preferring an address other machines could also use.
///
/// The same relay is reached at both its LAN address and loopback, and only the
/// first of those is worth showing: it is the one that goes in a config file and
/// the one that still works from the next room.
fn remember(found: &mut HashMap<(u64, String), FoundRoom>, key: (u64, String), room: FoundRoom) {
    match found.get(&key) {
        Some(kept) if !is_loopback_url(&kept.relay) => {}
        _ => {
            found.insert(key, room);
        }
    }
}

fn is_loopback_url(relay: &str) -> bool {
    relay.contains("//127.") || relay.contains("//[::1]")
}

fn relay_url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V6(ip) => format!("ws://[{ip}]:{port}"),
        IpAddr::V4(ip) => format!("ws://{ip}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ymsync_proto::discovery::RoomBrief;

    /// The end-to-end shape of the exchange, against a stand-in responder on
    /// loopback: a real relay is not needed to prove the asking half works.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_room_that_answers_is_found() {
        let found = ask(vec![RoomBrief {
            name: "home".into(),
            listeners: 2,
        }])
        .await;

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].room, "home");
        assert_eq!(found[0].listeners, 2);
        assert!(found[0].compatible);
        // The port is the relay's, from the payload; the address is where the
        // datagram came from.
        assert_eq!(found[0].relay, "ws://127.0.0.1:9999");
    }

    /// The case that matters most in practice: a relay you have just started holds
    /// no rooms at all, and finding *it* is the whole point.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_with_no_rooms_is_still_found() {
        let found = ask(Vec::new()).await;

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].relay, "ws://127.0.0.1:9999");
        assert!(
            found[0].room.is_empty(),
            "no room to name yet: {:?}",
            found[0].room
        );
        assert_eq!(found[0].listeners, 0);
    }

    /// Drives one exchange against a stand-in responder and returns what the
    /// asking half made of it.
    async fn ask(rooms: Vec<RoomBrief>) -> Vec<FoundRoom> {
        let responder = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind");
        let port = responder.local_addr().unwrap().port();

        tokio::spawn(async move {
            let mut buffer = vec![0u8; MAX_DATAGRAM];
            let (len, from) = responder.recv_from(&mut buffer).await.expect("recv");
            assert!(matches!(
                serde_json::from_slice::<Discovery>(&buffer[..len]),
                Ok(Discovery::Query { .. })
            ));
            let reply = serde_json::to_vec(&Discovery::Rooms {
                protocol: PROTOCOL_VERSION,
                id: 1,
                port: 9999,
                rooms,
            })
            .unwrap();
            responder.send_to(&reply, from).await.expect("send");
        });

        // The real `find_rooms` asks on the well-known port, which a test must not
        // squat on, so the question goes to the stand-in directly.
        let asker = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind");
        let question = serde_json::to_vec(&Discovery::Query {
            protocol: PROTOCOL_VERSION,
        })
        .unwrap();
        asker
            .send_to(&question, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .expect("ask");

        collect(&asker, Duration::from_secs(2)).await
    }

    /// Nothing on the network is an empty list, not an error: it is the ordinary
    /// answer when you are the first one up.
    #[tokio::test(flavor = "multi_thread")]
    async fn silence_is_an_empty_list() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind");
        let found = collect(&socket, Duration::from_millis(50)).await;
        assert!(found.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_undecodable_answer_is_ignored() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind");
        let target = socket.local_addr().unwrap();
        let sender = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind");
        sender.send_to(b"not json at all", target).await.unwrap();

        let found = collect(&socket, Duration::from_millis(150)).await;
        assert!(found.is_empty());
    }

    #[test]
    fn a_relay_url_is_built_for_the_address_family() {
        assert_eq!(
            relay_url(IpAddr::from([192, 168, 1, 10]), 8787),
            "ws://192.168.1.10:8787"
        );
        assert_eq!(
            relay_url("fe80::1".parse().unwrap(), 8787),
            "ws://[fe80::1]:8787"
        );
    }

    /// A relay on this machine answers both the broadcast and the loopback copy.
    /// It is one relay, and the address worth keeping is the one that also works
    /// from another machine.
    #[test]
    fn one_relay_at_two_addresses_is_listed_once() {
        let mut found = HashMap::new();
        let key = (7, "home".to_string());
        for relay in ["ws://127.0.0.1:8787", "ws://192.168.1.10:8787"] {
            remember(
                &mut found,
                key.clone(),
                FoundRoom {
                    relay: relay.to_string(),
                    room: "home".into(),
                    listeners: 1,
                    protocol: PROTOCOL_VERSION,
                    compatible: true,
                },
            );
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[&key].relay, "ws://192.168.1.10:8787");
    }

    /// The order the two answers arrive in must not decide which address is kept.
    #[test]
    fn the_routable_address_wins_whichever_arrives_first() {
        let mut found = HashMap::new();
        let key = (7, String::new());
        for relay in ["ws://192.168.1.10:8787", "ws://127.0.0.1:8787"] {
            remember(
                &mut found,
                key.clone(),
                FoundRoom {
                    relay: relay.to_string(),
                    room: String::new(),
                    listeners: 0,
                    protocol: PROTOCOL_VERSION,
                    compatible: true,
                },
            );
        }
        assert_eq!(found[&key].relay, "ws://192.168.1.10:8787");
    }

    /// Two relays that both hold a room called «home» are two entries, which is
    /// what the process id in the answer is for.
    #[test]
    fn same_room_name_on_two_relays_stays_two_entries() {
        let mut found = HashMap::new();
        for (id, relay) in [(1, "ws://192.168.1.10:8787"), (2, "ws://192.168.1.11:8787")] {
            remember(
                &mut found,
                (id, "home".to_string()),
                FoundRoom {
                    relay: relay.to_string(),
                    room: "home".into(),
                    listeners: 1,
                    protocol: PROTOCOL_VERSION,
                    compatible: true,
                },
            );
        }
        assert_eq!(found.len(), 2);
    }

    /// A room of another protocol version is shown, but never ahead of one this
    /// client can actually join.
    #[test]
    fn usable_rooms_sort_first() {
        let mut rooms = [
            FoundRoom {
                relay: "ws://192.168.1.5:8787".into(),
                room: "old".into(),
                listeners: 9,
                protocol: PROTOCOL_VERSION - 1,
                compatible: false,
            },
            FoundRoom {
                relay: "ws://192.168.1.6:8787".into(),
                room: "home".into(),
                listeners: 1,
                protocol: PROTOCOL_VERSION,
                compatible: true,
            },
        ];
        rooms.sort_by(|a, b| {
            b.compatible
                .cmp(&a.compatible)
                .then(b.listeners.cmp(&a.listeners))
                .then(a.room.cmp(&b.room))
        });
        assert_eq!(rooms[0].room, "home");
    }
}
