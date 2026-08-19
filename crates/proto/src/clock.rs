//! NTP-style clock offset estimation against the relay.
//!
//! Both players measure their own offset to the *relay's* clock; the relay is
//! only a shared reference point and never needs a correct absolute time. A
//! master publishes positions stamped in relay time, and a slave converts back
//! into its own clock. Neither machine's wall clock has to agree with reality.

use std::collections::VecDeque;

/// How many recent probes to remember. At one probe every few seconds this is a
/// window of roughly a minute, so a wall-clock step (NTP correction, sleep/wake)
/// ages out instead of skewing the estimate forever.
pub const WINDOW: usize = 16;

/// One completed round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub rtt_ms: i64,
    /// Add this to the local clock to get relay time.
    pub offset_ms: i64,
}

/// Derives an offset estimate from one probe.
///
/// `c0` is the local clock when the probe was sent, `s` the relay's clock when
/// it answered, `c1` the local clock when the answer arrived. Assuming the two
/// network legs are symmetric, the relay's reply was generated at local time
/// `c0 + rtt/2`, which makes the offset `s - (c0 + rtt/2)`.
pub fn sample_from_roundtrip(c0: i64, s: i64, c1: i64) -> ClockSample {
    let rtt_ms = (c1 - c0).max(0);
    ClockSample {
        rtt_ms,
        offset_ms: s - (c0 + rtt_ms / 2),
    }
}

/// Sliding window of probes that yields a de-noised offset.
#[derive(Debug, Clone, Default)]
pub struct ClockSampler {
    samples: VecDeque<ClockSample>,
}

impl ClockSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, sample: ClockSample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Best offset estimate, or `None` before the first probe completes.
    ///
    /// A high round-trip time means the "half the round trip each way"
    /// assumption is shakier, so only the faster half of the window is
    /// considered, and the median of those discards the remaining outliers.
    pub fn offset_ms(&self) -> Option<i64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut by_rtt: Vec<ClockSample> = self.samples.iter().copied().collect();
        by_rtt.sort_unstable_by_key(|s| s.rtt_ms);
        by_rtt.truncate((by_rtt.len() / 2).max(1));

        let mut offsets: Vec<i64> = by_rtt.iter().map(|s| s.offset_ms).collect();
        offsets.sort_unstable();
        Some(offsets[offsets.len() / 2])
    }

    /// Lowest round trip in the window: our best-case confidence bound on the
    /// offset estimate.
    pub fn rtt_ms(&self) -> Option<i64> {
        self.samples.iter().map(|s| s.rtt_ms).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_trip_recovers_exact_offset() {
        // Local clock is 1000 ms behind the relay, 40 ms round trip.
        let sample = sample_from_roundtrip(0, 1020, 40);
        assert_eq!(sample.rtt_ms, 40);
        assert_eq!(sample.offset_ms, 1000);
    }

    #[test]
    fn zero_offset_when_clocks_agree() {
        let sample = sample_from_roundtrip(500, 550, 600);
        assert_eq!(sample.offset_ms, 0);
    }

    #[test]
    fn negative_rtt_from_a_clock_step_is_clamped() {
        let sample = sample_from_roundtrip(1000, 1000, 900);
        assert_eq!(sample.rtt_ms, 0);
    }

    #[test]
    fn empty_sampler_has_no_estimate() {
        let sampler = ClockSampler::new();
        assert_eq!(sampler.offset_ms(), None);
        assert_eq!(sampler.rtt_ms(), None);
    }

    #[test]
    fn slow_probes_are_ignored_in_favour_of_fast_ones() {
        let mut sampler = ClockSampler::new();
        for _ in 0..4 {
            sampler.push(ClockSample {
                rtt_ms: 5,
                offset_ms: 100,
            });
        }
        for _ in 0..4 {
            sampler.push(ClockSample {
                rtt_ms: 900,
                offset_ms: -5_000,
            });
        }
        assert_eq!(sampler.offset_ms(), Some(100));
        assert_eq!(sampler.rtt_ms(), Some(5));
    }

    #[test]
    fn a_single_outlier_does_not_move_the_median() {
        let mut sampler = ClockSampler::new();
        for offset in [100, 102, 98, 101, 99, 100] {
            sampler.push(ClockSample { rtt_ms: 10, offset_ms: offset });
        }
        sampler.push(ClockSample { rtt_ms: 10, offset_ms: 90_000 });
        let offset = sampler.offset_ms().unwrap();
        assert!((98..=102).contains(&offset), "offset was {offset}");
    }

    #[test]
    fn window_forgets_stale_samples() {
        let mut sampler = ClockSampler::new();
        for _ in 0..WINDOW {
            sampler.push(ClockSample { rtt_ms: 10, offset_ms: 0 });
        }
        assert_eq!(sampler.len(), WINDOW);
        for _ in 0..WINDOW {
            sampler.push(ClockSample { rtt_ms: 10, offset_ms: 7_000 });
        }
        assert_eq!(sampler.len(), WINDOW);
        assert_eq!(sampler.offset_ms(), Some(7_000));
    }
}
