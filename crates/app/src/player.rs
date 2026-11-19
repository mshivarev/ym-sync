//! Desktop audio output.
//!
//! A track is decoded either from memory — a download, a file on disk — or from
//! a download still in progress, so playback starts on the first part instead
//! of after the whole file. Playback speed is deliberately left at 1.0: `rodio`
//! scales its reported position by playback speed, so nudging the rate would
//! corrupt the very measurement the sync loop corrects against — and it would
//! bend the pitch as well.

use std::io::{Cursor, Read, Seek};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use rodio::decoder::DecoderBuilder;
use rodio::cpal::traits::HostTrait;
use rodio::queue::SourcesQueueOutput;
use rodio::{DeviceTrait, OutputStream, OutputStreamBuilder, Sink, Source};
use ymsync_proto::TrackRef;

use crate::playback::{AudioSource, Playback, loudness, slider_for};
use crate::stream::Progressive;

/// How long a seek into a part still downloading may wait for it.
const SEEK_WAIT: Duration = Duration::from_secs(120);

/// Slack on top of the estimated byte offset of a seek target: the estimate
/// assumes a constant bitrate, and the decoder reads a little ahead.
const SEEK_MARGIN_BYTES: usize = 128 * 1024;

/// How often the audio thread checks which device Windows plays to.
const DEVICE_POLL: Duration = Duration::from_millis(500);

/// While there is no output at all, the queue is drained in steps this long.
const SILENT_STEP: Duration = Duration::from_millis(10);

pub struct Player {
    sink: Arc<Sink>,
    current: Mutex<Option<TrackRef>>,
    /// The download the current track plays from, while it is one.
    stream: Mutex<Option<Progressive>>,
    /// Bumped on every load, so a seek that waited for data can tell whether the
    /// track it was meant for is still the one playing.
    generation: Arc<AtomicU64>,
    seeker: mpsc::Sender<SeekJob>,
    /// Shared with every source this player decodes, so a change is heard at once.
    equalizer: Arc<crate::eq::Equalizer>,
}

/// The sink's queue as seen by one output stream.
///
/// Every stream that is opened gets its own, and the generation tells an old
/// one to end: by the time a stream is dropped it may still be inside its
/// callback, and two streams reading one queue would play it at double speed.
struct Detachable {
    queue: Arc<Mutex<SourcesQueueOutput>>,
    current: Arc<AtomicU64>,
    mine: u64,
}

impl Detachable {
    fn attached(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.mine
    }
}

impl Iterator for Detachable {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<rodio::Sample> {
        if !self.attached() {
            return None;
        }
        self.queue.lock().ok()?.next()
    }
}

