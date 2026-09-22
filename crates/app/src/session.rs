//! Bringing a player up: the room's relay if this machine hosts it, the file
//! server for cached tracks, and the sync engine.
//!
//! All three front ends go through here. Each of the three pieces has to be set
//! up in a particular order — the relay has to be listening before the engine can
//! connect to it, and the file server's port has to be known before the engine can
//! announce it — and doing that in one place is what keeps the CLI, the window and
//! the Android service from each getting it subtly differently.
//!
//! # Hosting is also what "offline" means
//!
//! There is no relay-less mode. The room's queue and playhead live in the relay,
//! and inventing a second authority for the offline case would mean two code paths
//! that have to agree. Instead, a machine with no internet hosts the room on
//! itself: the relay binds a socket, the player connects to it, and everything
//! downstream is unchanged. Playing alone on a train and hosting for the flat are
//! the same feature with a different bind address.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use crate::api::YandexMusic;
use crate::cache::Cache;
use crate::config::Config;
use crate::engine::{self, Handle, Wiring};
use crate::likes::Likes;
use crate::playback::Playback;
use crate::{net, share};

/// A running player, and whatever it is hosting.
///
/// The servers are held here rather than detached, so that closing the session
/// closes the ports with it: a front end that stops and starts again would
/// otherwise find its own fixed port taken.
pub struct Session {
    handle: Handle,
    relay: Option<ymsync_relay::Server>,
    share: Option<share::Server>,
    /// The address to give other people, when this app is hosting.
    join_url: Option<String>,
}

impl Session {
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// What to tell the others to type in, when this app is hosting the room.
    pub fn join_url(&self) -> Option<&str> {
        self.join_url.as_deref()
    }

    /// The port cached tracks are being served on, if sharing is on.
    pub fn share_port(&self) -> Option<u16> {
        self.share.as_ref().map(share::Server::port)
    }

    /// Stops the engine, then the servers.
    ///
    /// In that order deliberately: the engine says goodbye to the relay, and a
    /// hosted relay should still be there to hear it.
    pub async fn shutdown(mut self) -> Result<()> {
        self.handle.send(engine::Command::Shutdown);
        let outcome = self.handle.join().await;

        if let Some(server) = self.share.as_mut() {
            server.stop().await;
        }
        if let Some(server) = self.relay.as_mut() {
            server.stop().await;
        }
        outcome
    }
}

/// Opens the track cache described by the config.
///
/// Separate from [`start`] because a front end wants the offline library before it
/// connects to anything: that list is how a queue gets built with no network.
pub fn open_cache(cfg: &Config) -> Result<Arc<Cache>> {
    let dir = cfg.cache.directory()?;
    let cache = Cache::open(dir, cfg.cache.limit_bytes())?;
    Ok(Arc::new(cache))
}

/// Opens the stored «Мне нравится», which lives beside the downloads.
///
/// Separate from [`start`] for the same reason as the cache: the list and the
/// hearts drawn from it are wanted on screen before anything connects, and both
/// come off the disk.
pub fn open_likes(cache: &Cache) -> Arc<Likes> {
    Likes::open(cache.directory())
}

/// Brings up hosting, sharing and the engine.
pub async fn start(
    cfg: &Config,
    // `None` when this device has no Yandex token; see `engine::Wiring::api`.
    api: Option<Arc<YandexMusic>>,
    player: Arc<dyn Playback>,
    cache: Arc<Cache>,
    likes: Arc<Likes>,
) -> Result<Session> {
    let room_token = cfg.require_room_token()?.to_string();

    // 1. Host the room, if asked. This has to come first: the engine connects
    //    below, and there has to be something listening by then.
    let (relay, relay_url, join_url) = if cfg.host.enabled {
        let bind = format!("{}:{}", cfg.host.bind, cfg.host.port);
        let server = ymsync_relay::bind_with(&bind, room_token.clone(), cfg.host.discoverable)
            .await
            .context("не удалось поднять комнату на этом устройстве")?;
        let port = server.local_addr().port();
        let host = net::advertise_host(&cfg.host.advertise);
        let url = net::relay_url(&host, port);

        info!(room = %cfg.room, url = %url, "hosting the room");
        // Connecting to our own LAN address rather than to 127.0.0.1 is not
        // cosmetic. The relay composes every peer's file-server address from the
        // socket that peer arrives on, so reaching our own relay over loopback
        // would advertise a loopback address for our cached tracks and nobody
        // else could fetch them.
        (Some(server), url.clone(), Some(url))
    } else {
        (None, cfg.relay.clone(), None)
    };

    // 2. Serve this machine's cached tracks. Before the engine, so the port is
    //    known and can go out with the first announcement.
    let share = if cfg.share.enabled {
        match share::serve(Arc::clone(&cache), room_token, cfg.share.port).await {
            Ok(server) => {
                info!(port = server.port(), "sharing cached tracks with the room");
                Some(server)
            }
            // A port that will not bind is not worth failing to play music over;
            // this peer just cannot serve anybody.
            Err(err) => {
                tracing::warn!("раздача треков не включилась: {err:#}");
                None
            }
        }
    } else {
        None
    };
    let share_port = share.as_ref().map(share::Server::port);

    let handle = engine::spawn(
        cfg,
        Wiring {
            api,
            player,
            cache,
            likes,
            relay_url,
            share_port,
            hosting: join_url.clone(),
        },
    )
    .await?;

    Ok(Session {
        handle,
        relay,
        share,
        join_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HostConfig, ShareConfig};

    fn config() -> Config {
        Config {
            room_token: "secret".to_string(),
            ..Config::default()
        }
    }

    /// Hosting has to be able to pick a port the OS chose, because 8787 may
    /// already be taken by a relay someone else left running.
    #[tokio::test(flavor = "multi_thread")]
    async fn hosting_reports_an_address_others_can_use() {
        let cfg = Config {
            host: HostConfig {
                enabled: true,
                bind: "127.0.0.1".to_string(),
                port: 0,
                advertise: "192.168.1.50".to_string(),
                discoverable: true,
            },
            share: ShareConfig {
                enabled: false,
                port: 0,
            },
            ..config()
        };

        // Only the hosting half is exercised here: the engine needs an audio
        // backend and a Yandex token, which a unit test has neither of.
        let server = ymsync_relay::bind(&format!("{}:{}", cfg.host.bind, cfg.host.port), "secret")
            .await
            .expect("bind");
        let port = server.local_addr().port();
        assert_ne!(port, 0, "the OS should have chosen a port");

        let url = net::relay_url(&net::advertise_host(&cfg.host.advertise), port);
        assert_eq!(url, format!("ws://192.168.1.50:{port}"));
    }

    #[test]
    fn a_session_needs_a_room_token() {
        let cfg = Config::default();
        assert!(cfg.require_room_token().is_err());
    }

    #[test]
    fn the_cache_opens_where_the_config_says() {
        let dir = std::env::temp_dir().join(format!("ymsync-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            cache: crate::config::CacheConfig {
                dir: dir.to_string_lossy().to_string(),
                limit_gb: 1.0,
                auto: false,
            },
            ..config()
        };

        let cache = open_cache(&cfg).expect("open the cache");
        assert_eq!(cache.directory(), dir.as_path());
        assert_eq!(cache.limit_bytes(), 1024 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
