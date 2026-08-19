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
    ClientMsg, Command, Event, PROTOCOL_VERSION, PlaybackState, ServerMsg, TrackRef,
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
