//! What this device has played, newest first.
//!
//! Written by the engine each time the room moves to a new track, read by the
//! front ends for the collection's «История». Kept beside the downloads like
//! «Мне нравится», so it survives a restart and needs no internet to show.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;
use ymsync_proto::TrackRef;

/// Where the history lives, inside the cache directory. Not `.json`: the cache
/// reads every `<id>.json` there as a track's sidecar.
const FILE: &str = "history.state";

/// How many tracks are remembered.
const LIMIT: usize = 200;

/// One play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Played {
    pub track: TrackRef,
    /// When the room moved to it, in Unix milliseconds.
    pub at_ms: i64,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Everything remembered, newest first. Nothing stored yet, or a file that
/// cannot be read, is an empty history.
pub fn read(dir: impl AsRef<Path>) -> Vec<Played> {
    std::fs::read_to_string(path(dir.as_ref()))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Puts a track at the top. The same track played again moves up rather than
/// appearing twice in a row — but an earlier play further down stays.
pub fn record(dir: impl AsRef<Path>, track: &TrackRef, at_ms: i64) {
    let dir = dir.as_ref();
    let mut played = read(dir);
    if played.first().is_some_and(|last| last.track.track_id == track.track_id) {
        played.remove(0);
    }
    let mut track = track.clone();
    // Who queued it was about that room at that moment, not about the track.
    track.added_by = None;
    played.insert(0, Played { track, at_ms });
    played.truncate(LIMIT);
    let write = || -> std::io::Result<()> {
        let temp = path(dir).with_extension("state.tmp");
        std::fs::write(&temp, serde_json::to_string(&played)?)?;
        std::fs::rename(&temp, path(dir))
    };
    if let Err(err) = write() {
        debug!("не удалось записать историю: {err}");
    }
}

/// Forgets everything.
pub fn clear(dir: impl AsRef<Path>) -> std::io::Result<()> {
    match std::fs::remove_file(path(dir.as_ref())) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> TrackRef {
        TrackRef {
            track_id: id.into(),
            album_id: None,
            album: None,
            title: format!("t{id}"),
            artist: "a".into(),
            duration_ms: 1_000,
            cover_uri: None,
            added_by: Some("Аня".into()),
        }
    }

    #[test]
    fn newest_first_without_immediate_repeats_and_within_the_limit() {
        let dir = std::env::temp_dir().join(format!("ymsync-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        record(&dir, &track("1"), 1);
        record(&dir, &track("2"), 2);
        record(&dir, &track("2"), 3);
        record(&dir, &track("1"), 4);
        let ids: Vec<String> = read(&dir).into_iter().map(|p| p.track.track_id).collect();
        assert_eq!(ids, ["1", "2", "1"]);
        assert_eq!(read(&dir)[0].track.added_by, None, "who queued it is not kept");

        for n in 0..(LIMIT + 10) {
            record(&dir, &track(&n.to_string()), n as i64);
        }
        assert_eq!(read(&dir).len(), LIMIT);

        clear(&dir).unwrap();
        assert!(read(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
