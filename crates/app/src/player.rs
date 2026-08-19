//! Desktop audio output.
//!
//! The whole track is decoded from memory, which makes seeking cheap and keeps
//! drift correction free of network jitter. Playback speed is deliberately left
//! at 1.0: `rodio` scales its reported position by playback speed, so nudging
//! the rate would corrupt the very measurement the sync loop corrects against —
//! and it would bend the pitch as well.

use std::io::Cursor;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use rodio::decoder::DecoderBuilder;
use rodio::{OutputStreamBuilder, Sink, Source};
use ymsync_proto::TrackRef;

use crate::playback::{AudioSource, Playback};

pub struct Player {
    sink: Sink,
    current: Mutex<Option<TrackRef>>,
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

        Ok(Self {
            sink,
            current: Mutex::new(None),
        })
    }

    /// Decodes without opening an audio device, returning the track's duration
    /// when the decoder can determine it. Used by `ymsync probe` to prove the
    /// downloaded bytes are really playable audio.
    pub fn probe_decode(data: Bytes) -> Result<Option<Duration>> {
        Ok(build_decoder(data)
            .context("decoding the downloaded audio")?
            .total_duration())
    }
}

impl Playback for Player {
    /// The in-memory buffer is what makes a seek cost a decoder reset instead of
    /// a network round trip.
    fn needs_bytes(&self) -> bool {
        true
    }

    /// Decoding probes the container, so this blocks briefly; the engine calls
    /// it from a blocking task.
    fn load(&self, track: TrackRef, source: AudioSource) -> Result<()> {
        let AudioSource::Bytes(data) = source else {
            bail!("этот плеер играет только из памяти, а не по ссылке");
        };
        let decoder = build_decoder(data).with_context(|| format!("decoding audio for {track}"))?;

        // `clear` drops the previous source and leaves the sink paused, so the
        // new track cannot start before we have positioned it.
        self.sink.clear();
        self.sink.append(decoder);
        // The position counter is not reset until the audio thread notices the
        // new source, so force it: otherwise the previous track's playhead is
        // briefly reported against the new track's duration.
        let _ = self.sink.try_seek(Duration::ZERO);
        *self.current.lock().expect("player mutex") = Some(track);
        Ok(())
    }

    fn play(&self) {
        self.sink.play();
    }

    fn pause(&self) {
        self.sink.pause();
    }

    fn seek_ms(&self, ms: u64) -> Result<()> {
        self.sink
            .try_seek(Duration::from_millis(ms))
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

fn build_decoder(data: Bytes) -> Result<rodio::Decoder<Cursor<Bytes>>> {
    let byte_len = data.len() as u64;
    DecoderBuilder::new()
        .with_data(Cursor::new(data))
        // Without the byte length, MP3 seeking and duration are unreliable.
        .with_byte_len(byte_len)
        .with_seekable(true)
        .with_hint("mp3")
        .build()
        .map_err(Into::into)
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
}
