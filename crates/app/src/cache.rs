//! On-disk cache of downloaded tracks: what makes offline listening possible and
//! what a peer serves to the rest of the room.
//!
//! One track is two files — `<id>.mp3` and `<id>.json` beside it. The sidecar
//! holds the metadata Yandex would otherwise have to be asked for, and that is
//! the whole trick behind offline play: with the title, artist and duration on
//! disk, a queue can be built and a room can run with no network at all.
//!
//! # Which calls block
//!
//! The index lives in memory, so [`Cache::has`], [`Cache::track`], [`Cache::ids`]
//! and [`Cache::total_bytes`] are cheap lookups. [`Cache::read`],
//! [`Cache::insert`], [`Cache::remove`] and [`Cache::touch`] all touch the disk
//! and must be called from a blocking context — the Android front end polls the
//! engine on its UI thread, so a stray file read there is a visible stutter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use ymsync_proto::TrackRef;

/// Extension of the audio itself.
const AUDIO_EXT: &str = "mp3";
/// Extension of the metadata beside it.
const META_EXT: &str = "json";
/// A download in progress. Never counts as cached, and is swept up on startup.
const PART_EXT: &str = "part";

const MAX_ID_LEN: usize = 64;

/// Whether a track id is safe to use as a file name.
///
/// Ids come off the wire — from the relay's queue and from a peer's HTTP request
/// — so this is the boundary that stops `../../something` from being treated as
/// an id. Yandex's ids are plain digits; the few extra characters allowed here
/// cost nothing and keep the check from being brittle.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What is stored beside a cached track.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sidecar {
    track: TrackRef,
    /// Length of the audio file, checked on every read: a truncated file is the
    /// realistic way for a cache entry to go bad.
    bytes: u64,
    /// MD5 of the audio, so a peer receiving these bytes can tell they arrived
    /// intact. Not a security measure — the room already shares a secret — but it
    /// turns silent corruption into a clear error.
    digest: String,
    /// Downloaded on purpose rather than picked up in passing. Pinned entries are
    /// never evicted to make room: the user asked for these to be there.
    pinned: bool,
    /// When this track last played, in Unix milliseconds. Drives eviction.
    last_played_ms: i64,
}

#[derive(Debug, Clone)]
struct Entry {
    sidecar: Sidecar,
}

/// One cached track, as a front end wants to list it.
#[derive(Debug, Clone, Serialize)]
pub struct CachedTrack {
    #[serde(flatten)]
    pub track: TrackRef,
    pub bytes: u64,
    pub pinned: bool,
}

/// What an [`Cache::insert`] had to do to fit the new track in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insertion {
    pub digest: String,
    /// How many older tracks were dropped to make room.
    pub evicted: usize,
    /// The cache is over its limit and nothing evictable is left — every
    /// remaining track was downloaded on purpose. Worth telling the user, since
    /// the limit is now being ignored.
    pub over_limit: bool,
}

pub struct Cache {
    dir: PathBuf,
    /// 0 means no limit. Atomic so the settings page can change it while the
    /// engine is writing tracks.
    limit_bytes: std::sync::atomic::AtomicU64,
    index: Mutex<HashMap<String, Entry>>,
}

