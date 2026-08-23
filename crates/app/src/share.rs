//! Serving this machine's cached tracks to the room, and fetching a track from a
//! peer that already has it.
//!
//! The bytes go straight from one player to another and never through the relay.
//! That keeps the relay small — hosting a room from a phone does not turn it into
//! a file server for everybody — and it means the transfer can be checked with
//! nothing more than `curl`.
//!
//! # Why the HTTP is written out by hand
//!
//! There is exactly one route, `GET /t/<track id>`, and both ends of it are in
//! this repository. A server framework would be a large dependency, and on
//! Android an extra one to cross-compile, for a request this project fully
//! controls. What is served is bounded and boring: a status line, a length, and a
//! file.
//!
//! # Security
//!
//! The room's shared secret is required on every request, compared in
//! length-independent time by [`ymsync_proto::secret_eq`]. It travels in cleartext
//! over the local network, exactly as it already does to a `ws://` relay, so the
//! threat model has not changed: this belongs on a home network, not on the
//! internet. The track id is checked by [`cache::valid_id`] before it is allowed
//! anywhere near a path, which is what stops a request from reaching outside the
//! cache directory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use ymsync_proto::secret_eq;

use crate::cache::{self, Cache};

/// Carries the room's shared secret.
const TOKEN_HEADER: &str = "x-room-token";
/// Carries the MD5 of the body, so a receiver can tell the bytes arrived whole.
const DIGEST_HEADER: &str = "x-track-digest";

const PATH_PREFIX: &str = "/t/";

/// Longest request head accepted. Requests are one line and one header, so this
/// is already generous; the cap exists so a peer cannot make us buffer without
/// end.
const MAX_HEAD: usize = 8 * 1024;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A track is a few megabytes and the network is local, but a phone on poor
/// Wi-Fi is slow rather than broken.
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// How many tracks to serve at once.
///
/// Bounded because the host may be a phone, and because every transfer holds a
/// track's worth of memory while it is written.
const MAX_CONCURRENT: usize = 4;

/// Connect timeout when pulling from a peer. On a local network a peer either
/// answers at once or is not there; a long wait here would stall a track change.
const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PEER_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// A running file server. Dropping it stops listening.
pub struct Server {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The port to announce to the room.
    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    pub async fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.as_ref() {
            task.abort();
        }
    }
}

/// Starts serving `cache` on `port`.
///
/// Binds every interface: the point is to be reachable from the other machines on
/// the network. Port 0 asks the OS for a free one, which [`Server::port`] then
/// reports — that is what gets announced to the room.
pub async fn serve(cache: Arc<Cache>, token: impl Into<String>, port: u16) -> Result<Server> {
    let token = token.into();
    if token.trim().is_empty() {
        bail!("нельзя раздавать треки без токена комнаты: его проверяет каждый запрос");
    }

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen for track requests on port {port}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading the file server's own address")?;
    debug!(bind = %local_addr, "serving cached tracks");

    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(accept_loop(
        listener,
        cache,
        Arc::new(token),
        shutdown_rx,
    ));

    Ok(Server {
        local_addr,
        shutdown,
        task: Some(task),
    })
}

async fn accept_loop(
    listener: TcpListener,
    cache: Arc<Cache>,
    token: Arc<String>,
    shutdown: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT));
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(%err, "accept failed on the track server");
                continue;
            }
        };
        let cache = Arc::clone(&cache);
        let token = Arc::clone(&token);
        let permits = Arc::clone(&permits);
        let mut shutdown = shutdown.clone();

        tokio::spawn(async move {
            // Held for the whole transfer, so several peers asking at once queue
            // rather than all pulling megabytes through a phone together.
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let served = tokio::select! {
                result = handle(stream, &cache, &token) => result,
                _ = shutdown.changed() => Ok(()),
            };
            if let Err(err) = served {
                debug!(%from, "track request failed: {err:#}");
            }
        });
    }
}

/// What a request turned out to be asking for.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// Send the whole track.
    Audio(String),
    /// `HEAD`: the length and digest, no body. Handy for checking a peer has a
    /// track without pulling it.
    Head(String),
    Status(u16, &'static str),
}

