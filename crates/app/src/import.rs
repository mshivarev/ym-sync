//! Local audio files brought into this device's downloads.
//!
//! An imported file becomes an ordinary offline track: it lands in the same cache
//! as a download from Yandex, with the same sidecar, so the room plays it, the
//! other peers can fetch it over the local network, and it survives a restart.
//! Nothing here touches the internet, and no account is needed.
//!
//! What it cannot do is open what the Yandex Music app itself downloaded: that
//! app keeps its offline tracks encrypted with keys tied to the account and the
//! device, so those files are not audio anybody else can read.

use std::path::Path;

use anyhow::{Context, Result, bail};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;
use tracing::debug;
use ymsync_proto::TrackRef;

use crate::cache::{Cache, digest_of, valid_id};

/// Formats worth offering in a file picker. Everything the bundled decoders can
/// read; the cache stores the bytes as they are.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "mp4", "aac", "ogg", "oga", "wav"];

/// The outcome of importing one file.
#[derive(Debug, Clone)]
pub struct Imported {
    pub track: TrackRef,
    pub bytes: u64,
    /// The same file was already in the downloads, so nothing was written.
    pub already_there: bool,
}

/// Imports a file from disk. What the desktop's file picker hands over.
pub fn from_path(cache: &Cache, path: impl AsRef<Path>) -> Result<Imported> {
    let path = path.as_ref();
    let data = std::fs::read(path)
        .with_context(|| format!("не удалось прочитать {}", path.display()))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    from_bytes(cache, &name, data)
}

/// Imports a file a front end has already read.
///
/// Android's picker returns a content URI rather than a path, so the phone reads
/// the bytes itself and this is the way in.
pub fn from_bytes(cache: &Cache, name: &str, data: Vec<u8>) -> Result<Imported> {
    if data.is_empty() {
        bail!("файл «{name}» пуст");
    }

    // The id is the file's own digest, so importing the same file twice — from
    // two folders, or by pressing the button again — is not two copies of it.
    let digest = digest_of(&data);
    let id = format!("local_{}", &digest[..24]);
    debug_assert!(valid_id(&id));

    if let Some(track) = cache.track(&id) {
        return Ok(Imported {
            bytes: data.len() as u64,
            track,
            already_there: true,
        });
    }

    let meta = read_meta(&data, name);
    let track = TrackRef {
        track_id: id,
        album_id: None,
        album: meta.album,
        title: meta.title,
        artist: meta.artist,
        duration_ms: meta.duration_ms,
        // Covers come from Yandex's CDN, and a local file has no place there.
        // Embedded art would have to be served from this device to be of use to
        // the room, which is more than a picture is worth.
        cover_uri: None,
    };

    let bytes = data.len() as u64;
    // Pinned: a file the user went and picked is not something to evict when the
    // cache fills up.
    cache
        .insert(&track, &data, true)
        .with_context(|| format!("не удалось сохранить «{name}»"))?;
    debug!(track = %track, "imported a local file");

    Ok(Imported {
        track,
        bytes,
        already_there: false,
    })
}

struct Meta {
    title: String,
    artist: String,
    album: Option<String>,
    duration_ms: u64,
}

/// Tags and length, read from the file itself.
///
/// A file with no tags still has to say something useful, so the name stands in:
/// `Кино - Группа крови.mp3` is a shape people actually have on disk.
fn read_meta(data: &[u8], name: &str) -> Meta {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem).trim();
    let (name_artist, name_title) = match stem.split_once(" - ") {
        Some((artist, title)) if !artist.trim().is_empty() && !title.trim().is_empty() => {
            (Some(artist.trim().to_string()), title.trim().to_string())
        }
        _ => (None, stem.to_string()),
    };

    let probed = probe(data, name);
    Meta {
        title: probed
            .as_ref()
            .and_then(|found| found.title.clone())
            .unwrap_or(if name_title.is_empty() {
                "без названия".to_string()
            } else {
                name_title
            }),
        artist: probed
            .as_ref()
            .and_then(|found| found.artist.clone())
            .or(name_artist)
            .unwrap_or_else(|| "неизвестный исполнитель".to_string()),
        album: probed.as_ref().and_then(|found| found.album.clone()),
        duration_ms: probed.as_ref().map_or(0, |found| found.duration_ms),
    }
}

