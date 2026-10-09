//! Client for Yandex Music's unofficial mobile API.
//!
//! None of this is published or supported by Yandex: the endpoints, the JSON
//! shapes and the stream-URL signature are all community reverse-engineering and
//! can change without notice. `ymsync probe` exercises every call this module
//! makes so a breakage shows up as one clear diagnostic instead of a puzzling
//! failure mid-playback.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use ymsync_proto::TrackRef;

const API_BASE: &str = "https://api.music.yandex.net";

/// Salt the mobile clients prepend when signing a stream path.
const SIGN_SALT: &str = "XGRlBW9FXlekgbPrRHuSiA";

const CLIENT_HEADER: &str = "YandexMusicAndroid/24023621";
const USER_AGENT: &str = "Yandex-Music-API";

/// The station behind «Моя волна».
pub const WAVE_STATION: &str = "user:onyourwave";

/// Every account's «Мне нравится» is playlist 3 of that account.
const LIKES_KIND: &str = "3";

/// How many album ids go into one `/albums` request.
const ALBUM_BATCH: usize = 100;

/// How much of an unexpected response body to quote in an error.
const ERROR_EXCERPT: usize = 400;

pub struct YandexMusic {
    http: reqwest::Client,
    token: String,
    /// This account's numeric id, asked for once. Every «Мне нравится» call needs
    /// it, and it cannot change under a fixed token.
    uid: tokio::sync::OnceCell<i64>,
}

/// The platform TLS stack (schannel on Windows) needs no help.
#[cfg(not(feature = "tls-rustls"))]
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

/// Trusts the bundled webpki roots rather than the platform verifier.
///
/// `reqwest`'s rustls support would otherwise reach for
/// `rustls-platform-verifier`, which on Android has to be handed a JVM before its
/// first use. A fixed root set is one less moving part across the JNI boundary.
/// The provider is named explicitly so this does not depend on whether
/// [`crate::install_tls_provider`] ran first.
#[cfg(feature = "tls-rustls")]
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config =
        rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();

    reqwest::Client::builder().use_preconfigured_tls(config)
}

