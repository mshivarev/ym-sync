//! Playlists kept on this device.
//!
//! A Yandex playlist can only hold Yandex tracks, so a listener's own imported
//! files had nowhere to go but the flat list of downloads. These playlists take
//! anything — Yandex tracks and imported files alike — and live beside the
//! downloads, like «Мне нравится».
//!
//! Front ends see them as ordinary [`PlaylistInfo`] cards whose owner is
//! [`LOCAL_OWNER`], so the same page opens both kinds.
//!
//! Every call reads or writes a small JSON file, which is cheap enough for a
//! button press but still disk work: keep it off a UI thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;
use ymsync_proto::TrackRef;

use crate::api::PlaylistInfo;

/// The owner a local playlist reports, in place of a Yandex account id.
pub const LOCAL_OWNER: &str = "local";

/// Where the playlists live, inside the cache directory. Not `.json`, for the
/// same reason as `likes.state`: the cache reads every `<id>.json` there as a
/// track's sidecar.
const FILE: &str = "playlists.state";

/// Longest title kept, in characters.
const MAX_TITLE: usize = 80;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    playlists: Vec<LocalPlaylist>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalPlaylist {
    id: String,
    title: String,
    tracks: Vec<TrackRef>,
}

pub struct Playlists {
    path: PathBuf,
    stored: Mutex<Stored>,
}

impl Playlists {
    /// Reads what was stored in `dir`. An unreadable file is a warning and no
    /// playlists, never a failure.
    pub fn open(dir: impl AsRef<Path>) -> Arc<Self> {
        let path = dir.as_ref().join(FILE);
        let stored = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                warn!(path = %path.display(), "не удалось прочитать плейлисты: {err}");
                Stored::default()
            }),
            Err(_) => Stored::default(),
        };
        Arc::new(Self {
            path,
            stored: Mutex::new(stored),
        })
    }

    /// Every local playlist, as cards. The cover is the first track's that has
    /// one.
    pub fn list(&self) -> Vec<PlaylistInfo> {
        let stored = self.stored.lock().expect("playlists");
        stored
            .playlists
            .iter()
            .map(|playlist| PlaylistInfo {
                owner: LOCAL_OWNER.to_string(),
                kind: playlist.id.clone(),
                title: playlist.title.clone(),
                track_count: playlist.tracks.len() as u32,
                cover_uri: playlist.tracks.iter().find_map(|track| track.cover_uri.clone()),
            })
            .collect()
    }

    pub fn tracks(&self, id: &str) -> Result<Vec<TrackRef>> {
        let stored = self.stored.lock().expect("playlists");
        stored
            .playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.tracks.clone())
            .context("такого плейлиста на этом устройстве нет")
    }

    /// Makes an empty playlist. Answers its card.
    pub fn create(&self, title: &str) -> Result<PlaylistInfo> {
        let title: String = title.trim().chars().take(MAX_TITLE).collect();
        if title.is_empty() {
            bail!("у плейлиста должно быть название");
        }
        let mut stored = self.stored.lock().expect("playlists");
        stored.next_id += 1;
        let id = format!("p{}", stored.next_id);
        stored.playlists.push(LocalPlaylist {
            id: id.clone(),
            title: title.clone(),
            tracks: Vec::new(),
        });
        self.save(&stored)?;
        Ok(PlaylistInfo {
            owner: LOCAL_OWNER.to_string(),
            kind: id,
            title,
            track_count: 0,
            cover_uri: None,
        })
    }

    /// Adds a track to the end. Answers false when it was there already: a
    /// playlist holding the same track twice is almost always a double press.
    pub fn add(&self, id: &str, track: TrackRef) -> Result<bool> {
        let mut stored = self.stored.lock().expect("playlists");
        let playlist = stored
            .playlists
            .iter_mut()
            .find(|playlist| playlist.id == id)
            .context("такого плейлиста на этом устройстве нет")?;
        if playlist.tracks.iter().any(|known| known.track_id == track.track_id) {
            return Ok(false);
        }
        playlist.tracks.push(track);
        self.save(&stored)?;
        Ok(true)
    }

    pub fn remove_track(&self, id: &str, track_id: &str) -> Result<()> {
        let mut stored = self.stored.lock().expect("playlists");
        if let Some(playlist) = stored.playlists.iter_mut().find(|playlist| playlist.id == id) {
            playlist.tracks.retain(|track| track.track_id != track_id);
        }
        self.save(&stored)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let mut stored = self.stored.lock().expect("playlists");
        stored.playlists.retain(|playlist| playlist.id != id);
        self.save(&stored)
    }

    /// Written to a temporary file and renamed over the old one, so a crash
    /// mid-write leaves the previous list rather than half of one.
    fn save(&self, stored: &Stored) -> Result<()> {
        let text = serde_json::to_string(stored).context("serialising playlists")?;
        let temp = self.path.with_extension("state.tmp");
        std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }
}

/// Whether a track is one of the listener's own files rather than Yandex's —
/// see `import`, which names them so.
pub fn is_own_file(track_id: &str) -> bool {
    track_id.starts_with("local_")
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
            added_by: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ymsync-playlists-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_playlist_takes_yandex_tracks_and_own_files_and_survives_a_restart() {
        let dir = temp_dir("roundtrip");
        let playlists = Playlists::open(&dir);
        let card = playlists.create("  В дорогу  ").unwrap();
        assert_eq!(card.owner, LOCAL_OWNER);
        assert_eq!(card.title, "В дорогу");

        assert!(playlists.add(&card.kind, track("123")).unwrap());
        assert!(playlists.add(&card.kind, track("local_abc")).unwrap());
        assert!(!playlists.add(&card.kind, track("123")).unwrap(), "no doubles");

        let reopened = Playlists::open(&dir);
        let ids: Vec<String> = reopened.tracks(&card.kind).unwrap().into_iter().map(|t| t.track_id).collect();
        assert_eq!(ids, ["123", "local_abc"]);
        assert_eq!(reopened.list()[0].track_count, 2);

        reopened.remove_track(&card.kind, "123").unwrap();
        assert_eq!(reopened.tracks(&card.kind).unwrap().len(), 1);
        reopened.delete(&card.kind).unwrap();
        assert!(reopened.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_playlist_needs_a_name() {
        let dir = temp_dir("untitled");
        assert!(Playlists::open(&dir).create("   ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_files_are_told_apart_by_their_id() {
        assert!(is_own_file("local_0123abcd"));
        assert!(!is_own_file("38633706"));
    }
}