impl Source for Detachable {
    fn current_span_len(&self) -> Option<usize> {
        self.queue.lock().ok()?.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.queue.lock().map_or(2, |queue| queue.channels())
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.queue.lock().map_or(44_100, |queue| queue.sample_rate())
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// The output chosen in the settings, by name. Empty follows Windows.
static CHOSEN_OUTPUT: Mutex<String> = Mutex::new(String::new());

/// Picks the device to play to, by the name [`output_devices`] lists; empty
/// means whatever Windows plays to. Heard at once, and kept for players opened
/// later. A device that is not plugged in is waited for, playing to the
/// default meanwhile.
pub fn set_output_device(name: &str) {
    if let Ok(mut chosen) = CHOSEN_OUTPUT.lock() {
        *chosen = name.trim().to_string();
    }
}

fn chosen_output() -> String {
    CHOSEN_OUTPUT.lock().map(|chosen| chosen.clone()).unwrap_or_default()
}

/// Names of the outputs that can be played to right now.
pub fn output_devices() -> Vec<String> {
    let mut names: Vec<String> = rodio::cpal::default_host()
        .output_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default();
    names.dedup();
    names
}

fn output_named(name: &str) -> Option<rodio::Device> {
    rodio::cpal::default_host()
        .output_devices()
        .ok()?
        .find(|device| device.name().is_ok_and(|found| found == name))
}

/// Name of the device Windows currently plays to, if there is one.
fn default_output_name() -> Option<String> {
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|device| device.name().ok())
}

/// Opens the chosen output, or the default one, raising `failed` when the
/// stream later dies. Says whether it is the chosen device that opened.
fn open_output(chosen: &str, failed: &Arc<AtomicBool>) -> Result<(OutputStream, bool), String> {
    let flag = Arc::clone(failed);
    let on_error = move |err: rodio::cpal::StreamError| {
        tracing::warn!("audio output failed: {err}");
        flag.store(true, Ordering::Release);
    };
    if !chosen.is_empty()
        && let Some(device) = output_named(chosen)
    {
        match OutputStreamBuilder::from_device(device)
            .and_then(|builder| builder.with_error_callback(on_error.clone()).open_stream())
        {
            Ok(mut stream) => {
                stream.log_on_drop(false);
                return Ok((stream, true));
            }
            Err(err) => tracing::warn!(device = chosen, "cannot open the chosen output: {err}"),
        }
    }
    let mut stream = match OutputStreamBuilder::from_default_device() {
        Ok(builder) => builder.with_error_callback(on_error).open_stream(),
        Err(err) => Err(err),
    }
    // Something odd about the default device: let rodio try the others.
    .or_else(|_| OutputStreamBuilder::open_default_stream())
    .map_err(|err| err.to_string())?;
    stream.log_on_drop(false);
    Ok((stream, false))
}

/// The audio thread: keeps the sink's queue playing on whatever output is
/// current, for the life of the process.
///
/// A stream is replaced when cpal reports it dead (the device was unplugged),
/// when another output is chosen in the settings, when the chosen one comes
/// back, and, while following Windows, when Windows switches its default
/// output, so that plugging headphones in moves the music to them as any other
/// player does. With no device at all
/// the queue is still read, silently and in real time: the sink's controls are
/// carried out by whoever reads it, so pause and seek keep working, and the
/// track keeps its place in the room.
fn run_output(queue: Arc<Mutex<SourcesQueueOutput>>, ready: mpsc::Sender<Result<(), String>>) {
    let generation = Arc::new(AtomicU64::new(0));
    let mut ready = Some(ready);
    loop {
        let default_at_open = default_output_name();
        let chosen = chosen_output();
        let failed = Arc::new(AtomicBool::new(false));
        match open_output(&chosen, &failed) {
            Ok((stream, on_chosen)) => {
                let mine = generation.fetch_add(1, Ordering::AcqRel) + 1;
                stream.mixer().add(Detachable {
                    queue: Arc::clone(&queue),
                    current: Arc::clone(&generation),
                    mine,
                });
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
                loop {
                    std::thread::sleep(DEVICE_POLL);
                    if failed.load(Ordering::Acquire) {
                        tracing::info!("audio output lost; reopening");
                        break;
                    }
                    if chosen_output() != chosen {
                        tracing::info!(device = %chosen_output(), "audio output chosen");
                        break;
                    }
                    if on_chosen {
                        continue;
                    }
                    if !chosen.is_empty() && output_named(&chosen).is_some() {
                        tracing::info!(device = %chosen, "the chosen audio output is back");
                        break;
                    }
                    let now = default_output_name();
                    if now.is_some() && now != default_at_open {
                        tracing::info!(device = ?now, "default audio output changed");
                        break;
                    }
                }
                // Detach before dropping, so the old stream lets go of the queue.
                generation.fetch_add(1, Ordering::AcqRel);
                drop(stream);
            }
            Err(err) => {
                if let Some(tx) = ready.take() {
                    // Nothing to play to at start: say so, as before.
                    let _ = tx.send(Err(err));
                    return;
                }
                play_silently(&queue, DEVICE_POLL);
            }
        }
    }
}

/// Reads the queue at the speed it would be heard, without a device.
fn play_silently(queue: &Mutex<SourcesQueueOutput>, for_: Duration) {
    let until = Instant::now() + for_;
    let mut last = Instant::now();
    while Instant::now() < until {
        std::thread::sleep(SILENT_STEP);
        let now = Instant::now();
        let elapsed = now - last;
        last = now;
        let Ok(mut queue) = queue.lock() else { return };
        let per_second = u128::from(queue.sample_rate()) * u128::from(queue.channels());
        let samples = (per_second * elapsed.as_micros() / 1_000_000) as usize;
        for _ in 0..samples {
            if queue.next().is_none() {
                break;
            }
        }
    }
}

/// A seek into a part of the track that has not downloaded yet.
struct SeekJob {
    generation: u64,
    to: Duration,
    stream: Progressive,
    need_bytes: usize,
}

impl Player {
    pub fn new(volume: f32) -> Result<Self> {
        // The sink is not tied to a device: its queue is fed to whichever output
        // is current, by a thread of its own (see [`run_output`]). Pulling the
        // headphones out or switching the output in Windows kills a cpal stream,
        // and a sink built straight on that stream's mixer is then never read
        // again: `try_seek` waits for the dead audio thread for ever, and every
        // button behind it hangs. Here the queue just moves to the new device.
        let (sink, queue) = Sink::new();
        let queue = Arc::new(Mutex::new(queue));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("ymsync-audio".to_string())
            .spawn(move || run_output(queue, tx))
            .context("spawning the audio thread")?;

        rx.recv()
            .context("the audio thread stopped before opening the output")?
            .map_err(|err| anyhow!("cannot open the default audio output: {err}"))?;

        sink.set_volume(loudness(volume));
        sink.pause();

        let sink = Arc::new(sink);
        let generation = Arc::new(AtomicU64::new(0));
        let seeker = spawn_seeker(Arc::clone(&sink), Arc::clone(&generation))?;

        Ok(Self {
            sink,
            current: Mutex::new(None),
            stream: Mutex::new(None),
            generation,
            seeker,
            equalizer: crate::eq::Equalizer::new(),
        })
    }

