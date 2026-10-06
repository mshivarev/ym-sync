//! Desktop audio output.
//!
//! A track is decoded either from memory — a download, a file on disk — or from
//! a download still in progress, so playback starts on the first part instead
//! of after the whole file. Playback speed is deliberately left at 1.0: `rodio`
//! scales its reported position by playback speed, so nudging the rate would
//! corrupt the very measurement the sync loop corrects against — and it would
//! bend the pitch as well.

use std::io::{Cursor, Read, Seek};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use rodio::decoder::DecoderBuilder;
use rodio::{OutputStreamBuilder, Sink, Source};
use ymsync_proto::TrackRef;

use crate::playback::{AudioSource, Playback};
use crate::stream::Progressive;

/// How long a seek into a part still downloading may wait for it.
const SEEK_WAIT: Duration = Duration::from_secs(120);

/// Slack on top of the estimated byte offset of a seek target: the estimate
/// assumes a constant bitrate, and the decoder reads a little ahead.
const SEEK_MARGIN_BYTES: usize = 128 * 1024;

pub struct Player {
    sink: Arc<Sink>,
    current: Mutex<Option<TrackRef>>,
    /// The download the current track plays from, while it is one.
    stream: Mutex<Option<Progressive>>,
    /// Bumped on every load, so a seek that waited for data can tell whether the
    /// track it was meant for is still the one playing.
    generation: Arc<AtomicU64>,
    seeker: mpsc::Sender<SeekJob>,
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
        // cpal's stream is not `Send` on every platform, so the output device is
        // opened on its own thread that then parks, keeping the device open for
        // the life of the process. Only the `Sink` handle crosses threads.
        let (tx, rx) = mpsc::channel::<Result<Sink, String>>();
        std::thread::Builder::new()
            .name("ymsync-audio".to_string())
            .spawn(move || match OutputStreamBuilder::open_default_stream() {
                Err(err) => {
                    let _ = tx.send(Err(err.to_string()));
                }
                Ok(mut stream) => {
                    stream.log_on_drop(false);
                    if tx.send(Ok(Sink::connect_new(stream.mixer()))).is_err() {
                        return;
                    }
                    drop(tx);
                    loop {
                        std::thread::park();
                    }
                }
            })
            .context("spawning the audio thread")?;

        let sink = rx
            .recv()
            .context("the audio thread stopped before handing back a sink")?
            .map_err(|err| anyhow!("cannot open the default audio output: {err}"))?;

        sink.set_volume(volume.clamp(0.0, 2.0));
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
        self.sink.append(decoder);
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

    fn set_volume(&self, volume: f32) {
        self.sink.set_volume(volume.clamp(0.0, 2.0));
    }

    fn volume(&self) -> f32 {
        self.sink.volume()
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