impl YandexMusic {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = client_builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            token: token.into(),
            uid: tokio::sync::OnceCell::new(),
        })
    }

    pub async fn account_status(&self) -> Result<AccountStatus> {
        self.get_result("/account/status", &[]).await
    }

    pub async fn search_tracks(&self, text: &str, limit: usize) -> Result<Vec<Track>> {
        let found: SearchResponse = self
            .get_result(
                "/search",
                &[
                    ("text", text),
                    ("type", "track"),
                    ("page", "0"),
                    ("nocorrect", "false"),
                ],
            )
            .await?;
        let mut tracks = found.tracks.map(|t| t.results).unwrap_or_default();
        tracks.truncate(limit);
        Ok(tracks)
    }

    /// What to offer while somebody is still typing.
    ///
    /// This is Yandex's own suggest endpoint, the one its apps draw the dropdown
    /// from: a best guess at what is meant, plus the queries other people went on
    /// to search for. It is a different thing from [`Self::search_tracks`], which
    /// costs a full search — hence a separate call made on every keystroke.
    pub async fn suggest(&self, part: &str) -> Result<Suggest> {
        let raw: SuggestResponse = self.get_result("/search/suggest", &[("part", part)]).await?;
        Ok(Suggest {
            best: raw.best.and_then(BestMatch::from_raw),
            suggestions: raw
                .suggestions
                .into_iter()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect(),
        })
    }

    pub async fn track(&self, track_id: &str) -> Result<Track> {
        let tracks: Vec<Track> = self
            .get_result(&format!("/tracks/{track_id}"), &[])
            .await
            .with_context(|| format!("looking up track {track_id}"))?;
        tracks
            .into_iter()
            .next()
            .with_context(|| format!("трека {track_id} нет на этом аккаунте"))
    }

    /// Albums whose name or artist matches, for the row of cards above the
    /// tracks in search results.
    pub async fn search_albums(&self, text: &str, limit: usize) -> Result<Vec<AlbumInfo>> {
        let found: AlbumSearchResponse = self
            .get_result(
                "/search",
                &[
                    ("text", text),
                    ("type", "album"),
                    ("page", "0"),
                    ("nocorrect", "false"),
                ],
            )
            .await?;
        let mut albums: Vec<AlbumInfo> = found
            .albums
            .map(|bucket| bucket.results)
            .unwrap_or_default()
            .into_iter()
            .map(RawAlbum::into_info)
            .collect();
        albums.truncate(limit);
        Ok(albums)
    }

    /// Every track of an album, in disc order.
    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let album: AlbumWithTracks = self
            .get_result(&format!("/albums/{album_id}/with-tracks"), &[])
            .await
            .with_context(|| format!("запрос альбома {album_id}"))?;
        let tracks: Vec<Track> = album.volumes.into_iter().flatten().collect();
        if tracks.is_empty() {
            bail!("в альбоме {album_id} нет треков");
        }
        Ok(tracks)
    }

    /// Every track of a playlist. `kind` is the per-user playlist number, and
    /// `3` is the built-in "Мне нравится".
    pub async fn playlist_tracks(&self, owner: &str, kind: &str) -> Result<Vec<Track>> {
        let playlist: PlaylistResponse = self
            .get_result(&format!("/users/{owner}/playlists/{kind}"), &[])
            .await
            .with_context(|| format!("запрос плейлиста {owner}/{kind}"))?;
        // Entries whose `track` is missing are unavailable in this catalogue.
        let tracks: Vec<Track> = playlist
            .tracks
            .into_iter()
            .filter_map(|entry| entry.track)
            .collect();
        if tracks.is_empty() {
            bail!("в плейлисте {owner}/{kind} нет доступных треков");
        }
        Ok(tracks)
    }

    /// The account's own playlists, as Yandex lists them — «Мне нравится» is not
    /// among them, it has its own place.
    pub async fn my_playlists(&self) -> Result<Vec<PlaylistInfo>> {
        let uid = self.uid().await?;
        let raw: Vec<RawPlaylist> = self
            .get_result(&format!("/users/{uid}/playlists/list"), &[])
            .await
            .context("запрос списка плейлистов")?;
        Ok(raw
            .into_iter()
            .filter(|p| p.kind.0 != LIKES_KIND)
            .map(RawPlaylist::into_info)
            .collect())
    }

    /// A track's words, synced to the music where Yandex has timings.
    ///
    /// The endpoint wants a signature of the track id and the time, made with a
    /// key the official apps carry. Timed (LRC) words are asked for first, plain
    /// text second; `None` means the track has none at all.
    pub async fn lyrics(&self, track_id: &str) -> Result<Option<Lyrics>> {
        // A track id may arrive as `id:album`; the endpoint wants the bare id.
        let id = track_id.split(':').next().unwrap_or(track_id);
        for (format, synced) in [("LRC", true), ("TEXT", false)] {
            let stamp = (ymsync_proto::unix_ms() / 1000).to_string();
            let sign = lyrics_sign(id, &stamp);
            let path = format!("/tracks/{id}/lyrics");
            let found: Result<RawLyrics> = self
                .get_result(
                    &path,
                    &[("format", format), ("timeStamp", &stamp), ("sign", &sign)],
                )
                .await;
            let Ok(found) = found else {
                // 404 for a format the track has not got; try the next one.
                continue;
            };
            if found.download_url.is_empty() {
                continue;
            }
            let text = self.get_text(&found.download_url).await?;
            if text.trim().is_empty() {
                continue;
            }
            let lines = if synced { parse_lrc(&text) } else { Vec::new() };
            return Ok(Some(Lyrics {
                // A file that claimed to be LRC but had no timings reads as text.
                synced: !lines.is_empty(),
                lines,
                text: if synced { strip_lrc(&text) } else { text },
                writers: found.writers,
            }));
        }
        Ok(None)
    }

    /// The account's own «Мне нравится», newest first.
    ///
    /// This is playlist 3 of the account, which answers with whole tracks —
    /// `/likes/tracks` returns bare ids and would need a second round of
    /// requests to say what they are.
    pub async fn liked_tracks(&self) -> Result<Vec<Track>> {
        let uid = self.uid().await?;
        self.playlist_tracks(&uid.to_string(), LIKES_KIND).await
    }

    /// This account's numeric id.
    ///
    /// Asked for once and kept: every «Мне нравится» call needs it, and a like is
    /// a button press — paying for an extra round trip on each one would show.
    async fn uid(&self) -> Result<i64> {
        if let Some(uid) = self.uid.get() {
            return Ok(*uid);
        }
        let uid = self
            .account_status()
            .await?
            .account
            .uid
            .context("Яндекс не сообщил uid аккаунта — «Мне нравится» не найти")?;
        let _ = self.uid.set(uid);
        Ok(uid)
    }

    /// Puts the track into this account's «Мне нравится», or takes it out.
    ///
    /// Two endpoints rather than one toggle, because the API has no toggle: the
    /// caller says which state it wants. Deciding here from a cached "is it
    /// liked?" would mean a stale answer silently unliking what the listener
    /// meant to like.
    ///
    /// Likes belong to an account, not to the room: each participant listens on
    /// their own, so this changes nothing for anybody else.
    pub async fn set_liked(&self, track_id: &str, liked: bool) -> Result<()> {
        let uid = self.uid().await?;
        // `add` takes one id, `remove` takes a list — that asymmetry is the API's.
        let (path, field) = if liked {
            (format!("/users/{uid}/likes/tracks/add"), "track-id")
        } else {
            (format!("/users/{uid}/likes/tracks/remove"), "track-ids")
        };
        self.post_form(&path, &[(field, track_id)])
            .await
            .with_context(|| {
                if liked {
                    format!("не удалось добавить трек {track_id} в «Мне нравится»")
                } else {
                    format!("не удалось убрать трек {track_id} из «Мне нравится»")
                }
            })?;
        Ok(())
    }

    /// Names for a batch of album ids, as `id -> title`.
    ///
    /// A hundred ids per request: this is here to fill in names for tracks that
    /// never carried one, and a library of a thousand downloads would otherwise
    /// mean a thousand round trips. Albums the API says nothing about are simply
    /// missing from the answer.
    pub async fn album_titles(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut titles = std::collections::HashMap::new();
        for chunk in ids.chunks(ALBUM_BATCH) {
            let joined = chunk.join(",");
            let albums: Vec<Album> = self
                .get_result("/albums", &[("album-ids", joined.as_str())])
                .await
                .context("запрос названий альбомов")?;
            for album in albums {
                if let Some(title) = album.title.filter(|title| !title.trim().is_empty()) {
                    titles.insert(album.id.0, title);
                }
            }
        }
        Ok(titles)
    }

    /// One batch from an endless station; [`WAVE_STATION`] is «Моя волна».
    ///
    /// A station hands out a handful of tracks at a time and continues from
    /// whatever `after` names, so the caller passes the last track it received.
    pub async fn station_tracks(&self, station: &str, after: Option<&str>) -> Result<Vec<Track>> {
        let mut query = vec![("settings2", "true")];
        if let Some(after) = after {
            query.push(("queue", after));
        }
        let batch: StationBatch = self
            .get_result(&format!("/rotor/station/{station}/tracks"), &query)
            .await
            .with_context(|| format!("запрос станции {station}"))?;

        let tracks: Vec<Track> = batch
            .sequence
            .into_iter()
            .filter_map(|entry| entry.track)
            .collect();
        if tracks.is_empty() {
            bail!("станция {station} не прислала ни одного трека");
        }
        Ok(tracks)
    }

    /// Resolves a playable, signed URL for the track's audio.
    pub async fn stream_url(&self, track_id: &str) -> Result<String> {
        let variants: Vec<DownloadInfo> = self
            .get_result(&format!("/tracks/{track_id}/download-info"), &[])
            .await
            .with_context(|| format!("requesting download info for track {track_id}"))?;

        let playable: Vec<&DownloadInfo> = variants.iter().filter(|v| !v.preview).collect();
        if playable.is_empty() {
            bail!(
                "для трека {track_id} нет доступных вариантов загрузки — обычно это \
                 региональное или лицензионное ограничение на этом аккаунте"
            );
        }
        // Prefer MP3: it is what the signed `get-mp3` path serves, and rodio
        // decodes it without extra containers.
        let best = playable
            .iter()
            .filter(|v| v.codec.eq_ignore_ascii_case("mp3"))
            .max_by_key(|v| v.bitrate_in_kbps)
            .or_else(|| playable.iter().max_by_key(|v| v.bitrate_in_kbps))
            .expect("playable is not empty");

        let spec = self.stream_spec(&best.download_info_url).await?;
        Ok(spec.to_signed_url())
    }

    /// Downloads the whole track.
    ///
    /// Buffering it entirely is what makes drift correction cheap: a seek then
    /// costs a decoder reset instead of a network round trip, and playback never
    /// stalls mid-track. A few megabytes per track is a fair trade.
    /// Starts downloading a track and answers as soon as the server does, before
    /// the body has arrived — so playback can begin on the first part.
    ///
    /// The timeout is the download's own rather than the client's 30 seconds:
    /// that is fine for an API call, but a ten-megabyte track on a slow phone
    /// connection takes longer, and the client's limit would cut it off mid-song.
    pub async fn open_track(&self, url: &str) -> Result<reqwest::Response> {
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .context("downloading the track")?;
        let status = response.status();
        if !status.is_success() {
            bail!("не удалось скачать трек: HTTP {status}");
        }
        Ok(response)
    }

    pub async fn fetch_track(&self, url: &str) -> Result<Bytes> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .context("downloading the track")?;
        let status = response.status();
        let body = response.bytes().await.context("reading the track body")?;
        if !status.is_success() {
            bail!("не удалось скачать трек: HTTP {status}");
        }
        if body.is_empty() {
            bail!("скачанный трек оказался пустым");
        }
        Ok(body)
    }

    /// The download-info document, which carries the parts of the signature.
    async fn stream_spec(&self, download_info_url: &str) -> Result<StreamSpec> {
        // The endpoint serves XML by default and JSON on request. Try JSON, fall
        // back to XML, so a change in either direction keeps working.
        let separator = if download_info_url.contains('?') { '&' } else { '?' };
        let json_url = format!("{download_info_url}{separator}format=json");

        let json_body = self.get_text(&json_url).await?;
        if let Ok(spec) = serde_json::from_str::<StreamSpec>(&json_body) {
            return Ok(spec);
        }

        let xml_body = self.get_text(download_info_url).await?;
        quick_xml::de::from_str::<StreamSpec>(&xml_body).with_context(|| {
            format!(
                "download info was neither JSON nor the expected XML: {}",
                excerpt(&xml_body)
            )
        })
    }

    async fn get_result<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = format!("{API_BASE}{path}");
        let body = self.get_body(&url, query).await?;

        // Search responses repeat keys — every track object carries `albums`
        // twice, with identical contents. A derived deserialiser rejects that
        // outright, so parse to `Value` first: it keeps the last copy of a
        // duplicated key and lets the typed structs stay simple.
        let document: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("ответ {path} не является JSON: {}", excerpt(&body)))?;

        let envelope: Envelope<T> = serde_json::from_value(document).with_context(|| {
            format!(
                "неожиданная структура ответа {path} — возможно, неофициальный API \
                 изменился: {}",
                excerpt(&body)
            )
        })?;
        Ok(envelope.result)
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        self.get_body(url, &[]).await
    }

    async fn get_body(&self, url: &str, query: &[(&str, &str)]) -> Result<String> {
        self.send(self.authorized(self.http.get(url)).query(query), url)
            .await
    }

    /// A form POST. The two «Мне нравится» endpoints are the only writes this
    /// client makes, and they answer with a revision number nothing here needs.
    async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<String> {
        let url = format!("{API_BASE}{path}");
        self.send(self.authorized(self.http.post(&url)).form(form), &url)
            .await
    }

    /// The three headers every call needs. The client header matters: without it
    /// some endpoints answer as if the request came from a browser.
    fn authorized(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("OAuth {}", self.token))
            .header("X-Yandex-Music-Client", CLIENT_HEADER)
            .header("Accept-Language", "ru")
    }

    /// Sends a prepared request and turns anything but success into a diagnostic.
    ///
    /// One place for that on purpose: an expired token and a missing Плюс both
    /// come back as 401/403, and that is the one failure worth explaining rather
    /// than reporting as a status code.
    async fn send(&self, request: reqwest::RequestBuilder, url: &str) -> Result<String> {
        let response = request
            .send()
            .await
            .with_context(|| format!("запрос {}", redact_url(url)))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("чтение ответа {}", redact_url(url)))?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(TokenRejected {
                message: format!(
                    "Яндекс отклонил токен (HTTP {status}) — он отсутствует, истёк или \
                     у аккаунта нет Плюса: {}",
                    excerpt(&body)
                ),
            }));
        }
        if !status.is_success() {
            bail!(
                "{} вернул HTTP {status}: {}",
                redact_url(url),
                excerpt(&body)
            );
        }
        Ok(body)
    }
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    result: T,
}

