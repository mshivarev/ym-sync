//! Wire protocol and sync math shared by `ymsync` (player) and `ymsync-relay`.
//!
//! Everything here is pure data plus pure functions, so the timing logic can be
//! unit-tested without audio hardware or a network.

#![forbid(unsafe_code)]

pub mod clock;
pub mod message;
pub mod sync;

pub use clock::{ClockSample, ClockSampler, sample_from_roundtrip};
pub use message::{
    ClientMsg, Command, Event, PROTOCOL_VERSION, PeerShare, PlaybackState, ServerMsg, TrackRef,
};
pub use sync::{Correction, SyncParams, decide, target_position_ms, trust_clock};

/// Wall-clock milliseconds since the Unix epoch.
///
/// The offset between two machines' wall clocks is estimated separately (see
/// [`clock`]), so a wall clock is good enough here and, unlike
/// [`std::time::Instant`], is comparable across processes.
pub fn unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(err) => -(err.duration().as_millis() as i64),
    }
}

/// Length-independent-time comparison of two secrets, so a wrong token leaks
/// nothing about the right one through response timing.
///
/// Lives here because both places that check the room token — the relay's
/// handshake and the cached-track file server — must check it the same way.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_comparison_accepts_only_the_exact_token() {
        assert!(secret_eq("hunter2", "hunter2"));
        assert!(!secret_eq("hunter2", "hunter3"));
        assert!(!secret_eq("hunter2", "hunter"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }
}