async fn handle(mut stream: TcpStream, cache: &Cache, token: &str) -> Result<()> {
    let _ = stream.set_nodelay(true);

    let head = match tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream)).await {
        Err(_) => {
            return write_status(&mut stream, 408, "Request Timeout").await;
        }
        Ok(head) => head?,
    };

    match decide(&head, token) {
        Reply::Status(code, reason) => write_status(&mut stream, code, reason).await,
        Reply::Head(id) => {
            let Some((len, digest)) = length_and_digest(cache, &id) else {
                return write_status(&mut stream, 404, "Not Found").await;
            };
            let head = audio_headers(len, &digest);
            write_all(&mut stream, head.as_bytes()).await
        }
        Reply::Audio(id) => {
            // The read is blocking, and it verifies the file against the length
            // recorded when it was stored — so a peer is never handed a track
            // that has rotted on this disk.
            let data = match tokio::task::block_in_place(|| cache.read(&id)) {
                Ok(data) => data,
                Err(err) => {
                    debug!(track = %id, "cannot serve: {err:#}");
                    return write_status(&mut stream, 404, "Not Found").await;
                }
            };
            let digest = cache.digest(&id).unwrap_or_default();
            let head = audio_headers(data.len() as u64, &digest);
            write_all(&mut stream, head.as_bytes()).await?;
            write_all(&mut stream, &data).await?;
            debug!(track = %id, bytes = data.len(), "served a cached track");
            Ok(())
        }
    }
}

fn length_and_digest(cache: &Cache, id: &str) -> Option<(u64, String)> {
    let digest = cache.digest(id)?;
    let bytes = cache.byte_len(id)?;
    Some((bytes, digest))
}

/// Reads up to the end of the request head.
async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    // Byte at a time: the head is a couple of hundred bytes, and this way no part
    // of the body is ever swallowed into the buffer.
    loop {
        let read = stream.read(&mut byte).await.context("reading the request")?;
        if read == 0 {
            bail!("peer closed the connection mid-request");
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
        if head.len() > MAX_HEAD {
            bail!("request head longer than {MAX_HEAD} bytes");
        }
    }
}

/// Works out what to answer, without touching the disk or the network.
fn decide(head: &[u8], token: &str) -> Reply {
    let Ok(text) = std::str::from_utf8(head) else {
        return Reply::Status(400, "Bad Request");
    };
    let mut lines = text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Reply::Status(400, "Bad Request");
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Reply::Status(400, "Bad Request");
    };

    let given = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(TOKEN_HEADER)
            .then(|| value.trim())
    });
    // Checked before the path, so a request without the secret learns nothing
    // about which tracks exist.
    if !given.is_some_and(|given| secret_eq(token, given)) {
        return Reply::Status(401, "Unauthorized");
    }

    let Some(id) = path.strip_prefix(PATH_PREFIX) else {
        return Reply::Status(404, "Not Found");
    };
    // The one check that keeps a request inside the cache directory.
    if !cache::valid_id(id) {
        return Reply::Status(400, "Bad Request");
    }

    match method {
        "GET" => Reply::Audio(id.to_string()),
        "HEAD" => Reply::Head(id.to_string()),
        _ => Reply::Status(405, "Method Not Allowed"),
    }
}

fn audio_headers(len: u64, digest: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: audio/mpeg\r\n\
         Content-Length: {len}\r\n\
         {DIGEST_HEADER}: {digest}\r\n\
         Connection: close\r\n\
         \r\n"
    )
}

async fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    write_all(stream, head.as_bytes()).await
}

async fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes))
        .await
        .context("peer stopped reading")?
        .context("writing the response")?;
    Ok(())
}

/// An HTTP client for pulling tracks off the local network.
///
/// Separate from the Yandex client because the timeouts want to be different: a
/// peer on the same network answers immediately or not at all.
pub fn client() -> Result<reqwest::Client> {
    crate::api::client_builder()
        .connect_timeout(PEER_CONNECT_TIMEOUT)
        .timeout(PEER_TOTAL_TIMEOUT)
        .build()
        .context("building the local-network HTTP client")
}