/// Yandex's answer that this token will not do: HTTP 401 or 403 from any call.
///
/// A marker type rather than a message to match on, for the same reason as the
/// engine's `NotLicensed`. It matters where a token is being *saved*: a refusal
/// says the token is wrong, while an unreachable Yandex says nothing about it at
/// all, and the two cannot be told apart from the text alone.
#[derive(Debug)]
pub struct TokenRejected {
    /// The whole diagnostic, including the status and what Yandex said.
    pub message: String,
}

impl std::fmt::Display for TokenRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TokenRejected {}

/// What the suggest dropdown draws.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Suggest {
    /// Yandex's best guess: an artist, an album or a track.
    pub best: Option<BestMatch>,
    /// Queries to offer as plain lines, in Yandex's own order.
    pub suggestions: Vec<String>,
}

/// The row at the top of the dropdown, with a picture and what it is.
#[derive(Debug, Clone, Serialize)]
pub struct BestMatch {
    /// `artist`, `album` or `track`. What the row says under the name.
    pub kind: String,
    pub name: String,
    /// The artist under an album or a track; empty for an artist.
    pub subtitle: String,
    /// The same template as a track's, so every screen asks for its own size.
    pub cover_uri: Option<String>,
    /// What to search for when the row is pressed.
    pub query: String,
    /// Set when the guess is an album: pressing the row opens that album rather
    /// than searching for its tracks.
    pub album: Option<AlbumInfo>,
}

