//! A track that can be played before it has finished downloading.
//!
//! The download appends to a shared buffer; the decoder reads from it through an
//! ordinary `Read + Seek`, and a read past what has arrived waits for the rest.
//! That is all a music player needs to start within a moment of pressing play
//! instead of after the whole file is in memory.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

/// How long a read waits for bytes that have not arrived before giving up. Long
/// enough to ride out a phone switching networks; short enough that a dead
/// connection ends the track rather than hanging the audio thread for good.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A download in progress, shared between the task filling it and the decoder
/// reading it. Cloning shares the same buffer.
#[derive(Clone)]
pub struct Progressive {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    data: Vec<u8>,
    /// From the response's Content-Length, when the server sent one.
    total: Option<u64>,
    done: bool,
    /// Why the download stopped short, if it did — including being cancelled.
    failed: Option<String>,
}

impl Progressive {
    pub fn new(total: Option<u64>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    data: Vec::with_capacity(total.unwrap_or(0).min(64 << 20) as usize),
                    total,
                    done: false,
                    failed: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().expect("stream mutex")
    }

    fn notify(&self) {
        self.shared.changed.notify_all();
    }

    /// Appends what just arrived.
    pub fn push(&self, chunk: &[u8]) {
        self.state().data.extend_from_slice(chunk);
        self.notify();
    }

    /// The whole file is in.
    pub fn finish(&self) {
        self.state().done = true;
        self.notify();
    }

    /// The download stopped short. Readers waiting for more get the error.
    pub fn fail(&self, reason: impl Into<String>) {
        let mut state = self.state();
        if !state.done && state.failed.is_none() {
            state.failed = Some(reason.into());
        }
        drop(state);
        self.notify();
    }

    /// Stops the download: the track is no longer wanted.
    pub fn cancel(&self) {
        self.fail("загрузка отменена");
    }

    /// Whether two handles share one download.
    pub fn same_as(&self, other: &Progressive) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn is_cancelled_or_failed(&self) -> bool {
        self.state().failed.is_some()
    }

    pub fn is_done(&self) -> bool {
        self.state().done
    }

    pub fn len(&self) -> usize {
        self.state().data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total(&self) -> Option<u64> {
        self.state().total
    }

    /// The whole file, once it is in. What gets written to the cache.
    pub fn complete_bytes(&self) -> Option<Bytes> {
        let state = self.state();
        state.done.then(|| Bytes::copy_from_slice(&state.data))
    }

    /// Waits until at least `bytes` have arrived, or the download ended either
    /// way, or `timeout` passed. Answers whether the bytes are there.
    pub fn wait_for(&self, bytes: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state();
        loop {
            if state.data.len() >= bytes || state.done {
                return state.data.len() >= bytes || state.done;
            }
            if state.failed.is_some() {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.shared.changed.wait_timeout(state, left).expect("stream mutex").0;
        }
    }

    /// A reader for the decoder, starting at the beginning.
    pub fn reader(&self) -> Reader {
        Reader {
            stream: self.clone(),
            position: 0,
        }
    }
}

/// `Read + Seek` over a [`Progressive`]: reads past the downloaded part wait.
pub struct Reader {
    stream: Progressive,
    position: u64,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut state = self.stream.state();
        loop {
            let available = state.data.len() as u64;
            if self.position < available {
                let start = self.position as usize;
                let count = buf.len().min(state.data.len() - start);
                buf[..count].copy_from_slice(&state.data[start..start + count]);
                self.position += count as u64;
                return Ok(count);
            }
            if state.done {
                return Ok(0);
            }
            if let Some(reason) = &state.failed {
                return Err(io::Error::other(reason.clone()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "трек не догрузился"));
            }
            state = self
                .stream
                .shared
                .changed
                .wait_timeout(state, left)
                .expect("stream mutex")
                .0;
        }
    }
}

impl Seek for Reader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => self.position as i64 + delta,
            SeekFrom::End(delta) => {
                // The end is known from Content-Length; without one, only once the
                // download is complete.
                let end = match self.stream.total() {
                    Some(total) => total,
                    None => {
                        self.stream.wait_for(usize::MAX, READ_TIMEOUT);
                        self.stream.len() as u64
                    }
                };
                end as i64 + delta
            }
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        // Moving the cursor costs nothing; the next read waits if it has to.
        self.position = target as u64;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_returns_what_has_arrived_and_waits_for_the_rest() {
        let stream = Progressive::new(Some(6));
        stream.push(b"abc");
        let mut reader = stream.reader();

        let mut buf = [0u8; 6];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");

        // The rest arrives from another thread while the reader is waiting.
        let filler = stream.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            filler.push(b"def");
            filler.finish();
        });
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        handle.join().unwrap();
        assert_eq!(rest, b"def");
    }

    #[test]
    fn seeking_ahead_waits_only_when_read() {
        let stream = Progressive::new(Some(10));
        stream.push(b"0123");
        let mut reader = stream.reader();

        // Past what is downloaded: the seek itself is immediate.
        assert_eq!(reader.seek(SeekFrom::Start(8)).unwrap(), 8);
        assert_eq!(reader.seek(SeekFrom::End(-1)).unwrap(), 9);

        stream.push(b"456789");
        stream.finish();
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"9");
    }

    #[test]
    fn a_failed_download_ends_the_read_with_an_error() {
        let stream = Progressive::new(None);
        stream.push(b"ab");
        let mut reader = stream.reader();
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();

        stream.cancel();
        assert!(reader.read(&mut buf).is_err());
        assert!(stream.is_cancelled_or_failed());
        assert!(stream.complete_bytes().is_none());
    }

    #[test]
    fn wait_for_answers_when_enough_has_arrived() {
        let stream = Progressive::new(None);
        assert!(!stream.wait_for(4, Duration::from_millis(20)));
        stream.push(b"abcd");
        assert!(stream.wait_for(4, Duration::from_millis(20)));
        stream.finish();
        // Once complete, asking for more than there is still returns.
        assert!(stream.wait_for(100, Duration::from_millis(20)));
        assert_eq!(stream.complete_bytes().unwrap().as_ref(), b"abcd");
    }
}