impl Cache {
    /// Opens (creating if needed) the cache directory and reads what is in it.
    ///
    /// Half-finished and orphaned files are cleaned up here rather than being
    /// left to confuse a later read: an interrupted download leaves a `.part`,
    /// and a crash between writing the audio and writing its sidecar leaves audio
    /// nothing can identify.
    pub fn open(dir: impl Into<PathBuf>, limit_bytes: u64) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the track cache at {}", dir.display()))?;
        let index = scan(&dir)?;
        debug!(
            dir = %dir.display(),
            tracks = index.len(),
            "track cache opened"
        );
        Ok(Self {
            dir,
            limit_bytes: std::sync::atomic::AtomicU64::new(limit_bytes),
            index: Mutex::new(index),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A new limit, honoured from the next track written: lowering it does not
    /// delete anything on the spot.
    pub fn set_limit_bytes(&self, limit_bytes: u64) {
        self.limit_bytes
            .store(limit_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    fn audio_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{AUDIO_EXT}"))
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{META_EXT}"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.index.lock().expect("track cache mutex")
    }

    pub fn has(&self, id: &str) -> bool {
        self.lock().contains_key(id)
    }

    /// The metadata for a cached track. This is what lets a queue be built with
    /// no network: the answer Yandex would have given is already on disk.
    pub fn track(&self, id: &str) -> Option<TrackRef> {
        self.lock().get(id).map(|e| e.sidecar.track.clone())
    }

    pub fn digest(&self, id: &str) -> Option<String> {
        self.lock().get(id).map(|e| e.sidecar.digest.clone())
    }

    /// Size of the cached audio, as recorded when it was stored.
    pub fn byte_len(&self, id: &str) -> Option<u64> {
        self.lock().get(id).map(|e| e.sidecar.bytes)
    }

    /// Every cached id, sorted. The order matters: this list is announced to the
    /// room, and an unstable one would look like a change on every announcement.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// The offline library, newest first, which is the order a listener expects
    /// to find their downloads in.
    pub fn tracks(&self) -> Vec<CachedTrack> {
        let mut entries: Vec<&Entry> = Vec::new();
        let index = self.lock();
        entries.extend(index.values());
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.sidecar.last_played_ms));
        entries
            .into_iter()
            .map(|e| CachedTrack {
                track: e.sidecar.track.clone(),
                bytes: e.sidecar.bytes,
                pinned: e.sidecar.pinned,
            })
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.lock().values().map(|e| e.sidecar.bytes).sum()
    }

    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// A `file://` URL for a backend that streams rather than being handed bytes.
    /// ExoPlayer plays these directly, so a cached track needs no HTTP at all.
    pub fn file_url(&self, id: &str) -> Option<String> {
        if !self.has(id) {
            return None;
        }
        Some(file_url(&self.audio_path(id)))
    }

    /// Reads a cached track. Blocking.
    ///
    /// A length that disagrees with the sidecar means the file is damaged, so the
    /// entry is dropped instead of handing a decoder something that will fail
    /// halfway through a song.
    pub fn read(&self, id: &str) -> Result<Bytes> {
        let expected = self
            .lock()
            .get(id)
            .map(|e| e.sidecar.bytes)
            .with_context(|| format!("трека {id} нет в кеше"))?;

        let path = self.audio_path(id);
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if data.len() as u64 != expected {
            self.forget(id);
            bail!(
                "кешированный файл трека {id} повреждён ({} байт вместо {expected}) — \
                 удалил его из кеша, трек скачается заново",
                data.len()
            );
        }
        Ok(Bytes::from(data))
    }

    /// Stores a track. Blocking.
    ///
    /// The audio is written to a `.part` file and renamed into place, and only
    /// then is the sidecar written. That order is what makes a cache entry
    /// all-or-nothing: nothing is considered cached until its metadata exists, so
    /// a crash mid-download can leave litter but never a plausible-looking half
    /// of a track.
    pub fn insert(&self, track: &TrackRef, data: &[u8], pinned: bool) -> Result<Insertion> {
        let id = &track.track_id;
        if !valid_id(id) {
            bail!("«{id}» не годится как имя файла для кеша");
        }
        if data.is_empty() {
            bail!("нечего кешировать: пустые данные трека {id}");
        }

        let digest = digest_of(data);
        let part = self.dir.join(format!("{id}.{PART_EXT}"));
        std::fs::write(&part, data).with_context(|| format!("writing {}", part.display()))?;

        let audio = self.audio_path(id);
        if let Err(err) = std::fs::rename(&part, &audio) {
            let _ = std::fs::remove_file(&part);
            return Err(err).with_context(|| format!("renaming into {}", audio.display()));
        }

        let sidecar = Sidecar {
            track: track.clone(),
            bytes: data.len() as u64,
            digest: digest.clone(),
            // Re-downloading something that was pinned must not quietly unpin it.
            pinned: pinned || self.lock().get(id).is_some_and(|e| e.sidecar.pinned),
            last_played_ms: ymsync_proto::unix_ms(),
        };
        self.write_sidecar(id, &sidecar)?;
        self.lock().insert(
            id.clone(),
            Entry {
                sidecar: sidecar.clone(),
            },
        );

        let (evicted, over_limit) = self.evict_to_fit(id);
        Ok(Insertion {
            digest,
            evicted,
            over_limit,
        })
    }

    /// Notes that a track just played, so eviction can tell stale from useful.
    /// Blocking, but a single small write — call it on a track change, not on a
    /// correction tick.
    pub fn touch(&self, id: &str) {
        let updated = {
            let mut index = self.lock();
            match index.get_mut(id) {
                None => return,
                Some(entry) => {
                    entry.sidecar.last_played_ms = ymsync_proto::unix_ms();
                    entry.sidecar.clone()
                }
            }
        };
        if let Err(err) = self.write_sidecar(id, &updated) {
            // Losing a timestamp only makes eviction less well informed, so it is
            // not worth failing playback over.
            debug!(track = id, "could not record when the track played: {err:#}");
        }
    }

    /// Writes album names into the metadata of the downloads they belong to.
    /// Blocking: one small file per track that changed.
    ///
    /// This is what makes grouping the library by album work offline — the names
    /// are fetched once, while there is a connection, and then live beside the
    /// audio like the rest of a track's metadata. Returns how many tracks changed.
    pub fn name_albums(&self, titles: &HashMap<String, String>) -> Result<usize> {
        // The sidecars are collected under the lock and written outside it: the
        // index is read by the engine on every tick.
        let updated: Vec<(String, Sidecar)> = {
            let mut index = self.lock();
            index
                .iter_mut()
                .filter_map(|(id, entry)| {
                    let album_id = entry.sidecar.track.album_id.as_deref()?;
                    let title = titles.get(album_id)?;
                    if entry.sidecar.track.album.as_deref() == Some(title.as_str()) {
                        return None;
                    }
                    entry.sidecar.track.album = Some(title.clone());
                    Some((id.clone(), entry.sidecar.clone()))
                })
                .collect()
        };

        for (id, sidecar) in &updated {
            self.write_sidecar(id, sidecar)?;
        }
        Ok(updated.len())
    }

    /// Deletes a track from the cache. Blocking.
    pub fn remove(&self, id: &str) -> Result<()> {
        if !valid_id(id) {
            bail!("«{id}» не похоже на id трека");
        }
        self.lock().remove(id);
        let audio = self.audio_path(id);
        let meta = self.meta_path(id);
        // Metadata first: while it is gone the audio is already not cached, so a
        // crash between the two cannot leave a track that looks present.
        let _ = std::fs::remove_file(&meta);
        std::fs::remove_file(&audio)
            .or_else(ignore_missing)
            .with_context(|| format!("deleting {}", audio.display()))?;
        Ok(())
    }

    /// Drops the entry from the index without touching the disk. Used when a file
    /// turns out to be damaged, where the caller is about to report why.
    fn forget(&self, id: &str) {
        self.lock().remove(id);
        let _ = std::fs::remove_file(self.meta_path(id));
        let _ = std::fs::remove_file(self.audio_path(id));
    }

    fn write_sidecar(&self, id: &str, sidecar: &Sidecar) -> Result<()> {
        let path = self.meta_path(id);
        let text = serde_json::to_string_pretty(sidecar).context("serialising track metadata")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Evicts least-recently-played tracks until the cache is under its limit.
    ///
    /// Pinned tracks are never taken, and neither is `keep` — the track that has
    /// just been stored, which would otherwise be the first thing thrown out when
    /// a single download already exceeds the limit. Returns how many went, and
    /// whether the cache is still over its limit with nothing left to give.
    fn evict_to_fit(&self, keep: &str) -> (usize, bool) {
        let limit_bytes = self.limit_bytes();
        if limit_bytes == 0 {
            return (0, false);
        }

        let mut evicted = 0;
        loop {
            let (total, oldest) = {
                let index = self.lock();
                let total: u64 = index.values().map(|e| e.sidecar.bytes).sum();
                if total <= limit_bytes {
                    return (evicted, false);
                }
                let oldest = index
                    .iter()
                    .filter(|(id, entry)| id.as_str() != keep && !entry.sidecar.pinned)
                    .min_by_key(|(_, entry)| entry.sidecar.last_played_ms)
                    .map(|(id, _)| id.clone());
                (total, oldest)
            };

            let Some(id) = oldest else {
                warn!(
                    total,
                    limit = limit_bytes,
                    "cache is over its limit but everything left was downloaded on purpose"
                );
                return (evicted, true);
            };

            match self.remove(&id) {
                Ok(()) => {
                    debug!(track = %id, "evicted to stay within the cache limit");
                    evicted += 1;
                }
                // Nothing else will make this file go away, so stop rather than
                // spin on it for ever.
                Err(err) => {
                    warn!(track = %id, "could not evict: {err:#}");
                    return (evicted, true);
                }
            }
        }
    }
}

fn ignore_missing(err: std::io::Error) -> std::io::Result<()> {
    if err.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(err)
    }
}

/// MD5 of some audio, as stored in a sidecar and advertised to a peer.
///
/// Not a security measure — the room already shares a secret — but it turns a
/// file that went bad on one machine into a clear error on another, instead of a
/// track that plays as noise.
pub(crate) fn digest_of(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// A `file://` URL for a local path, with the separators and drive letter shape
/// Windows needs (`file:///C:/…`).
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Reads the directory and reconciles audio against metadata, cleaning up
/// anything that cannot be a complete cache entry.
fn scan(dir: &Path) -> Result<HashMap<String, Entry>> {
    let mut audio: HashMap<String, u64> = HashMap::new();
    let mut meta: HashMap<String, Sidecar> = HashMap::new();

    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warn!("skipping an unreadable cache entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");

        match extension {
            // An interrupted download. There is nothing to salvage: the tail is
            // missing and the sidecar was never written.
            PART_EXT => {
                let _ = std::fs::remove_file(&path);
            }
            AUDIO_EXT if valid_id(stem) => {
                if let Ok(metadata) = entry.metadata() {
                    audio.insert(stem.to_string(), metadata.len());
                }
            }
            META_EXT if valid_id(stem) => match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<Sidecar>(&text) {
                    Ok(sidecar) => {
                        meta.insert(stem.to_string(), sidecar);
                    }
                    Err(err) => warn!(track = stem, "unreadable track metadata: {err}"),
                },
                Err(err) => warn!(track = stem, "cannot read track metadata: {err}"),
            },
            _ => {}
        }
    }

