//! The account's «Мне нравится»: the list, the heart state, and both kept on
//! disk.
//!
//! Likes belong to an *account*, not to the room. Each participant listens on
//! their own token, so hearting a track changes nothing for anybody else — which
//! is why none of this goes through the relay.
//!
//! # Why it is stored at all
//!
//! The list is one request away, but only while there is an internet connection,
//! and this app is built to keep playing without one. An unstored list would mean
//! every heart appears empty offline — telling the listener that nothing is liked,
//! which is worse than saying nothing. So the last known list is written beside
//! the downloads and read back at startup.
//!
//! # Which calls block
//!
//! [`Likes::tracks`], [`Likes::contains`] and [`Likes::revision`] are in-memory
//! lookups. [`Likes::open`], [`Likes::replace`] and [`Likes::set`] touch the disk:
//! a thousand liked tracks are a few hundred kilobytes of JSON, so they belong in
//! a task of their own rather than on a UI thread or an engine tick.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use ymsync_proto::TrackRef;

use crate::api::{Track, YandexMusic};

/// Where the list lives, inside the cache directory.
///
/// Deliberately not `likes.json`: the cache treats every `<id>.json` in that
/// directory as a downloaded track's sidecar, so a `.json` here would be read as
/// a damaged entry and deleted on the next startup.
const FILE: &str = "likes.state";

/// What is written to disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    /// When Yandex was last asked, in Unix milliseconds. 0 means never.
    updated_ms: i64,
    tracks: Vec<TrackRef>,
}

/// The in-memory half.
struct Current {
    /// Shared rather than cloned: the whole list goes out to a front end on every
    /// change, and it runs to well over a thousand tracks.
    tracks: Arc<Vec<TrackRef>>,
    ids: HashSet<String>,
    /// Bumped on every change, so a front end can tell whether to redraw without
    /// comparing lists.
    revision: u64,
    updated_ms: i64,
}

pub struct Likes {
    path: PathBuf,
    current: Mutex<Current>,
}