/// Pulls one track from a peer.
///
/// The digest the peer sends is checked against the bytes that arrive, so a
/// damaged file on the other machine is reported here rather than becoming a
/// track that plays as noise.
pub async fn fetch_from_peer(
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    track_id: &str,
) -> Result<Bytes> {
    if !cache::valid_id(track_id) {
        bail!("«{track_id}» не похоже на id трека");
    }
    let url = format!(
        "{}{PATH_PREFIX}{track_id}",
        endpoint.trim_end_matches('/')
    );

    let response = http
        .get(&url)
        .header(TOKEN_HEADER, token)
        .send()
        .await
        .with_context(|| format!("запрос трека у {endpoint}"))?;

    let status = response.status();
    let advertised = response
        .headers()
        .get(DIGEST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response
        .bytes()
        .await
        .with_context(|| format!("чтение трека от {endpoint}"))?;

    if !status.is_success() {
        bail!("участник {endpoint} ответил HTTP {status}");
    }
    if body.is_empty() {
        bail!("участник {endpoint} прислал пустой трек");
    }
    if let Some(expected) = advertised {
        let actual = cache::digest_of(&body);
        if actual != expected {
            bail!(
                "трек от {endpoint} побился при передаче: контрольная сумма {actual} \
                 вместо {expected}"
            );
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ymsync_proto::TrackRef;

    const TOKEN: &str = "room-secret";

    fn head(method: &str, path: &str, token: Option<&str>) -> Vec<u8> {
        let mut text = format!("{method} {path} HTTP/1.1\r\nHost: peer\r\n");
        if let Some(token) = token {
            text.push_str(&format!("{TOKEN_HEADER}: {token}\r\n"));
        }
        text.push_str("\r\n");
        text.into_bytes()
    }

    #[test]
    fn a_valid_request_asks_for_a_track() {
        let request = head("GET", "/t/42", Some(TOKEN));
        assert_eq!(decide(&request, TOKEN), Reply::Audio("42".to_string()));
    }

    #[test]
    fn head_asks_for_the_length_only() {
        let request = head("HEAD", "/t/42", Some(TOKEN));
        assert_eq!(decide(&request, TOKEN), Reply::Head("42".to_string()));
    }

    /// The header name is case-insensitive in HTTP, and reqwest lowercases it on
    /// the way out, so the check must not depend on the spelling.
    #[test]
    fn the_token_header_is_matched_case_insensitively() {
        let request = format!("GET /t/42 HTTP/1.1\r\nX-Room-Token: {TOKEN}\r\n\r\n").into_bytes();
        assert_eq!(decide(&request, TOKEN), Reply::Audio("42".to_string()));
    }

    #[test]
    fn a_request_without_the_secret_is_refused() {
        let request = head("GET", "/t/42", None);
        assert_eq!(decide(&request, TOKEN), Reply::Status(401, "Unauthorized"));
    }

    #[test]
    fn a_request_with_the_wrong_secret_is_refused() {
        let request = head("GET", "/t/42", Some("guess"));
        assert_eq!(decide(&request, TOKEN), Reply::Status(401, "Unauthorized"));
    }

    /// Answered before the path is even looked at, so an unauthorised caller
    /// cannot use the 404-versus-200 difference to find out what is cached here.
    #[test]
    fn authorisation_is_checked_before_the_path() {
        let nonsense = head("GET", "/../../secrets", None);
        assert_eq!(decide(&nonsense, TOKEN), Reply::Status(401, "Unauthorized"));
    }

    /// The check that keeps a request inside the cache directory.
    #[test]
    fn a_path_that_tries_to_escape_is_refused() {
        for path in ["/t/../../etc/passwd", "/t/..", "/t/a%2Fb", "/t/"] {
            let request = head("GET", path, Some(TOKEN));
            assert_eq!(
                decide(&request, TOKEN),
                Reply::Status(400, "Bad Request"),
                "{path}"
            );
        }
    }

    #[test]
    fn another_route_is_not_found() {
        let request = head("GET", "/tracks/42", Some(TOKEN));
        assert_eq!(decide(&request, TOKEN), Reply::Status(404, "Not Found"));
    }

    #[test]
    fn writing_is_not_allowed() {
        for method in ["POST", "PUT", "DELETE"] {
            let request = head(method, "/t/42", Some(TOKEN));
            assert_eq!(
                decide(&request, TOKEN),
                Reply::Status(405, "Method Not Allowed"),
                "{method}"
            );
        }
    }

    #[test]
    fn the_response_head_states_the_length_and_the_digest() {
        let head = audio_headers(4_096, "abc123");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Length: 4096\r\n"), "{head}");
        assert!(head.contains("x-track-digest: abc123\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "{head}");
    }

    fn track(id: &str) -> TrackRef {
        TrackRef {
            track_id: id.to_string(),
            album_id: None,
            title: format!("Title {id}"),
            artist: "Artist".to_string(),
            duration_ms: 1_000,
        }
    }

    /// A cache in a temporary directory, removed when the test ends.
    struct TempCache {
        cache: Arc<Cache>,
        dir: std::path::PathBuf,
    }

    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_cache(name: &str) -> TempCache {
        let dir = std::env::temp_dir()
            .join(format!("ymsync-share-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Arc::new(Cache::open(&dir, 0).expect("open the cache"));
        TempCache { cache, dir }
    }

    /// The whole path, over a real socket: what one peer serves is what the other
    /// receives, byte for byte.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_receives_exactly_what_was_cached() {
        let temp = temp_cache("roundtrip");
        let audio: Vec<u8> = (0..5_000u32).map(|n| (n % 251) as u8).collect();
        temp.cache.insert(&track("42"), &audio, true).unwrap();

        let server = serve(Arc::clone(&temp.cache), TOKEN, 0).await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}", server.port());
        let http = client().unwrap();

        let fetched = fetch_from_peer(&http, &endpoint, TOKEN, "42")
            .await
            .expect("fetch from the peer");
        assert_eq!(fetched.as_ref(), audio.as_slice());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_track_nobody_has_is_a_clean_failure() {
        let temp = temp_cache("missing");
        let server = serve(Arc::clone(&temp.cache), TOKEN, 0).await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}", server.port());
        let http = client().unwrap();

        let err = fetch_from_peer(&http, &endpoint, TOKEN, "99")
            .await
            .expect_err("nothing to serve");
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_wrong_room_token_gets_nothing() {
        let temp = temp_cache("unauthorised");
        temp.cache.insert(&track("42"), b"audio", true).unwrap();
        let server = serve(Arc::clone(&temp.cache), TOKEN, 0).await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}", server.port());
        let http = client().unwrap();

        let err = fetch_from_peer(&http, &endpoint, "not-the-token", "42")
            .await
            .expect_err("should be refused");
        assert!(format!("{err:#}").contains("401"), "{err:#}");
    }

    /// Port 0 lets the OS pick, and that port is what gets announced to the room.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_reports_the_port_it_was_given() {
        let temp = temp_cache("port");
        let server = serve(Arc::clone(&temp.cache), TOKEN, 0).await.unwrap();
        assert_ne!(server.port(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serving_without_a_room_token_is_refused() {
        let temp = temp_cache("notoken");
        assert!(serve(Arc::clone(&temp.cache), "  ", 0).await.is_err());
    }

    /// Dropping the handle has to free the port, or turning sharing off and on
    /// again would fail on a fixed port.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_server_stops_it() {
        let temp = temp_cache("stop");
        temp.cache.insert(&track("42"), b"audio", true).unwrap();
        let server = serve(Arc::clone(&temp.cache), TOKEN, 0).await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}", server.port());
        drop(server);

        let http = client().unwrap();
        for _ in 0..100 {
            if fetch_from_peer(&http, &endpoint, TOKEN, "42").await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the file server kept answering after being dropped");
    }
}