    /// Decodes without opening an audio device, returning the track's duration
    /// when the decoder can determine it. Used by `ymsync probe` to prove the
    /// downloaded bytes are really playable audio.
    pub fn probe_decode(data: Bytes) -> Result<Option<Duration>> {
        let len = data.len() as u64;
        Ok(build_decoder(Cursor::new(data), Some(len))
            .context("decoding the downloaded audio")?
            .total_duration())
    }

    /// Where in the file `to` probably is. Assumes a steady bitrate, which is
    /// close enough for MP3 to know when a seek can go ahead without waiting.
    fn bytes_needed(&self, stream: &Progressive, to: Duration) -> usize {
        let duration_ms = self
            .current
            .lock()
            .expect("player mutex")
            .as_ref()
            .map_or(0, |track| track.duration_ms);
        match stream.total() {
            Some(total) if duration_ms > 0 => {
                let fraction = (to.as_millis() as f64 / duration_ms as f64).min(1.0);
                (total as f64 * fraction) as usize + SEEK_MARGIN_BYTES
            }
            // No way to tell where the time lands: wait for the whole file.
            _ => usize::MAX,
        }
    }
}

/// The thread that carries out seeks into parts still downloading.
///
/// `Sink::try_seek` waits for the audio thread to do the seek, and the decoder
/// would wait there for bytes that have not arrived — holding up both the sound
/// and whoever asked. Here the wait happens first, on a thread of its own, and
/// the seek is issued once the data is in. Only the latest request counts.
fn spawn_seeker(sink: Arc<Sink>, generation: Arc<AtomicU64>) -> Result<mpsc::Sender<SeekJob>> {
    let (tx, rx) = mpsc::channel::<SeekJob>();
    std::thread::Builder::new()
        .name("ymsync-seek".to_string())
        .spawn(move || {
            while let Ok(mut job) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                let arrived = job.stream.wait_for(job.need_bytes, SEEK_WAIT);
                if job.generation != generation.load(Ordering::SeqCst) {
                    continue;
                }
                if arrived {
                    let _ = sink.try_seek(job.to);
                }
            }
        })
        .context("spawning the seek thread")?;
    Ok(tx)
}

