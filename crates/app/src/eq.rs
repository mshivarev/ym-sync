//! A five-band equaliser, applied to whatever the player decodes.
//!
//! The gains live in atomics shared with the source wrapper, so a slider moves
//! the sound while the track plays — no reload, no seek. Each band is a peaking
//! biquad (the RBJ cookbook's), run per channel. All bands flat means no
//! filtering at all, so the equaliser costs nothing until it is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The bands' centre frequencies, in hertz.
pub const BANDS_HZ: [f32; BANDS] = [60.0, 230.0, 910.0, 3_600.0, 14_000.0];
pub const BANDS: usize = 5;
/// How far a band may go either way, in decibels.
pub const MAX_GAIN_DB: f32 = 12.0;

/// Band width: about an octave and a half, wide enough that neighbouring bands
/// blend rather than leaving notches between them.
const Q: f32 = 1.0;

/// The settings, shared between whoever moves the sliders and the audio thread.
#[derive(Default)]
pub struct Equalizer {
    gains: [AtomicU32; BANDS],
    /// Bumped on every change, so the audio thread knows to recompute.
    generation: AtomicU64,
}

impl Equalizer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets the bands, in decibels; out-of-range values are clamped, missing
    /// ones are flat.
    pub fn set(&self, gains: &[f32]) {
        for (index, slot) in self.gains.iter().enumerate() {
            let gain = gains
                .get(index)
                .copied()
                .filter(|gain| gain.is_finite())
                .unwrap_or(0.0)
                .clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
            slot.store(gain.to_bits(), Ordering::Relaxed);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn gains(&self) -> [f32; BANDS] {
        std::array::from_fn(|index| f32::from_bits(self.gains[index].load(Ordering::Relaxed)))
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// One band's coefficients, normalised by `a0`.
#[derive(Clone, Copy, Default)]
struct Coefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Coefficients {
    /// A peaking filter at `hz` with `gain_db`.
    fn peaking(hz: f32, gain_db: f32, sample_rate: f32) -> Option<Self> {
        if gain_db.abs() < 0.01 || hz >= sample_rate * 0.45 {
            return None;
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = std::f32::consts::TAU * hz / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * Q);
        let a0 = 1.0 + alpha / a;
        Some(Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
        })
    }
}

/// A filter's memory for one channel (transposed direct form II).
#[derive(Clone, Copy, Default)]
struct State {
    z1: f32,
    z2: f32,
}

impl State {
    fn run(&mut self, c: &Coefficients, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

/// The filtering, independent of where the samples come from: interleaved
/// samples in, the same out.
pub struct Filter {
    settings: Arc<Equalizer>,
    seen: u64,
    sample_rate: u32,
    channels: usize,
    bands: Vec<Coefficients>,
    /// `states[channel][band]`.
    states: Vec<Vec<State>>,
    /// Pulled down when bands are raised, so a boosted track does not clip.
    preamp: f32,
    /// Which channel the next sample belongs to.
    channel: usize,
}

impl Filter {
    pub fn new(settings: Arc<Equalizer>) -> Self {
        Self {
            settings,
            seen: u64::MAX,
            sample_rate: 0,
            channels: 0,
            bands: Vec::new(),
            states: Vec::new(),
            preamp: 1.0,
            channel: 0,
        }
    }

    /// Picks up new gains or a new stream format. Cheap when nothing changed.
    pub fn configure(&mut self, sample_rate: u32, channels: u16) {
        let generation = self.settings.generation();
        let channels = channels.max(1) as usize;
        if generation == self.seen && sample_rate == self.sample_rate && channels == self.channels {
            return;
        }
        let format_changed = sample_rate != self.sample_rate || channels != self.channels;
        self.seen = generation;
        self.sample_rate = sample_rate;
        self.channels = channels;

        let gains = self.settings.gains();
        self.bands = BANDS_HZ
            .iter()
            .zip(gains)
            .filter_map(|(&hz, gain)| Coefficients::peaking(hz, gain, sample_rate.max(1) as f32))
            .collect();
        let boost = gains.iter().copied().fold(0.0f32, f32::max);
        self.preamp = 10f32.powf(-boost / 2.0 / 20.0);
        if format_changed || self.states.len() != channels || self.states.first().map(Vec::len) != Some(self.bands.len()) {
            self.states = vec![vec![State::default(); self.bands.len()]; channels];
            self.channel = 0;
        }
    }

    /// Forgets the filters' memory — after a seek, the old tail is not this
    /// part of the track.
    pub fn reset(&mut self) {
        for channel in &mut self.states {
            channel.fill(State::default());
        }
        self.channel = 0;
    }

    pub fn process(&mut self, sample: f32) -> f32 {
        if self.bands.is_empty() {
            return sample;
        }
        let states = &mut self.states[self.channel];
        let mut value = sample * self.preamp;
        for (state, coefficients) in states.iter_mut().zip(&self.bands) {
            value = state.run(coefficients, value);
        }
        self.channel = (self.channel + 1) % self.channels;
        value.clamp(-1.0, 1.0)
    }
}

#[cfg(feature = "audio-rodio")]
pub use source::EqSource;

#[cfg(feature = "audio-rodio")]
mod source {
    use std::time::Duration;

    use rodio::Source;
    use rodio::source::SeekError;

    use super::*;

    /// How often, in samples, new gains are looked for: about every 20 ms.
    const CHECK_EVERY: u32 = 1_024;

    /// A rodio source run through the equaliser.
    pub struct EqSource<S> {
        inner: S,
        filter: Filter,
        until_check: u32,
    }

    impl<S: Source<Item = f32>> EqSource<S> {
        pub fn new(inner: S, settings: Arc<Equalizer>) -> Self {
            let mut filter = Filter::new(settings);
            filter.configure(inner.sample_rate(), inner.channels());
            Self {
                inner,
                filter,
                until_check: CHECK_EVERY,
            }
        }
    }

    impl<S: Source<Item = f32>> Iterator for EqSource<S> {
        type Item = f32;

        fn next(&mut self) -> Option<f32> {
            self.until_check -= 1;
            if self.until_check == 0 {
                self.until_check = CHECK_EVERY;
                self.filter.configure(self.inner.sample_rate(), self.inner.channels());
            }
            self.inner.next().map(|sample| self.filter.process(sample))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.inner.size_hint()
        }
    }

    impl<S: Source<Item = f32>> Source for EqSource<S> {
        fn current_span_len(&self) -> Option<usize> {
            self.inner.current_span_len()
        }

        fn channels(&self) -> u16 {
            self.inner.channels()
        }

        fn sample_rate(&self) -> u32 {
            self.inner.sample_rate()
        }

        fn total_duration(&self) -> Option<Duration> {
            self.inner.total_duration()
        }

        fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
            self.inner.try_seek(pos)?;
            self.filter.reset();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at `hz`, its peak level after the filter, past the settling time.
    fn level_through(settings: &Arc<Equalizer>, hz: f32) -> f32 {
        let rate = 44_100;
        let mut filter = Filter::new(Arc::clone(settings));
        filter.configure(rate, 1);
        let mut peak = 0.0f32;
        for n in 0..rate {
            let x = 0.25 * (std::f32::consts::TAU * hz * n as f32 / rate as f32).sin();
            let y = filter.process(x);
            if n > rate / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn flat_settings_leave_the_sound_untouched() {
        let settings = Equalizer::new();
        let mut filter = Filter::new(Arc::clone(&settings));
        filter.configure(44_100, 2);
        for x in [-0.5f32, 0.0, 0.3, 0.9] {
            assert_eq!(filter.process(x), x);
        }
    }

    #[test]
    fn a_raised_band_raises_its_frequencies_and_not_far_ones() {
        let settings = Equalizer::new();
        let flat_bass = level_through(&settings, 60.0);
        let flat_treble = level_through(&settings, 10_000.0);
        settings.set(&[12.0, 0.0, 0.0, 0.0, 0.0]);
        let boosted_bass = level_through(&settings, 60.0);
        let boosted_treble = level_through(&settings, 10_000.0);
        // +12 dB is ×4, less the preamp's 6 dB: about ×2 for the bass.
        assert!(boosted_bass > flat_bass * 1.7, "{boosted_bass} vs {flat_bass}");
        // The treble is only pulled down by the preamp.
        assert!(boosted_treble < flat_treble * 0.6, "{boosted_treble} vs {flat_treble}");
    }

    #[test]
    fn gains_are_clamped_and_missing_ones_are_flat() {
        let settings = Equalizer::new();
        settings.set(&[40.0, -40.0, f32::NAN]);
        assert_eq!(settings.gains(), [MAX_GAIN_DB, -MAX_GAIN_DB, 0.0, 0.0, 0.0]);
    }
}