impl BestMatch {
    /// Yandex packs a different object per `type`; only the three that can be
    /// turned into something to look at are taken.
    fn from_raw(raw: RawBest) -> Option<Self> {
        let result = raw.result?;
        let artists = || {
            result
                .artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (name, subtitle) = match raw.kind.as_str() {
            "artist" => (result.name.clone()?, String::new()),
            "album" => (result.title.clone()?, artists()),
            "track" => (result.title.clone()?, artists()),
            // Playlists, podcasts and whatever else Yandex adds later: the plain
            // suggestion lines below still cover them.
            _ => return None,
        };
        let cover_uri = result
            .cover
            .and_then(|cover| cover.uri)
            .or(result.og_image)
            .or_else(|| result.albums.first().and_then(|album| album.cover_uri.clone()))
            .filter(|uri| !uri.trim().is_empty());
        let album = match (raw.kind.as_str(), &result.id) {
            ("album", Some(id)) => Some(AlbumInfo {
                id: id.0.clone(),
                title: name.clone(),
                artist: subtitle.clone(),
                cover_uri: cover_uri.clone(),
                year: result.year,
                track_count: result.track_count,
            }),
            _ => None,
        };
        Some(Self {
            query: if subtitle.is_empty() {
                name.clone()
            } else {
                format!("{subtitle} {name}")
            },
            kind: raw.kind,
            name,
            subtitle,
            cover_uri,
            album,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SuggestResponse {
    #[serde(default)]
    best: Option<RawBest>,
    #[serde(default)]
    suggestions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawBest {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    result: Option<RawBestResult>,
}

/// One shape for all three kinds: an artist has a name, an album and a track a
/// title, and each carries its picture somewhere else again.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBestResult {
    #[serde(default)]
    id: Option<Id>,
    #[serde(default)]
    year: Option<u32>,
    #[serde(default)]
    track_count: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    albums: Vec<Album>,
    #[serde(default)]
    cover: Option<RawCover>,
    #[serde(default)]
    og_image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCover {
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    tracks: Option<SearchBucket>,
}

#[derive(Debug, Deserialize)]
struct SearchBucket {
    #[serde(default)]
    results: Vec<Track>,
}

/// `/search?type=album`: the same envelope, with albums in it.
#[derive(Debug, Deserialize)]
struct AlbumSearchResponse {
    #[serde(default)]
    albums: Option<AlbumBucket>,
}

#[derive(Debug, Deserialize)]
struct AlbumBucket {
    #[serde(default)]
    results: Vec<RawAlbum>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAlbum {
    id: Id,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    cover_uri: Option<String>,
    #[serde(default)]
    year: Option<u32>,
    #[serde(default)]
    track_count: Option<u32>,
}

/// An album as a search result or the head of an album page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct AlbumInfo {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// The same template as a track's; each screen asks for its own size.
    pub cover_uri: Option<String>,
    pub year: Option<u32>,
    pub track_count: Option<u32>,
}

impl RawAlbum {
    fn into_info(self) -> AlbumInfo {
        AlbumInfo {
            id: self.id.0,
            title: self.title,
            artist: self
                .artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            cover_uri: self.cover_uri.filter(|uri| !uri.trim().is_empty()),
            year: self.year,
            track_count: self.track_count,
        }
    }
}

/// `/albums/{id}/with-tracks` groups tracks by disc.
#[derive(Debug, Deserialize)]
struct AlbumWithTracks {
    #[serde(default)]
    volumes: Vec<Vec<Track>>,
}

#[derive(Debug, Deserialize)]
struct PlaylistResponse {
    #[serde(default)]
    tracks: Vec<PlaylistEntry>,
}

/// Playlist entries wrap the track, and omit it when it is unavailable.
#[derive(Debug, Deserialize)]
struct PlaylistEntry {
    #[serde(default)]
    track: Option<Track>,
}

/// What a station hands out. `batchId` identifies the batch for feedback
/// reporting, which this client does not send: the sequence advances on its own.
#[derive(Debug, Deserialize)]
struct StationBatch {
    #[serde(default)]
    sequence: Vec<StationEntry>,
}

/// Station entries wrap the track the same way playlist entries do.
#[derive(Debug, Deserialize)]
struct StationEntry {
    #[serde(default)]
    track: Option<Track>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadInfo {
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate_in_kbps: u32,
    /// A 30-second sample rather than the track.
    #[serde(default)]
    preview: bool,
    download_info_url: String,
}

/// Both the JSON and the XML form of the download-info document use these names.
#[derive(Debug, Clone, Deserialize)]
struct StreamSpec {
    host: String,
    path: String,
    ts: String,
    s: String,
}

impl StreamSpec {
    /// `md5(salt + path-without-leading-slash + s)` keys the CDN URL.
    fn to_signed_url(&self) -> String {
        let path = self.path.strip_prefix('/').unwrap_or(&self.path);
        let mut hasher = Md5::new();
        hasher.update(SIGN_SALT.as_bytes());
        hasher.update(path.as_bytes());
        hasher.update(self.s.as_bytes());
        let sign = hex::encode(hasher.finalize());
        format!(
            "https://{}/get-mp3/{}/{}{}",
            self.host,
            sign,
            self.ts,
            if self.path.starts_with('/') {
                self.path.clone()
            } else {
                format!("/{}", self.path)
            }
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    pub account: Account,
    #[serde(default)]
    pub plus: Option<Plus>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub uid: Option<i64>,
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub region: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plus {
    #[serde(default)]
    pub has_plus: bool,
}

/// One of the account's playlists, as a card shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PlaylistInfo {
    /// Whose playlist it is; with `kind`, what [`YandexMusic::playlist_tracks`] takes.
    pub owner: String,
    pub kind: String,
    pub title: String,
    pub track_count: u32,
    /// A cover template, like a track's — the playlist's own picture, or the
    /// first of the album covers Yandex tiles it from.
    pub cover_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlaylist {
    uid: Id,
    kind: Id,
    #[serde(default)]
    title: String,
    #[serde(default)]
    track_count: u32,
    #[serde(default)]
    cover: Option<PlaylistCover>,
    #[serde(default)]
    og_image: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistCover {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    items_uri: Vec<String>,
}

impl RawPlaylist {
    fn into_info(self) -> PlaylistInfo {
        let cover_uri = self
            .cover
            .and_then(|cover| cover.uri.or_else(|| cover.items_uri.into_iter().next()))
            .or(self.og_image)
            .filter(|uri| !uri.trim().is_empty());
        PlaylistInfo {
            owner: self.uid.0,
            kind: self.kind.0,
            title: self.title,
            track_count: self.track_count,
            cover_uri,
        }
    }
}

/// A track's words.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Lyrics {
    /// `lines` carries timings and a player can follow along.
    pub synced: bool,
    /// Timed lines, in order; empty when the words are plain text.
    pub lines: Vec<LyricLine>,
    /// The whole text without timings, for when following along is off.
    pub text: String,
    pub writers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LyricLine {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLyrics {
    #[serde(default)]
    download_url: String,
    #[serde(default)]
    writers: Vec<String>,
}

/// The key the official apps sign lyrics requests with.
const LYRICS_KEY: &[u8] = b"p93jhgh689SBReK6ghtw62";

/// `base64(HMAC-SHA256(key, track_id + timestamp))`, as the lyrics endpoint wants.
fn lyrics_sign(track_id: &str, stamp: &str) -> String {
    use base64::Engine as _;
    let message = format!("{track_id}{stamp}");
    base64::engine::general_purpose::STANDARD.encode(hmac_sha256(LYRICS_KEY, message.as_bytes()))
}

/// HMAC per RFC 2104. Short enough to write out rather than pull in a crate for
/// the one signature this client makes.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner_pad: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let outer_pad: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new().chain_update(&inner_pad).chain_update(message).finalize();
    Sha256::new()
        .chain_update(&outer_pad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// Splits `[mm:ss.xx] words` lines into timings. Lines without a timing — the
/// `[ar:…]` style headers among them — are dropped; a line with several stamps
/// is repeated at each.
fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            match lrc_stamp(&after[..end]) {
                Some(at_ms) => stamps.push(at_ms),
                None => break,
            }
            rest = &after[end + 1..];
        }
        let words = rest.trim().to_string();
        for at_ms in stamps {
            lines.push(LyricLine {
                at_ms,
                text: words.clone(),
            });
        }
    }
    lines.sort_by_key(|line| line.at_ms);
    lines
}

/// `mm:ss`, `mm:ss.xx` or `mm:ss.xxx`, in milliseconds.
fn lrc_stamp(stamp: &str) -> Option<u64> {
    let (minutes, seconds) = stamp.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole: u64 = whole.trim().parse().ok()?;
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction[..3].parse::<u64>().ok()?,
    };
    Some(minutes * 60_000 + whole * 1000 + fraction_ms)
}

/// The words of an LRC file without its timings.
fn strip_lrc(text: &str) -> String {
    let lines = parse_lrc(text);
    if lines.is_empty() {
        return text.to_string();
    }
    lines
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: Id,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// Absent on some payloads; assume playable and let the download fail loudly.
    #[serde(default = "yes")]
    pub available: bool,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub albums: Vec<Album>,
    #[serde(default)]
    pub cover_uri: Option<String>,
}

fn yes() -> bool {
    true
}

impl Track {
    pub fn artist_names(&self) -> String {
        if self.artists.is_empty() {
            return "Unknown artist".to_string();
        }
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn album_id(&self) -> Option<String> {
        self.albums.first().map(|a| a.id.0.clone())
    }

    /// The album's name, when the payload carried one.
    ///
    /// Search results and playlists include it; a track resolved by id sometimes
    /// does not, and a track that arrived from another peer never does. Whoever
    /// needs the name for those falls back to [`YandexMusic::album_titles`].
    pub fn album_title(&self) -> Option<String> {
        self.albums
            .first()
            .and_then(|a| a.title.clone())
            .filter(|title| !title.trim().is_empty())
    }

    /// The cover template: the track's own, else its album's.
    pub fn cover_uri(&self) -> Option<String> {
        self.cover_uri
            .clone()
            .or_else(|| self.albums.first().and_then(|a| a.cover_uri.clone()))
            .filter(|uri| !uri.trim().is_empty())
    }

    pub fn to_track_ref(&self) -> TrackRef {
        TrackRef {
            track_id: self.id.0.clone(),
            album_id: self.album_id(),
            album: self.album_title(),
            title: self.title.clone(),
            artist: self.artist_names(),
            duration_ms: self.duration_ms,
            cover_uri: self.cover_uri(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: Id,
    /// Present in search results and playlist payloads; absent often enough that
    /// nothing may depend on it.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cover_uri: Option<String>,
}

/// An id that the API returns sometimes as a string and sometimes as a number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id(pub String);

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl<'de> serde::de::Visitor<'de> for IdVisitor {
            type Value = Id;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an id as a string or an integer")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Id, E> {
                Ok(Id(v.to_string()))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Id, E> {
                Ok(Id(v.to_string()))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Id, E> {
                Ok(Id(v.to_string()))
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

fn excerpt(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= ERROR_EXCERPT {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(ERROR_EXCERPT).collect();
    format!("{head}…")
}

/// Signed URLs carry their credential in the path as well as the query, so keep
/// only enough of them to identify the endpoint. Ordinary API paths are short
/// and survive intact; the signed blob is cut down.
pub fn redact_url(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    base.split('/')
        .map(|segment| {
            if segment.chars().count() > 40 {
                let head: String = segment.chars().take(8).collect();
                format!("{head}…")
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Convenience for callers that only have an id string.
pub fn parse_track_id(raw: &str) -> Result<String> {
    let candidate = raw.trim();
    // Accept a full web URL such as .../album/123/track/456.
    let id = candidate
        .rsplit_once("/track/")
        .map(|(_, tail)| tail)
        .unwrap_or(candidate);
    let id = id.split(['?', '#', '/']).next().unwrap_or(id);
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "«{raw}» не похоже на id трека: нужны цифры или ссылка music.yandex.ru/.../track/<id>"
        ));
    }
    Ok(id.to_string())
}

/// Accepts a bare album id or any music.yandex.ru URL containing `/album/<id>`
/// (including a track URL, whose album is then used).
pub fn parse_album_id(raw: &str) -> Result<String> {
    let candidate = raw.trim();
    let id = candidate
        .split_once("/album/")
        .map(|(_, tail)| tail)
        .unwrap_or(candidate);
    let id = id.split(['?', '#', '/']).next().unwrap_or(id);
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "«{raw}» не похоже на id альбома: нужны цифры или ссылка music.yandex.ru/album/<id>"
        ));
    }
    Ok(id.to_string())
}

/// Accepts `owner/kind` or a music.yandex.ru playlist URL.
pub fn parse_playlist_ref(raw: &str) -> Result<(String, String)> {
    let candidate = raw.trim().trim_end_matches('/');

    if let Some((_, tail)) = candidate.split_once("/users/")
        && let Some((owner, rest)) = tail.split_once("/playlists/")
    {
        let kind = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if !owner.is_empty() && !kind.is_empty() {
            return Ok((owner.to_string(), kind.to_string()));
        }
    }

    if let Some((owner, kind)) = candidate.split_once('/')
        && !owner.is_empty()
        && !kind.is_empty()
        && !kind.contains('/')
    {
        return Ok((owner.to_string(), kind.to_string()));
    }

    Err(anyhow!(
        "«{raw}» не похоже на плейлист: нужно логин/номер или ссылка \
         music.yandex.ru/users/<логин>/playlists/<номер>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_url_has_the_expected_shape() {
        let spec = StreamSpec {
            host: "cdn.example.net".into(),
            path: "/get-mp3/abc/1.mp3".into(),
            ts: "6512ab00".into(),
            s: "deadbeef".into(),
        };
        let url = spec.to_signed_url();
        let rest = url
            .strip_prefix("https://cdn.example.net/get-mp3/")
            .expect("host and prefix: {url}");
        let (sign, tail) = rest.split_once('/').expect("signature segment");
        assert_eq!(sign.len(), 32, "md5 hex digest");
        assert!(sign.chars().all(|c| c.is_ascii_hexdigit()), "{sign}");
        assert_eq!(tail, "6512ab00/get-mp3/abc/1.mp3");
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2: key "Jefe".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        let mac = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(
            hex::encode(mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn lrc_lines_become_timings() {
        let lrc = "[ar:Кино]\n[00:01.50]Белый снег\n[00:12.345][01:00]Припев\n\n[00:05]Серый лёд";
        let lines = parse_lrc(lrc);
        let got: Vec<(u64, &str)> = lines.iter().map(|l| (l.at_ms, l.text.as_str())).collect();
        assert_eq!(
            got,
            [(1_500, "Белый снег"), (5_000, "Серый лёд"), (12_345, "Припев"), (60_000, "Припев")]
        );
        assert_eq!(strip_lrc(lrc), "Белый снег\nСерый лёд\nПрипев\nПрипев");
        assert_eq!(strip_lrc("просто текст"), "просто текст");
    }

    #[test]
    fn a_playlist_list_reads_what_a_card_shows() {
        let raw: Vec<RawPlaylist> = serde_json::from_str(
            r#"[
                {"uid": 42, "kind": 1003, "title": "В дорогу", "trackCount": 17,
                 "cover": {"type": "mosaic", "itemsUri": ["avatars.yandex.net/get-music-content/1/a/%%"]}},
                {"uid": 42, "kind": 1004, "title": "Пустой", "trackCount": 0,
                 "ogImage": "avatars.yandex.net/get-music-content/2/b/%%"}
            ]"#,
        )
        .unwrap();
        let cards: Vec<PlaylistInfo> = raw.into_iter().map(RawPlaylist::into_info).collect();
        assert_eq!(cards[0].owner, "42");
        assert_eq!(cards[0].kind, "1003");
        assert_eq!(cards[0].track_count, 17);
        assert_eq!(cards[0].cover_uri.as_deref(), Some("avatars.yandex.net/get-music-content/1/a/%%"));
        assert_eq!(cards[1].cover_uri.as_deref(), Some("avatars.yandex.net/get-music-content/2/b/%%"));
    }

    #[test]
    fn signature_depends_on_the_path() {
        let base = StreamSpec {
            host: "h".into(),
            path: "/a.mp3".into(),
            ts: "1".into(),
            s: "salt".into(),
        };
        let other = StreamSpec {
            path: "/b.mp3".into(),
            ..base.clone()
        };
        assert_ne!(base.to_signed_url(), other.to_signed_url());
    }

    #[test]
    fn path_without_a_leading_slash_still_produces_a_valid_url() {
        let spec = StreamSpec {
            host: "h".into(),
            path: "a/b.mp3".into(),
            ts: "9".into(),
            s: "s".into(),
        };
        assert!(spec.to_signed_url().ends_with("/9/a/b.mp3"));
    }

    #[test]
    fn ids_decode_from_strings_and_numbers() {
        assert_eq!(serde_json::from_str::<Id>("\"123\"").unwrap(), Id("123".into()));
        assert_eq!(serde_json::from_str::<Id>("123").unwrap(), Id("123".into()));
    }

    #[test]
    fn album_search_reads_what_a_card_shows() {
        let found: AlbumSearchResponse = serde_json::from_str(
            r#"{"albums": {"results": [
                {"id": 5307396, "title": "Группа крови", "artists": [{"name": "Кино"}],
                 "coverUri": "avatars.yandex.net/a/%%", "year": 1988, "trackCount": 11},
                {"id": "7", "title": "Без обложки"}
            ]}}"#,
        )
        .unwrap();
        let albums: Vec<AlbumInfo> = found
            .albums
            .unwrap()
            .results
            .into_iter()
            .map(RawAlbum::into_info)
            .collect();

        assert_eq!(albums[0].id, "5307396");
        assert_eq!(albums[0].title, "Группа крови");
        assert_eq!(albums[0].artist, "Кино");
        assert_eq!(albums[0].year, Some(1988));
        assert_eq!(albums[0].track_count, Some(11));
        assert_eq!(albums[0].cover_uri.as_deref(), Some("avatars.yandex.net/a/%%"));
        // Missing fields are absent, not a reason to drop the album.
        assert_eq!(albums[1].artist, "");
        assert_eq!(albums[1].cover_uri, None);
    }

    #[test]
    fn an_album_best_match_carries_the_album_to_open() {
        let raw: SuggestResponse = serde_json::from_str(
            r#"{"best": {"type": "album", "result": {"id": 42, "title": "Дикие травы",
                "artists": [{"name": "Мельница"}], "year": 2009, "trackCount": 12}}}"#,
        )
        .unwrap();
        let album = BestMatch::from_raw(raw.best.unwrap()).unwrap().album.unwrap();
        assert_eq!(album.id, "42");
        assert_eq!(album.title, "Дикие травы");
        assert_eq!(album.artist, "Мельница");
        assert_eq!(album.year, Some(2009));

        // Anything else opens nothing: it is searched for, as before.
        let raw: SuggestResponse = serde_json::from_str(
            r#"{"best": {"type": "artist", "result": {"id": 1, "name": "Мельница"}}}"#,
        )
        .unwrap();
        assert!(BestMatch::from_raw(raw.best.unwrap()).unwrap().album.is_none());
    }

    #[test]
    fn suggest_reads_the_best_match_of_each_kind() {
        let artist: SuggestResponse = serde_json::from_str(
            r#"{"best": {"type": "artist", "result": {"name": "Мельница",
                "cover": {"uri": "avatars.yandex.net/a/%%"}}},
                "suggestions": ["мельница", "мельница - дороги", ""]}"#,
        )
        .unwrap();
        let best = BestMatch::from_raw(artist.best.unwrap()).unwrap();
        assert_eq!(best.kind, "artist");
        assert_eq!(best.name, "Мельница");
        assert_eq!(best.subtitle, "");
        assert_eq!(best.query, "Мельница");
        assert_eq!(best.cover_uri.as_deref(), Some("avatars.yandex.net/a/%%"));

        let album: SuggestResponse = serde_json::from_str(
            r#"{"best": {"type": "album", "result": {"title": "Дикие травы",
                "artists": [{"name": "Мельница"}], "ogImage": "avatars.yandex.net/b/%%"}}}"#,
        )
        .unwrap();
        let best = BestMatch::from_raw(album.best.unwrap()).unwrap();
        assert_eq!(best.subtitle, "Мельница");
        // What pressing the row searches for: the artist and the title together.
        assert_eq!(best.query, "Мельница Дикие травы");
        assert_eq!(best.cover_uri.as_deref(), Some("avatars.yandex.net/b/%%"));

        // A kind with nothing to draw is dropped; the plain lines remain.
        let playlist: SuggestResponse =
            serde_json::from_str(r#"{"best": {"type": "playlist", "result": {"title": "X"}}}"#)
                .unwrap();
        assert!(BestMatch::from_raw(playlist.best.unwrap()).is_none());
    }

    #[test]
    fn cover_comes_from_the_track_else_from_its_album() {
        let own: Track = serde_json::from_str(
            r#"{"id": 1, "coverUri": "avatars.yandex.net/t/%%",
                "albums": [{"id": 2, "coverUri": "avatars.yandex.net/a/%%"}]}"#,
        )
        .unwrap();
        assert_eq!(own.to_track_ref().cover_uri.as_deref(), Some("avatars.yandex.net/t/%%"));

        let borrowed: Track = serde_json::from_str(
            r#"{"id": 1, "albums": [{"id": 2, "coverUri": "avatars.yandex.net/a/%%"}]}"#,
        )
        .unwrap();
        assert_eq!(borrowed.to_track_ref().cover_uri.as_deref(), Some("avatars.yandex.net/a/%%"));

        let none: Track = serde_json::from_str(r#"{"id": 1, "coverUri": ""}"#).unwrap();
        assert_eq!(none.to_track_ref().cover_uri, None);
    }

    #[test]
    fn track_decodes_a_realistic_payload() {
        let track: Track = serde_json::from_str(
            r#"{
                "id": 42,
                "title": "Группа крови",
                "durationMs": 286000,
                "available": true,
                "artists": [{"name": "Кино"}],
                "albums": [{"id": "77", "title": "Группа крови"}]
            }"#,
        )
        .unwrap();
        assert_eq!(track.id.0, "42");
        assert_eq!(track.duration_ms, 286_000);
        assert_eq!(track.artist_names(), "Кино");
        assert_eq!(track.album_id().as_deref(), Some("77"));
        assert_eq!(track.to_track_ref().track_id, "42");
    }

    #[test]
    fn absent_optional_track_fields_do_not_break_decoding() {
        let track: Track = serde_json::from_str(r#"{"id": "7"}"#).unwrap();
        assert!(track.available);
        assert_eq!(track.artist_names(), "Unknown artist");
        assert_eq!(track.album_id(), None);
    }

    /// Yandex's search response really does repeat `albums` inside every track
    /// object, so the client parses through `Value` rather than straight into
    /// these structs.
    #[test]
    fn duplicate_keys_in_a_track_payload_are_tolerated_via_value() {
        let json = r#"{
            "id": 1,
            "title": "T",
            "durationMs": 1000,
            "albums": [{"id": 10}],
            "albums": [{"id": 10}]
        }"#;

        assert!(
            serde_json::from_str::<Track>(json).is_err(),
            "a derived deserialiser is expected to reject duplicate keys"
        );

        let document: serde_json::Value = serde_json::from_str(json).unwrap();
        let track: Track = serde_json::from_value(document).unwrap();
        assert_eq!(track.album_id().as_deref(), Some("10"));
    }

    #[test]
    fn stream_spec_decodes_from_xml_and_json() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <download-info>
              <host>cdn.example.net</host>
              <path>/get-mp3/x/1.mp3</path>
              <ts>abc</ts>
              <region>225</region>
              <s>sig</s>
            </download-info>"#;
        let from_xml: StreamSpec = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(from_xml.host, "cdn.example.net");
        assert_eq!(from_xml.s, "sig");

        let json = r#"{"host":"cdn.example.net","path":"/get-mp3/x/1.mp3","ts":"abc","s":"sig","region":225}"#;
        let from_json: StreamSpec = serde_json::from_str(json).unwrap();
        assert_eq!(from_json.to_signed_url(), from_xml.to_signed_url());
    }

    #[test]
    fn track_ids_are_accepted_as_digits_or_urls() {
        assert_eq!(parse_track_id(" 12345 ").unwrap(), "12345");
        assert_eq!(
            parse_track_id("https://music.yandex.ru/album/77/track/42").unwrap(),
            "42"
        );
        assert_eq!(
            parse_track_id("https://music.yandex.ru/album/77/track/42?utm=x").unwrap(),
            "42"
        );
        assert!(parse_track_id("not-an-id").is_err());
        assert!(parse_track_id("").is_err());
    }

    #[test]
    fn album_ids_are_accepted_as_digits_or_urls() {
        assert_eq!(parse_album_id(" 77 ").unwrap(), "77");
        assert_eq!(
            parse_album_id("https://music.yandex.ru/album/77").unwrap(),
            "77"
        );
        // A track URL names its album too, which is a useful shorthand.
        assert_eq!(
            parse_album_id("https://music.yandex.ru/album/77/track/42").unwrap(),
            "77"
        );
        assert!(parse_album_id("abc").is_err());
    }

    #[test]
    fn playlist_refs_are_accepted_as_pairs_or_urls() {
        assert_eq!(
            parse_playlist_ref("mshiv/1017").unwrap(),
            ("mshiv".to_string(), "1017".to_string())
        );
        assert_eq!(
            parse_playlist_ref("https://music.yandex.ru/users/mshiv/playlists/1017").unwrap(),
            ("mshiv".to_string(), "1017".to_string())
        );
        assert_eq!(
            parse_playlist_ref("https://music.yandex.ru/users/mshiv/playlists/3/").unwrap(),
            ("mshiv".to_string(), "3".to_string())
        );
        assert!(parse_playlist_ref("mshiv").is_err());
        assert!(parse_playlist_ref("").is_err());
    }

    #[test]
    fn album_tracks_are_flattened_across_discs() {
        let album: AlbumWithTracks = serde_json::from_str(
            r#"{"volumes": [
                [{"id": 1, "title": "A"}, {"id": 2, "title": "B"}],
                [{"id": 3, "title": "C"}]
            ]}"#,
        )
        .unwrap();
        let ids: Vec<String> = album
            .volumes
            .into_iter()
            .flatten()
            .map(|t| t.id.0)
            .collect();
        assert_eq!(ids, ["1", "2", "3"]);
    }

    #[test]
    fn playlist_entries_without_a_track_are_dropped() {
        let playlist: PlaylistResponse = serde_json::from_str(
            r#"{"tracks": [
                {"id": 1, "track": {"id": 10, "title": "A"}},
                {"id": 2},
                {"id": 3, "track": {"id": 30, "title": "C"}}
            ]}"#,
        )
        .unwrap();
        let ids: Vec<String> = playlist
            .tracks
            .into_iter()
            .filter_map(|entry| entry.track)
            .map(|t| t.id.0)
            .collect();
        assert_eq!(ids, ["10", "30"]);
    }

    /// A station wraps its tracks in a `sequence`, and the surrounding fields —
    /// `batchId`, `trackParameters`, `liked` — are of no interest here.
    #[test]
    fn station_batches_yield_their_tracks_in_order() {
        let batch: StationBatch = serde_json::from_str(
            r#"{
              "batchId": "1787082047311347-1829.bPZU",
              "sequence": [
                {"type": "track", "liked": false, "trackParameters": {"bpm": 0},
                 "track": {"id": 152610414, "title": "Bloodstained II",
                           "durationMs": 142100, "artists": [{"name": "NINETRAUMA"}],
                           "albums": [{"id": 42613198}]}},
                {"type": "track", "liked": true},
                {"type": "track", "track": {"id": "9", "title": "B"}}
              ]
            }"#,
        )
        .unwrap();

        let tracks: Vec<Track> = batch
            .sequence
            .into_iter()
            .filter_map(|entry| entry.track)
            .collect();
        let ids: Vec<&str> = tracks.iter().map(|t| t.id.0.as_str()).collect();
        assert_eq!(ids, ["152610414", "9"], "an entry without a track is dropped");
        assert_eq!(tracks[0].artist_names(), "NINETRAUMA");
        assert_eq!(tracks[0].duration_ms, 142_100);
    }

    #[test]
    fn signed_urls_are_not_echoed_into_errors() {
        // The query string goes, and so does the signed blob in the path.
        assert_eq!(
            redact_url("https://h/get-mp3/sig/1.mp3?track-id=1&secret=abc"),
            "https://h/get-mp3/sig/1.mp3"
        );
        let signed = format!("https://h/get-mp3/{}/1.mp3", "S".repeat(900));
        let redacted = redact_url(&signed);
        assert!(redacted.starts_with("https://h/get-mp3/SSSSSSSS…/1.mp3"), "{redacted}");
        assert!(redacted.len() < 60, "still {} chars", redacted.len());
        // Ordinary API paths are short and must stay readable.
        assert_eq!(
            redact_url("https://api.music.yandex.net/tracks/42/download-info"),
            "https://api.music.yandex.net/tracks/42/download-info"
        );
    }

    #[test]
    fn long_bodies_are_truncated_in_errors() {
        let body = "x".repeat(ERROR_EXCERPT + 50);
        let text = excerpt(&body);
        assert!(text.ends_with('…'));
        assert_eq!(text.chars().count(), ERROR_EXCERPT + 1);
    }
}
