//! Filling in album names the downloads arrived without.
//!
//! A track cached before album names existed, or one that came from another peer,
//! knows which album it belongs to but not what that album is called. Grouping a
//! library by «Альбом 31888058» would be no better than not grouping it, so the
//! missing names are fetched once — a hundred albums per request — and written
//! into the sidecars beside the audio. After that the grouping works offline,
//! which is the point: this runs when there *is* an internet connection so that
//! the library reads properly when there is not.

use std::collections::BTreeSet;

use anyhow::Result;
use tracing::{debug, info};

use crate::api::YandexMusic;
use crate::cache::Cache;

/// Albums among the downloads whose name is not known yet.
///
/// Sorted and deduplicated: a library is mostly a handful of albums with many
/// tracks each, and asking for the same album once per track would turn a single
/// request into hundreds.
pub fn missing(cache: &Cache) -> Vec<String> {
    cache
        .tracks()
        .into_iter()
        .filter(|entry| entry.track.album.is_none())
        .filter_map(|entry| entry.track.album_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Asks Yandex for the missing names and stores them. Returns how many tracks
/// were updated.
///
/// Needs the network, so it is called where a failure is only a log line: the
/// library still lists everything, it just cannot group by name yet.
pub async fn fill(cache: &Cache, api: &YandexMusic) -> Result<usize> {
    let unknown = missing(cache);
    if unknown.is_empty() {
        return Ok(0);
    }
    debug!(albums = unknown.len(), "запрашиваю названия альбомов");

    let titles = api.album_titles(&unknown).await?;
    if titles.is_empty() {
        return Ok(0);
    }

    let updated = cache.name_albums(&titles)?;
    if updated > 0 {
        info!(
            tracks = updated,
            albums = titles.len(),
            "названия альбомов дописаны в метаданные скачанного"
        );
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use ymsync_proto::TrackRef;

    struct TempCache {
        cache: Cache,
        dir: std::path::PathBuf,
    }

    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl TempCache {
        fn new(name: &str) -> TempCache {
            let dir = std::env::temp_dir()
                .join(format!("ymsync-albums-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let cache = Cache::open(&dir, 0).expect("open the cache");
            TempCache { cache, dir }
        }
    }

    fn track(id: &str, album_id: Option<&str>, album: Option<&str>) -> TrackRef {
        TrackRef {
            track_id: id.to_string(),
            album_id: album_id.map(str::to_string),
            album: album.map(str::to_string),
            title: format!("Title {id}"),
            artist: "Artist".to_string(),
            duration_ms: 1000,
            cover_uri: None,
        }
    }

    /// One request per album, not per track: a library is a few albums with many
    /// tracks each.
    #[test]
    fn each_unnamed_album_is_asked_about_once() {
        let temp = TempCache::new("dedup");
        for id in ["1", "2", "3"] {
            temp.cache
                .insert(&track(id, Some("100"), None), b"bytes", true)
                .unwrap();
        }
        temp.cache
            .insert(&track("4", Some("200"), None), b"bytes", true)
            .unwrap();
        // Already named, and one with no album at all: neither is worth asking.
        temp.cache
            .insert(&track("5", Some("300"), Some("Легенда")), b"bytes", true)
            .unwrap();
        temp.cache
            .insert(&track("6", None, None), b"bytes", true)
            .unwrap();

        assert_eq!(missing(&temp.cache), vec!["100", "200"]);
    }

    #[test]
    fn a_fully_named_library_asks_for_nothing() {
        let temp = TempCache::new("nothing");
        temp.cache
            .insert(&track("1", Some("100"), Some("Легенда")), b"bytes", true)
            .unwrap();
        assert!(missing(&temp.cache).is_empty());
    }

    /// The names have to land on disk, or every launch would ask again and the
    /// grouping would be empty offline.
    #[test]
    fn stored_names_survive_a_restart() {
        let temp = TempCache::new("store");
        temp.cache
            .insert(&track("1", Some("100"), None), b"bytes", true)
            .unwrap();
        temp.cache
            .insert(&track("2", Some("100"), None), b"bytes", true)
            .unwrap();

        let titles = HashMap::from([("100".to_string(), "Группа крови".to_string())]);
        assert_eq!(temp.cache.name_albums(&titles).unwrap(), 2);

        let reopened = Cache::open(&temp.dir, 0).unwrap();
        assert_eq!(
            reopened.track("1").unwrap().album.as_deref(),
            Some("Группа крови")
        );
        assert!(missing(&reopened).is_empty());
    }
}
