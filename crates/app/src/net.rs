//! Which of this machine's addresses the rest of the network can reach.
//!
//! Needed only when hosting a room: the others have to be told an address to
//! connect to, and the app has to dial the same one itself rather than
//! `127.0.0.1`. That last part matters more than it looks — the relay composes
//! every peer's file-server address from the socket that peer arrives on, so a
//! host that reached its own relay over loopback would advertise a loopback
//! address for its cached tracks, and nobody else could fetch them.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// Addresses used only to ask the routing table a question. Nothing is sent to
/// them: connecting a UDP socket just fixes a destination, which makes the kernel
/// choose the source address it would use to get there.
///
/// A public address first, because with a working internet connection the
/// interface holding the default route is almost always the right answer. Then
/// the usual private ranges, for a network with no route out — which is exactly
/// the case this feature exists for.
const PROBES: [&str; 4] = [
    "8.8.8.8:53",
    "192.168.0.1:53",
    "10.0.0.1:53",
    "172.16.0.1:53",
];

/// This machine's address on the local network, as far as the routing table
/// knows.
///
/// `None` when there is no usable network at all, which is a perfectly ordinary
/// state: a room hosted for this machine alone works over loopback.
///
/// Note the limit of asking the routing table rather than enumerating interfaces:
/// with a VPN up, the default route belongs to the VPN, and that is the address
/// this returns. Hence `[host].advertise` in the config, and an editable field in
/// the front ends.
pub fn lan_address() -> Option<Ipv4Addr> {
    PROBES.iter().find_map(|probe| source_address_for(probe))
}

fn source_address_for(target: &str) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(target).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if usable(addr.ip()) => Some(*addr.ip()),
        _ => None,
    }
}

/// Whether an address is worth handing to somebody else.
fn usable(ip: &Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()
}

/// The host to advertise, and to dial ourselves, when hosting a room.
///
/// A configured value always wins: on a machine with a VPN or a virtual switch it
/// is the only way to be sure. Falling back to loopback is deliberate rather than
/// an error — a room with just this machine in it works perfectly over loopback,
/// and that is what offline listening alone looks like.
pub fn advertise_host(configured: &str) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    match lan_address() {
        Some(ip) => ip.to_string(),
        None => Ipv4Addr::LOCALHOST.to_string(),
    }
}

/// A relay URL for a host and port, bracketing an IPv6 literal as a URL needs.
pub fn relay_url(host: &str, port: u16) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(ip)) => format!("ws://[{ip}]:{port}"),
        _ => format!("ws://{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing that must never be guessed: on a machine with several
    /// networks the user's answer is the only reliable one.
    #[test]
    fn a_configured_host_is_used_verbatim() {
        assert_eq!(advertise_host("192.168.1.50"), "192.168.1.50");
        assert_eq!(advertise_host("  192.168.1.50  "), "192.168.1.50");
        assert_eq!(advertise_host("desktop.local"), "desktop.local");
    }

    /// With no network there is still a working answer, because a room hosted for
    /// this machine alone needs nothing more.
    #[test]
    fn without_a_configured_host_there_is_always_an_answer() {
        let host = advertise_host("");
        assert!(!host.is_empty());
        assert!(host.parse::<IpAddr>().is_ok(), "{host} should be an address");
    }

    /// An address that is offered to other machines has to be one they could
    /// plausibly reach.
    #[test]
    fn a_detected_address_is_never_loopback_or_unspecified() {
        if let Some(ip) = lan_address() {
            assert!(usable(&ip), "{ip} is not worth advertising");
        }
    }

    #[test]
    fn unusable_addresses_are_recognised() {
        assert!(!usable(&Ipv4Addr::LOCALHOST));
        assert!(!usable(&Ipv4Addr::UNSPECIFIED));
        assert!(!usable(&Ipv4Addr::BROADCAST));
        assert!(usable(&Ipv4Addr::new(192, 168, 1, 10)));
        assert!(usable(&Ipv4Addr::new(10, 0, 0, 4)));
    }

    #[test]
    fn a_relay_url_is_built_for_the_address_family() {
        assert_eq!(relay_url("192.168.1.10", 8787), "ws://192.168.1.10:8787");
        assert_eq!(relay_url("desktop.local", 8787), "ws://desktop.local:8787");
        assert_eq!(relay_url("2001:db8::5", 8787), "ws://[2001:db8::5]:8787");
    }
}