impl Likes {
    /// Reads whatever was last stored in `dir`. A file that cannot be read is a
    /// warning and an empty list, never a failure: this is a convenience, and the
    /// app has to start without it.
    pub fn open(dir: impl AsRef<Path>) -> Arc<Self> {
        let path = dir.as_ref().join(FILE);
        let stored = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Stored>(&text) {
                Ok(stored) => stored,
                Err(err) => {
                    warn!(path = %path.display(), "не удалось прочитать «Мне нравится»: {err}");
                    Stored::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(err) => {
                warn!(path = %path.display(), "не удалось открыть «Мне нравится»: {err}");
                Stored::default()
            }
        };

        debug!(
            path = %path.display(),
            tracks = stored.tracks.len(),
            "«Мне нравится» прочитано с диска"
        );
        Arc::new(Self {
            path,
            current: Mutex::new(Current {
                ids: stored.tracks.iter().map(|t| t.track_id.clone()).collect(),
                tracks: Arc::new(stored.tracks),
                // Starts at 1 so that a front end holding 0 always redraws once.
                revision: 1,
                updated_ms: stored.updated_ms,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current.lock().expect("likes mutex")
    }

    pub fn tracks(&self) -> Arc<Vec<TrackRef>> {
        Arc::clone(&self.lock().tracks)
    }

    pub fn contains(&self, track_id: &str) -> bool {
        self.lock().ids.contains(track_id)
    }

    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    pub fn count(&self) -> usize {
        self.lock().tracks.len()
    }

    /// When Yandex was last asked, in Unix milliseconds. 0 means never, which is
    /// what a front end shows as «список ещё не загружен».
    pub fn updated_ms(&self) -> i64 {
        self.lock().updated_ms
    }

    /// Asks Yandex for the whole list and stores it. Returns how many there are.
    ///
    /// The full playlist rather than the bare ids `/likes/tracks` would give: the
    /// list is also a place to play from, and ids alone could not be shown or
    /// queued without a second round of requests.
    pub async fn refresh(&self, api: &YandexMusic) -> Result<usize> {
        let tracks = api
            .liked_tracks()
            .await
            .context("не удалось получить «Мне нравится»")?;
        let tracks: Vec<TrackRef> = tracks.iter().map(Track::to_track_ref).collect();
        let count = tracks.len();
        self.replace(tracks);
        Ok(count)
    }

    /// Adds the track to «Мне нравится» or takes it out, on Yandex and here.
    ///
    /// Yandex first: a heart that lights up locally and was refused by the server
    /// is a lie that survives until the next refresh. The track is passed whole so
    /// that a like made from the player also puts a playable row into the list —
    /// otherwise the list would show what was liked only after a refresh.
    pub async fn set(&self, api: &YandexMusic, track: &TrackRef, liked: bool) -> Result<()> {
        api.set_liked(&track.track_id, liked).await?;
        self.apply(track, liked);
        Ok(())
    }

    /// The half of [`Likes::set`] that happens once the server has agreed.
    fn apply(&self, track: &TrackRef, liked: bool) {
        let stored = {
            let mut current = self.lock();
            let mut tracks = current.tracks.as_ref().clone();
            if liked {
                current.ids.insert(track.track_id.clone());
                // Newest first, matching the order Yandex answers in.
                if !tracks.iter().any(|t| t.track_id == track.track_id) {
                    tracks.insert(0, track.clone());
                }
            } else {
                current.ids.remove(&track.track_id);
                tracks.retain(|t| t.track_id != track.track_id);
            }
            current.tracks = Arc::new(tracks);
            current.revision += 1;
            Stored {
                updated_ms: current.updated_ms,
                tracks: current.tracks.as_ref().clone(),
            }
        };
        self.save(&stored);
    }

    /// Replaces the list with what the server just said.
    fn replace(&self, tracks: Vec<TrackRef>) {
        let stored = {
            let mut current = self.lock();
            current.ids = tracks.iter().map(|t| t.track_id.clone()).collect();
            current.tracks = Arc::new(tracks);
            current.revision += 1;
            current.updated_ms = ymsync_proto::unix_ms();
            Stored {
                updated_ms: current.updated_ms,
                tracks: current.tracks.as_ref().clone(),
            }
        };
        self.save(&stored);
    }

    /// Writes the list out. A failure here costs offline hearts, not the like
    /// itself — which is already on the server — so it is reported and not raised.
    fn save(&self, stored: &Stored) {
        let text = match serde_json::to_string(stored) {
            Ok(text) => text,
            Err(err) => {
                warn!("не удалось сериализовать «Мне нравится»: {err}");
                return;
            }
        };
        if let Err(err) = std::fs::write(&self.path, text) {
            warn!(path = %self.path.display(), "не удалось сохранить «Мне нравится»: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir()
                .join(format!("ymsync-likes-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the directory");
            TempDir(dir)
        }
    }

    fn track(id: &str) -> TrackRef {
        TrackRef {
            track_id: id.to_string(),
            album_id: Some("7".to_string()),
            album: Some("Альбом".to_string()),
            title: format!("Title {id}"),
            artist: "Artist".to_string(),
            duration_ms: 200_000,
            cover_uri: None,
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_list_rather_than_an_error() {
        let dir = TempDir::new("absent");
        let likes = Likes::open(&dir.0);
        assert_eq!(likes.count(), 0);
        assert!(!likes.contains("42"));
        assert_eq!(likes.updated_ms(), 0);
    }

    /// The whole point of storing it: hearts still mean something with no
    /// internet, and the list can be played from.
    #[test]
    fn the_list_survives_a_restart() {
        let dir = TempDir::new("restart");
        let likes = Likes::open(&dir.0);
        likes.replace(vec![track("42"), track("43")]);

        let reopened = Likes::open(&dir.0);
        assert_eq!(reopened.count(), 2);
        assert!(reopened.contains("42"));
        assert_eq!(reopened.tracks()[0].title, "Title 42");
        assert!(reopened.updated_ms() > 0, "the refresh time is recorded");
    }

    #[test]
    fn a_damaged_file_is_ignored_rather_than_fatal() {
        let dir = TempDir::new("damaged");
        std::fs::write(dir.0.join(FILE), "{ not json").unwrap();
        assert_eq!(Likes::open(&dir.0).count(), 0);
    }

    /// The stored file must not look like a downloaded track's sidecar, or the
    /// cache would delete it as a damaged entry on the next startup.
    #[test]
    fn the_state_file_is_not_mistaken_for_a_sidecar() {
        let dir = TempDir::new("sidecar");
        let likes = Likes::open(&dir.0);
        likes.replace(vec![track("42")]);

        let cache = crate::cache::Cache::open(&dir.0, 0).expect("open the cache");
        assert_eq!(cache.count(), 0, "the state file is not a track");
        assert!(dir.0.join(FILE).exists(), "and it is still there");
        assert_eq!(Likes::open(&dir.0).count(), 1);
    }

    /// A like made from the player has to show up in the list without waiting for
    /// a refresh, and an unlike has to disappear from it.
    #[test]
    fn liking_and_unliking_change_the_list_in_place() {
        let dir = TempDir::new("toggle");
        let likes = Likes::open(&dir.0);
        likes.replace(vec![track("42")]);
        let before = likes.revision();

        likes.apply(&track("77"), true);
        assert!(likes.contains("77"));
        assert_eq!(likes.tracks()[0].track_id, "77", "newest first");
        assert!(likes.revision() > before, "a change is visible as a revision");

        // Liking the same track twice must not list it twice.
        likes.apply(&track("77"), true);
        assert_eq!(likes.count(), 2);

        likes.apply(&track("42"), false);
        assert!(!likes.contains("42"));
        assert_eq!(likes.count(), 1);

        // And it is on disk, not only in memory.
        assert_eq!(Likes::open(&dir.0).tracks()[0].track_id, "77");
    }
}