#[derive(Default)]
struct Probed {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: u64,
}

/// What the decoders can tell us about the file. `None` when the format is not
/// one of them — the import still goes ahead, named after the file.
fn probe(data: &[u8], name: &str) -> Option<Probed> {
    let mut hint = Hint::new();
    if let Some((_, extension)) = name.rsplit_once('.') {
        hint.with_extension(extension);
    }

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default());
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;

    let mut found = Probed::default();

    // Length comes from the stream itself rather than from a tag: a tag can say
    // anything, and the room keeps time by this number.
    if let Some(track) = probed.format.default_track() {
        let params = &track.codec_params;
        if let (Some(frames), Some(rate)) = (params.n_frames, params.sample_rate) {
            found.duration_ms = frames.saturating_mul(1000) / u64::from(rate.max(1));
        }
    }

    // Tags sit either in the container or ahead of it, depending on the format.
    let mut metadata = probed.metadata.get();
    let revision = metadata
        .as_mut()
        .and_then(|list| list.current().cloned())
        .or_else(|| probed.format.metadata().current().cloned());

    if let Some(revision) = revision {
        for tag in revision.tags() {
            let text = match &tag.value {
                Value::String(text) => text.trim().to_string(),
                other => other.to_string().trim().to_string(),
            };
            if text.is_empty() {
                continue;
            }
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => found.title.get_or_insert(text),
                Some(StandardTagKey::Artist) | Some(StandardTagKey::AlbumArtist) => {
                    found.artist.get_or_insert(text)
                }
                Some(StandardTagKey::Album) => found.album.get_or_insert(text),
                _ => continue,
            };
        }
    }

    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache of its own per test, in the system temporary directory — the same
    /// shape the cache's own tests use.
    fn cache(name: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("ymsync-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::open(&dir, 0).expect("open the cache")
    }

    #[test]
    fn a_file_with_no_tags_is_named_after_itself() {
        let cache = cache("named");

        let imported = from_bytes(&cache, "Кино - Группа крови.mp3", b"not really audio".to_vec())
            .expect("import");

        assert_eq!(imported.track.artist, "Кино");
        assert_eq!(imported.track.title, "Группа крови");
        assert!(!imported.already_there);
        assert!(imported.track.track_id.starts_with("local_"));
        assert!(valid_id(&imported.track.track_id));
    }

    #[test]
    fn a_name_without_a_dash_is_all_title() {
        let cache = cache("plain");

        let imported = from_bytes(&cache, "track01.mp3", b"bytes".to_vec()).expect("import");

        assert_eq!(imported.track.title, "track01");
        assert_eq!(imported.track.artist, "неизвестный исполнитель");
    }

    #[test]
    fn the_same_file_twice_is_one_track() {
        let cache = cache("twice");

        let first = from_bytes(&cache, "a.mp3", b"same bytes".to_vec()).expect("import");
        // A different name, so only the bytes can be what ties the two together.
        let second = from_bytes(&cache, "b.mp3", b"same bytes".to_vec()).expect("import");

        assert_eq!(first.track.track_id, second.track.track_id);
        assert!(second.already_there);
        assert_eq!(cache.tracks().len(), 1);
    }

    #[test]
    fn an_empty_file_is_refused() {
        let cache = cache("empty");

        assert!(from_bytes(&cache, "empty.mp3", Vec::new()).is_err());
    }

    /// A real MP3 frame, so the probe has something it can actually parse: the
    /// length has to come from the audio rather than from a tag.
    #[test]
    fn a_real_mp3_reports_its_length() {
        let cache = cache("mp3");
        // 26 frames of silence, 32 kbps at 44.1 kHz: 104 bytes each, 26 ms each.
        let mut data = Vec::new();
        for _ in 0..26 {
            let mut frame = vec![0u8; 104];
            frame[0] = 0xFF;
            frame[1] = 0xFB;
            frame[2] = 0x10;
            frame[3] = 0xC4;
            data.append(&mut frame);
        }

        let imported = from_bytes(&cache, "silence.mp3", data).expect("import");

        assert!(
            imported.track.duration_ms > 0,
            "expected a length from the stream, got {}",
            imported.track.duration_ms
        );
    }
}