impl Playback for Player {
    /// The in-memory buffer is what makes a seek cost a decoder reset instead of
    /// a network round trip.
    fn needs_bytes(&self) -> bool {
        true
    }

    fn plays_while_downloading(&self) -> bool {
        true
    }

    /// Decoding probes the container, so this blocks briefly — and, for a track
    /// still downloading, until its first part is in. The engine calls it from a
    /// blocking task.
    fn load(&self, track: TrackRef, source: AudioSource) -> Result<()> {
        let (decoder, stream): (Box<dyn Source<Item = f32> + Send>, _) = match source {
            AudioSource::Bytes(data) => {
                let len = data.len() as u64;
                let decoder = build_decoder(Cursor::new(data), Some(len))
                    .with_context(|| format!("decoding audio for {track}"))?;
                (Box::new(decoder), None)
            }
            AudioSource::Stream(stream) => {
                let decoder = build_decoder(stream.reader(), stream.total())
                    .with_context(|| format!("decoding audio for {track}"))?;
                (Box::new(decoder), Some(stream))
            }
            AudioSource::Url(_) => bail!("этот плеер играет из памяти или из загрузки, а не по ссылке"),
        };

        self.generation.fetch_add(1, Ordering::SeqCst);
        // `clear` drops the previous source and leaves the sink paused, so the
        // new track cannot start before we have positioned it.
        self.sink.clear();
        self.sink
            .append(crate::eq::EqSource::new(decoder, Arc::clone(&self.equalizer)));
        // The position counter is not reset until the audio thread notices the
        // new source, so force it: otherwise the previous track's playhead is
        // briefly reported against the new track's duration.
        let _ = self.sink.try_seek(Duration::ZERO);
        *self.current.lock().expect("player mutex") = Some(track);
        *self.stream.lock().expect("player mutex") = stream;
        Ok(())
    }

    fn play(&self) {
        self.sink.play();
    }

    fn pause(&self) {
        self.sink.pause();
    }

    fn seek_ms(&self, ms: u64) -> Result<()> {
        let to = Duration::from_millis(ms);
        let pending = self
            .stream
            .lock()
            .expect("player mutex")
            .clone()
            .filter(|stream| !stream.is_done());

        if let Some(stream) = pending {
            let need_bytes = self.bytes_needed(&stream, to);
            if stream.len() < need_bytes {
                // Not downloaded that far yet: the seek thread waits for it, and
                // the caller — the sync loop — carries on meanwhile.
                let _ = self.seeker.send(SeekJob {
                    generation: self.generation.load(Ordering::SeqCst),
                    to,
                    stream,
                    need_bytes,
                });
                return Ok(());
            }
        }

        self.sink
            .try_seek(to)
            .map_err(|err| anyhow!("seeking to {ms} ms failed: {err}"))
    }

    fn position_ms(&self) -> u64 {
        self.sink.get_pos().as_millis() as u64
    }

    fn is_playing(&self) -> bool {
        !self.sink.is_paused() && !self.sink.empty()
    }

    fn is_finished(&self) -> bool {
        self.sink.empty()
    }

    fn current_track_id(&self) -> Option<String> {
        self.current
            .lock()
            .expect("player mutex")
            .as_ref()
            .map(|t| t.track_id.clone())
    }

    fn set_equalizer(&self, gains: &[f32]) {
        self.equalizer.set(gains);
    }

    fn set_volume(&self, volume: f32) {
        self.sink.set_volume(loudness(volume));
    }

    /// The slider's position, not the gain: the inverse of [`loudness`].
    fn volume(&self) -> f32 {
        slider_for(self.sink.volume())
    }
}

