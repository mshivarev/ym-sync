//! Drift correction: what a peer should do to line up with the room.
//!
//! Correction is seek-only, deliberately. Nudging playback rate would drift the
//! pitch, and `rodio`'s reported position is scaled by playback speed, which
//! would make the very measurement we correct against unreliable. Because the
//! whole track is held in memory, a seek costs a decoder reset rather than a
//! network round trip, so seeking is cheap enough to be the only tool needed.

use crate::PlaybackState;

/// Tunables for [`decide`]. Defaults aim at "different rooms, in step to within
/// a few hundred milliseconds".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncParams {
    /// Correct only once the error exceeds this, so we do not seek constantly.
    pub seek_threshold_ms: i64,
    /// Seek this far ahead of the computed target to absorb the time a seek and
    /// decoder reset take. Only applied while playing.
    pub seek_lead_ms: i64,
    /// Above this round-trip time the offset estimate is too coarse to act on.
    pub max_rtt_ms: i64,
}

impl Default for SyncParams {
    fn default() -> Self {
        Self {
            seek_threshold_ms: 300,
            seek_lead_ms: 40,
            max_rtt_ms: 400,
        }
    }
}

/// What to do with the local player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    /// Close enough; leave the playhead alone.
    None,
    Seek { to_ms: u64 },
    Pause,
    /// Seek, then resume.
    Resume { to_ms: u64 },
}

/// Where the room's playhead is *now*, extrapolated from the relay's last
/// snapshot.
///
/// `now_server_ms` must be in relay time (local clock plus the estimated
/// offset). A paused room does not move, so its anchor is used as-is.
pub fn target_position_ms(state: &PlaybackState, now_server_ms: i64) -> i64 {
    if state.playing {
        let elapsed = (now_server_ms - state.at_server_ms).max(0);
        state.position_ms as i64 + elapsed
    } else {
        state.position_ms as i64
    }
}

/// Whether the clock estimate is sharp enough to correct against.
pub fn trust_clock(rtt_ms: Option<i64>, params: &SyncParams) -> bool {
    matches!(rtt_ms, Some(rtt) if rtt <= params.max_rtt_ms)
}

/// Compares the local playhead against the target and picks a correction.
pub fn decide(
    target_ms: i64,
    local_ms: i64,
    room_playing: bool,
    local_playing: bool,
    params: &SyncParams,
) -> Correction {
    let drift_ms = local_ms - target_ms;
    let off_target = drift_ms.abs() > params.seek_threshold_ms;

    match (room_playing, local_playing) {
        // Room stopped: stop too, and only then line the playhead up, since a
        // paused playhead does not run away from us.
        (false, true) => Correction::Pause,
        (false, false) if off_target => Correction::Seek {
            to_ms: target_ms.max(0) as u64,
        },
        (false, false) => Correction::None,

        // Starting from a standstill: aim slightly ahead, because the seek and
        // decoder reset consume real time before the first sample is heard.
        (true, false) => Correction::Resume {
            to_ms: (target_ms + params.seek_lead_ms).max(0) as u64,
        },
        (true, true) if off_target => Correction::Seek {
            to_ms: (target_ms + params.seek_lead_ms).max(0) as u64,
        },
        (true, true) => Correction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrackRef;

    fn state(position_ms: u64, playing: bool, at_server_ms: i64) -> PlaybackState {
        PlaybackState {
            seq: 1,
            track: Some(TrackRef {
                track_id: "1".into(),
                album_id: None,
                album: None,
                title: "t".into(),
                artist: "a".into(),
                duration_ms: 300_000,
            }),
            index: 0,
            queue_revision: 1,
            position_ms,
            playing,
            at_server_ms,
            station: None,
            station_unfed: false,
        }
    }

    #[test]
    fn playing_target_advances_with_relay_time() {
        let s = state(10_000, true, 1_000);
        assert_eq!(target_position_ms(&s, 1_000), 10_000);
        assert_eq!(target_position_ms(&s, 3_500), 12_500);
    }

    #[test]
    fn paused_target_is_frozen() {
        let s = state(10_000, false, 1_000);
        assert_eq!(target_position_ms(&s, 60_000), 10_000);
    }

    #[test]
    fn snapshot_from_the_future_does_not_rewind_the_target() {
        // A late clock re-estimate can briefly put the snapshot ahead of us.
        let s = state(10_000, true, 5_000);
        assert_eq!(target_position_ms(&s, 4_000), 10_000);
    }

    #[test]
    fn small_drift_is_left_alone() {
        let p = SyncParams::default();
        assert_eq!(decide(10_000, 10_200, true, true, &p), Correction::None);
        assert_eq!(decide(10_000, 9_800, true, true, &p), Correction::None);
    }

    #[test]
    fn large_drift_seeks_with_lead() {
        let p = SyncParams::default();
        assert_eq!(
            decide(10_000, 5_000, true, true, &p),
            Correction::Seek { to_ms: 10_040 }
        );
        assert_eq!(
            decide(10_000, 20_000, true, true, &p),
            Correction::Seek { to_ms: 10_040 }
        );
    }

    #[test]
    fn a_paused_room_pauses_us_before_aligning() {
        let p = SyncParams::default();
        assert_eq!(decide(10_000, 90_000, false, true, &p), Correction::Pause);
    }

    #[test]
    fn while_both_paused_we_align_without_lead() {
        let p = SyncParams::default();
        assert_eq!(
            decide(10_000, 12_000, false, false, &p),
            Correction::Seek { to_ms: 10_000 }
        );
        assert_eq!(decide(10_000, 10_100, false, false, &p), Correction::None);
    }

    #[test]
    fn a_resumed_room_resumes_us() {
        let p = SyncParams::default();
        assert_eq!(
            decide(10_000, 0, true, false, &p),
            Correction::Resume { to_ms: 10_040 }
        );
    }

    #[test]
    fn negative_targets_clamp_to_the_start() {
        let p = SyncParams::default();
        assert_eq!(
            decide(-5_000, 30_000, false, false, &p),
            Correction::Seek { to_ms: 0 }
        );
    }

    #[test]
    fn noisy_clock_is_not_trusted() {
        let p = SyncParams::default();
        assert!(trust_clock(Some(20), &p));
        assert!(trust_clock(Some(400), &p));
        assert!(!trust_clock(Some(401), &p));
        assert!(!trust_clock(None, &p));
    }
}