    let mut index = HashMap::new();
    for (id, sidecar) in meta {
        match audio.remove(&id) {
            // The sidecar is the record of what was written, so a file of a
            // different length is damaged rather than merely surprising.
            Some(len) if len == sidecar.bytes => {
                index.insert(id, Entry { sidecar });
            }
            Some(len) => {
                warn!(
                    track = %id,
                    len, expected = sidecar.bytes,
                    "cached audio has the wrong length; dropping it"
                );
                let _ = std::fs::remove_file(dir.join(format!("{id}.{AUDIO_EXT}")));
                let _ = std::fs::remove_file(dir.join(format!("{id}.{META_EXT}")));
            }
            None => {
                let _ = std::fs::remove_file(dir.join(format!("{id}.{META_EXT}")));
            }
        }
    }
    // Whatever audio is left has no metadata, so nothing can say what track it
    // is — which is exactly the state a crash between the two writes leaves.
    for id in audio.keys() {
        warn!(track = %id, "cached audio with no metadata; dropping it");
        let _ = std::fs::remove_file(dir.join(format!("{id}.{AUDIO_EXT}")));
    }

    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache in a temporary directory, removed when the test ends.
    struct TempCache {
        cache: Cache,
        dir: PathBuf,
    }

    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl TempCache {
        fn with_limit(name: &str, limit_bytes: u64) -> TempCache {
            let dir = std::env::temp_dir().join(format!(
                "ymsync-cache-test-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let cache = Cache::open(&dir, limit_bytes).expect("open the cache");
            TempCache { cache, dir }
        }

        fn new(name: &str) -> TempCache {
            Self::with_limit(name, 0)
        }

        /// Re-opens the same directory, as a restart would.
        fn reopen(&self) -> Cache {
            Cache::open(&self.dir, self.cache.limit_bytes()).expect("reopen the cache")
        }
    }

    fn track(id: &str) -> TrackRef {
        TrackRef {
            track_id: id.to_string(),
            album_id: Some("7".to_string()),
            album: None,
            title: format!("Title {id}"),
            artist: "Artist".to_string(),
            duration_ms: 200_000,
            cover_uri: None,
        }
    }

    #[test]
    fn a_stored_track_comes_back_byte_for_byte() {
        let temp = TempCache::new("roundtrip");
        temp.cache.insert(&track("42"), b"audio bytes", true).unwrap();

        assert!(temp.cache.has("42"));
        assert_eq!(temp.cache.read("42").unwrap().as_ref(), b"audio bytes");
        assert_eq!(temp.cache.total_bytes(), 11);
    }

    /// The point of the sidecar: after a restart the cache can name its tracks
    /// without asking Yandex, which is what offline play is built on.
    #[test]
    fn metadata_survives_a_restart_so_a_queue_can_be_built_offline() {
        let temp = TempCache::new("restart");
        temp.cache.insert(&track("42"), b"bytes", true).unwrap();

        let reopened = temp.reopen();
        let found = reopened.track("42").expect("the track is still known");
        assert_eq!(found.title, "Title 42");
        assert_eq!(found.duration_ms, 200_000);
        assert_eq!(reopened.count(), 1);
    }

    #[test]
    fn an_unknown_track_is_absent_rather_than_an_error_to_look_up() {
        let temp = TempCache::new("absent");
        assert!(!temp.cache.has("nope"));
        assert_eq!(temp.cache.track("nope"), None);
        assert_eq!(temp.cache.file_url("nope"), None);
        assert!(temp.cache.read("nope").is_err());
    }

    /// Ids arrive from the relay's queue and from a peer's HTTP request, so this
    /// is the boundary that keeps one from reaching outside the cache directory.
    #[test]
    fn ids_that_could_escape_the_directory_are_refused() {
        assert!(valid_id("38633712"));
        assert!(valid_id("abc-DEF_123"));

        assert!(!valid_id(""));
        assert!(!valid_id(".."));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a\\b"));
        assert!(!valid_id("a.mp3"));
        assert!(!valid_id(&"9".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn storing_under_an_unusable_id_is_refused() {
        let temp = TempCache::new("badid");
        assert!(temp.cache.insert(&track("../evil"), b"x", true).is_err());
        assert!(temp.cache.insert(&track("42"), b"", true).is_err());
    }

    /// An interrupted download leaves a `.part`; it must never be mistaken for a
    /// cached track, and must not accumulate.
    #[test]
    fn a_half_finished_download_is_swept_up() {
        let temp = TempCache::new("part");
        let part = temp.dir.join("77.part");
        std::fs::write(&part, b"half a track").unwrap();

        let reopened = temp.reopen();
        assert!(!reopened.has("77"));
        assert!(!part.exists(), "the leftover should have been removed");
    }

    /// A crash between writing the audio and writing the sidecar leaves bytes
    /// nothing can identify.
    #[test]
    fn audio_without_metadata_is_not_a_cached_track() {
        let temp = TempCache::new("orphan-audio");
        std::fs::write(temp.dir.join("77.mp3"), b"unidentified").unwrap();

        let reopened = temp.reopen();
        assert!(!reopened.has("77"));
        assert_eq!(reopened.count(), 0);
    }

    #[test]
    fn metadata_without_audio_is_not_a_cached_track() {
        let temp = TempCache::new("orphan-meta");
        temp.cache.insert(&track("77"), b"bytes", true).unwrap();
        std::fs::remove_file(temp.dir.join("77.mp3")).unwrap();

        let reopened = temp.reopen();
        assert!(!reopened.has("77"));
        assert!(
            !temp.dir.join("77.json").exists(),
            "the stranded metadata should have been cleaned up"
        );
    }

    /// Truncation is how a cache entry realistically goes bad, and the recorded
    /// length is what catches it — before a decoder fails halfway through a song.
    #[test]
    fn a_truncated_file_is_rejected_and_dropped() {
        let temp = TempCache::new("truncated");
        temp.cache.insert(&track("77"), b"the whole track", true).unwrap();
        std::fs::write(temp.dir.join("77.mp3"), b"cut").unwrap();

        assert!(temp.cache.read("77").is_err());
        assert!(!temp.cache.has("77"), "a damaged entry is not kept");
    }

    #[test]
    fn a_truncated_file_is_dropped_on_startup_too() {
        let temp = TempCache::new("truncated-scan");
        temp.cache.insert(&track("77"), b"the whole track", true).unwrap();
        std::fs::write(temp.dir.join("77.mp3"), b"cut").unwrap();

        assert!(!temp.reopen().has("77"));
    }

    #[test]
    fn removing_a_track_takes_both_of_its_files() {
        let temp = TempCache::new("remove");
        temp.cache.insert(&track("42"), b"bytes", true).unwrap();
        temp.cache.remove("42").unwrap();

        assert!(!temp.cache.has("42"));
        assert!(!temp.dir.join("42.mp3").exists());
        assert!(!temp.dir.join("42.json").exists());
        assert_eq!(temp.cache.total_bytes(), 0);
    }

    /// Announced to the room on every change, so an unstable order would look
    /// like a change even when nothing moved.
    #[test]
    fn ids_come_out_sorted() {
        let temp = TempCache::new("sorted");
        for id in ["30", "10", "20"] {
            temp.cache.insert(&track(id), b"bytes", true).unwrap();
        }
        assert_eq!(temp.cache.ids(), ["10", "20", "30"]);
    }

    #[test]
    fn the_library_lists_the_most_recently_played_first() {
        let temp = TempCache::new("library");
        for id in ["1", "2", "3"] {
            temp.cache.insert(&track(id), b"bytes", true).unwrap();
            // `unix_ms` has millisecond resolution, so keep the writes apart.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        temp.cache.touch("1");

        let listed: Vec<String> = temp
            .cache
            .tracks()
            .into_iter()
            .map(|t| t.track.track_id)
            .collect();
        assert_eq!(listed.first().map(String::as_str), Some("1"));
    }

    #[test]
    fn nothing_is_evicted_without_a_limit() {
        let temp = TempCache::with_limit("nolimit", 0);
        for id in ["1", "2", "3"] {
            let result = temp.cache.insert(&track(id), &[0u8; 1_000], false).unwrap();
            assert_eq!(result.evicted, 0);
        }
        assert_eq!(temp.cache.count(), 3);
    }

    /// The limit is enforced by dropping what has not been listened to in the
    /// longest time.
    #[test]
    fn the_oldest_unpinned_track_is_evicted_first() {
        let temp = TempCache::with_limit("evict", 2_500);
        for id in ["1", "2"] {
            temp.cache.insert(&track(id), &[0u8; 1_000], false).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Track 2 has played more recently than track 1.
        temp.cache.touch("2");

        let result = temp.cache.insert(&track("3"), &[0u8; 1_000], false).unwrap();
        assert_eq!(result.evicted, 1);
        assert!(!result.over_limit);
        assert!(!temp.cache.has("1"), "the stalest track goes first");
        assert!(temp.cache.has("2"));
        assert!(temp.cache.has("3"));
    }

    /// What the user explicitly downloaded is what they expect to find on the
    /// train, so automatic caching must never displace it.
    #[test]
    fn a_pinned_track_is_never_evicted_to_make_room() {
        let temp = TempCache::with_limit("pinned", 1_500);
        temp.cache.insert(&track("keep"), &[0u8; 1_000], true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));

        let result = temp.cache.insert(&track("new"), &[0u8; 1_000], false).unwrap();
        assert!(temp.cache.has("keep"));
        assert!(
            result.over_limit,
            "with only pinned tracks left the limit cannot be honoured, and that is worth reporting"
        );
    }

    /// Evicting the track just stored would leave the cache empty *and* still
    /// without the track that was asked for.
    #[test]
    fn a_track_larger_than_the_limit_is_still_stored() {
        let temp = TempCache::with_limit("oversize", 100);
        let result = temp.cache.insert(&track("big"), &[0u8; 1_000], true).unwrap();

        assert!(temp.cache.has("big"));
        assert!(result.over_limit);
        assert_eq!(temp.cache.read("big").unwrap().len(), 1_000);
    }

    #[test]
    fn re_downloading_a_pinned_track_keeps_it_pinned() {
        let temp = TempCache::new("repin");
        temp.cache.insert(&track("42"), b"first", true).unwrap();
        temp.cache.insert(&track("42"), b"second", false).unwrap();

        let listed = temp.cache.tracks();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].pinned, "an explicit download stays explicit");
    }

    #[test]
    fn the_digest_identifies_the_contents() {
        let temp = TempCache::new("digest");
        let stored = temp.cache.insert(&track("42"), b"audio bytes", true).unwrap();

        assert_eq!(temp.cache.digest("42").as_deref(), Some(stored.digest.as_str()));
        assert_eq!(stored.digest.len(), 32);
        assert_eq!(stored.digest, digest_of(b"audio bytes"));
        assert_ne!(stored.digest, digest_of(b"other bytes"));
    }

    /// ExoPlayer is handed a URL rather than bytes, so a cached track has to be
    /// expressible as one — and on Windows that means `file:///C:/…`.
    #[test]
    fn a_windows_path_becomes_a_three_slash_file_url() {
        let url = file_url(Path::new(r"C:\Users\x\ymsync\tracks\42.mp3"));
        assert_eq!(url, "file:///C:/Users/x/ymsync/tracks/42.mp3");
    }

    #[test]
    fn a_unix_path_keeps_its_leading_slash() {
        let url = file_url(Path::new("/data/user/0/dev.mshiv.ymsync/files/tracks/42.mp3"));
        assert_eq!(
            url,
            "file:///data/user/0/dev.mshiv.ymsync/files/tracks/42.mp3"
        );
    }

    #[test]
    fn a_cached_track_has_a_file_url() {
        let temp = TempCache::new("fileurl");
        temp.cache.insert(&track("42"), b"bytes", true).unwrap();
        let url = temp.cache.file_url("42").expect("a url for a cached track");
        assert!(url.starts_with("file://"), "{url}");
        assert!(url.ends_with("/42.mp3"), "{url}");
    }
}