fn build_decoder<R>(data: R, byte_len: Option<u64>) -> Result<rodio::Decoder<R>>
where
    R: Read + Seek + Send + Sync + 'static,
{
    let mut builder = DecoderBuilder::new()
        .with_data(data)
        .with_seekable(true)
        .with_hint("mp3");
    // Without the byte length, MP3 seeking and duration are unreliable.
    if let Some(len) = byte_len {
        builder = builder.with_byte_len(len);
    }
    builder.build().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    /// The sync loop shares one player across tasks and blocking calls. If a
    /// future `rodio` release makes `Sink` non-shareable, fail here rather than
    /// at every call site.
    #[test]
    fn player_can_be_shared_across_threads() {
        assert_send_sync::<Player>();
    }

    /// A decoder must be buildable over a download that has only begun: that is
    /// what lets a track start before it is all here.
    #[test]
    fn a_decoder_opens_on_a_partial_download() {
        let frame = [&[0xFF, 0xFB, 0x10, 0xC4][..], &[0u8; 100][..]].concat();
        let stream = Progressive::new(Some((frame.len() * 400) as u64));
        for _ in 0..40 {
            stream.push(&frame);
        }
        let filler = stream.clone();
        let rest = std::thread::spawn(move || {
            for _ in 0..360 {
                filler.push(&frame);
            }
            filler.finish();
        });
        let decoder = build_decoder(stream.reader(), stream.total());
        rest.join().unwrap();
        assert!(decoder.is_ok(), "decoder failed: {:?}", decoder.err());
    }

    /// Needs a sound card, so it runs only when asked: `cargo test -p ymsync
    /// switching_outputs -- --ignored`. Moving between outputs must neither
    /// stop the track nor leave a seek waiting for a stream that is gone,
    /// which is what used to hang every button after headphones were pulled.
    #[test]
    #[ignore]
    fn switching_outputs_keeps_playing_and_answering() {
        let frame = [&[0xFF, 0xFB, 0x10, 0xC4][..], &[0u8; 100][..]].concat();
        let data = Bytes::from(frame.repeat(20_000));
        let player = Player::new(0.0).expect("an audio output");
        let track = TrackRef {
            track_id: "1".to_string(),
            album_id: None,
            album: None,
            title: String::new(),
            artist: String::new(),
            duration_ms: 500_000,
            cover_uri: None,
            added_by: None,
        };
        player.load(track, AudioSource::Bytes(data)).expect("load");
        player.play();

        let mut outputs = output_devices();
        outputs.push("нет такого устройства".to_string());
        outputs.push(String::new());
        for name in outputs {
            set_output_device(&name);
            std::thread::sleep(DEVICE_POLL * 3);
            let before = player.position_ms();
            std::thread::sleep(Duration::from_millis(400));
            assert!(player.position_ms() > before, "stalled on «{name}»");

            let (done_tx, done_rx) = mpsc::channel();
            let to = before + 5_000;
            let sink = Arc::clone(&player.sink);
            std::thread::spawn(move || {
                let _ = done_tx.send(sink.try_seek(Duration::from_millis(to)));
            });
            done_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| panic!("a seek hung on «{name}»"))
                .expect("seek");
        }
    }

    /// The point of it all: sound comes out while the file is still arriving.
    #[test]
    fn audio_decodes_before_the_download_finishes() {
        let frame = [&[0xFF, 0xFB, 0x10, 0xC4][..], &[0u8; 100][..]].concat();
        let total_frames = 400;
        let stream = Progressive::new(Some((frame.len() * total_frames) as u64));
        for _ in 0..40 {
            stream.push(&frame);
        }

        // The rest trickles in later, as from a slow connection.
        let filler = stream.clone();
        let late = frame.clone();
        let rest = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            for _ in 40..total_frames {
                filler.push(&late);
            }
            filler.finish();
        });

        let decoder = build_decoder(stream.reader(), stream.total()).expect("decoder");
        // About a quarter of a second of audio, all from the first part.
        let samples = decoder.take(44_100 / 4).count();
        let arrived = stream.len();
        rest.join().unwrap();

        assert_eq!(samples, 44_100 / 4);
        assert!(
            arrived < frame.len() * total_frames,
            "the download had already finished ({arrived} bytes) — nothing was proven"
        );
    }
}
