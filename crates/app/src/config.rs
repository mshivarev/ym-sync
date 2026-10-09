//! Configuration: a TOML file per machine, overridable from the environment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use ymsync_proto::SyncParams;

pub const DEFAULT_RELAY: &str = "ws://127.0.0.1:8787";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Relay URL. `ws://` for a LAN or localhost relay, `wss://` once it sits
    /// behind TLS. Ignored while `[host].enabled` is set, since the app then
    /// connects to the relay it is running itself.
    pub relay: String,
    /// Players sharing a room name sync with each other.
    pub room: String,
    /// Shared secret; must match the relay's `--token`.
    pub room_token: String,
    /// Yandex OAuth token for *this machine's* account. See the README.
    pub yandex_token: String,
    /// 0.0 to 1.0 (values above 1.0 amplify and may clip).
    pub volume: f32,
    /// What the rest of the room calls this listener. Empty means the device's
    /// own name — see [`Config::display_name`].
    pub name: String,
    pub sync: SyncConfig,
    pub cache: CacheConfig,
    pub share: ShareConfig,
    pub host: HostConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay: DEFAULT_RELAY.to_string(),
            room: "home".to_string(),
            room_token: String::new(),
            yandex_token: String::new(),
            volume: 0.8,
            name: String::new(),
            sync: SyncConfig::default(),
            cache: CacheConfig::default(),
            share: ShareConfig::default(),
            host: HostConfig::default(),
        }
    }
}

/// Where downloaded tracks live and how much room they may take.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Empty means the per-user data directory. Android passes its own
    /// app-private path, which is the only place it may write without asking for
    /// a permission.
    pub dir: String,
    /// Gigabytes. 0 means no limit, and then only an explicit delete frees space.
    pub limit_gb: f64,
    /// Keep every track that plays, not just the ones explicitly downloaded.
    ///
    /// Off by default: a cache that fills itself is a cache the user did not ask
    /// for. With it on, a few evenings of listening leaves a usable offline
    /// library, and the limit above is what stops it growing without end.
    pub auto: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            limit_gb: 8.0,
            auto: false,
        }
    }
}

impl CacheConfig {
    pub fn limit_bytes(&self) -> u64 {
        if self.limit_gb <= 0.0 {
            return 0;
        }
        (self.limit_gb * 1024.0 * 1024.0 * 1024.0) as u64
    }

    /// Where to keep tracks, falling back to the per-user data directory.
    pub fn directory(&self) -> Result<PathBuf> {
        if !self.dir.trim().is_empty() {
            return Ok(PathBuf::from(self.dir.trim()));
        }
        let dirs = directories::ProjectDirs::from("", "", "ymsync")
            .context("cannot determine a per-user data directory for the track cache")?;
        Ok(dirs.data_dir().join("tracks"))
    }
}

/// Serving this machine's cached tracks to the rest of the room.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShareConfig {
    /// Whether to offer cached tracks to the others at all.
    pub enabled: bool,
    /// Port for the file server.
    ///
    /// Fixed by default, and deliberately: a firewall rule has to name a port, and
    /// on Windows an inbound connection to a program without one is dropped
    /// silently. With a port that changed every run there was nothing to allow, so
    /// the others saw the track on offer and could never fetch it. 0 still asks the
    /// OS to pick, which is fine on a machine with no firewall in the way.
    pub port: u16,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Next to the relay's 8787 and the discovery port, so all of ym-sync
            // is two adjacent numbers to remember.
            port: 8788,
        }
    }
}

/// Running the room's relay in this process instead of connecting to a separate
/// one.
///
/// This is also how listening offline works: with no network at all, a machine
/// hosts the room on itself, and the queue and playhead have an authority again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub enabled: bool,
    /// Interface to listen on. `0.0.0.0` accepts the local network as well as
    /// this machine; `127.0.0.1` keeps the room strictly private.
    pub bind: String,
    /// 0 asks the OS for a free port. A fixed one is friendlier, because the
    /// others have to type it.
    pub port: u16,
    /// Address to advertise to the others, and to dial ourselves. Empty means
    /// "work it out" — see `ymsync::net`.
    ///
    /// Worth setting by hand on a machine with several networks, where the
    /// automatic answer may pick a VPN or a virtual switch.
    pub advertise: String,
    /// Answer «кто держит комнаты» broadcasts, so the others can find this room
    /// without being told its address.
    ///
    /// The answer names the room and counts its listeners — no token, and nothing
    /// about what is playing. Turn it off to leave the room reachable only for
    /// those given the address by hand.
    pub discoverable: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "0.0.0.0".to_string(),
            port: 8787,
            advertise: String::new(),
            discoverable: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Re-align this peer once it is off the room by more than this.
    pub seek_threshold_ms: i64,
    /// Aim this far ahead when seeking, to cover the seek's own cost.
    pub seek_lead_ms: i64,
    /// Skip corrections while the relay round trip is worse than this.
    pub max_rtt_ms: i64,
    /// How often this peer compares itself against the room.
    pub correction_interval_ms: u64,
    /// Clock probe interval, in seconds.
    pub clock_probe_secs: u64,
    /// Calibration for a player that reports its playhead with a constant lag.
    ///
    /// ExoPlayer trails the position it was seeked to by the length of its audio
    /// pipeline — a few hundred milliseconds on a phone — so the engine would
    /// chase an offset it can never remove. This value is *added* to the position
    /// the player reports before any comparison, which cancels the lag instead of
    /// correcting it.
    ///
    /// Calibrate from what the slave shows with the sign flipped: a steady
    /// `рассинхрон -400 мс` means `position_bias_ms = 400`. Zero for the desktop
    /// player, which reports its own buffer honestly.
    pub position_bias_ms: i64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            seek_threshold_ms: 300,
            seek_lead_ms: 40,
            max_rtt_ms: 400,
            correction_interval_ms: 250,
            clock_probe_secs: 5,
            position_bias_ms: 0,
        }
    }
}

impl SyncConfig {
    pub fn params(&self) -> SyncParams {
        SyncParams {
            seek_threshold_ms: self.seek_threshold_ms,
            seek_lead_ms: self.seek_lead_ms,
            max_rtt_ms: self.max_rtt_ms,
        }
    }
}

impl Config {
    /// Default config location, e.g. `%APPDATA%\ymsync\config\config.toml`.
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "ymsync")
            .context("cannot determine a per-user config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Loads the file if it exists, falls back to defaults if it does not, then
    /// applies environment overrides.
    pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => Self::default_path()?,
        };
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        config.apply_env();
        Ok((config, path))
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("YMSYNC_RELAY") {
            self.relay = v;
        }
        if let Ok(v) = std::env::var("YMSYNC_ROOM") {
            self.room = v;
        }
        if let Ok(v) = std::env::var("YMSYNC_ROOM_TOKEN") {
            self.room_token = v;
        }
        // YM_TOKEN is what most community tooling calls it.
        for key in ["YMSYNC_YANDEX_TOKEN", "YM_TOKEN"] {
            if let Ok(v) = std::env::var(key) {
                self.yandex_token = v;
            }
        }
    }

    /// Writes a starter config, creating parent directories. Refuses to clobber
    /// an existing file.
    pub fn write_template(path: &Path) -> Result<()> {
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        Self::default().save(path)
    }

    /// Writes this configuration to `path`, creating parent directories.
    ///
    /// The window saves what was typed into it — the room, its password, where to
    /// connect — so that a room is set up once rather than at every launch. Hand
    /// written comments do not survive that, which is why the preamble is
    /// re-emitted here instead of living only in the template.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialising the configuration")?;
        let text = format!(
            "# ym-sync configuration.\n\
             #\n\
             # yandex_token: OAuth token for this machine's Yandex account (see README).\n\
             # room_token:   the room's password; everyone in the room needs the same one.\n\
             # Both can also be supplied as YM_TOKEN and YMSYNC_ROOM_TOKEN.\n\
             \n{body}"
        );
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The name the room is told: the one chosen, or failing that the computer's
    /// own — a list of three blank lines would say nothing about who is who.
    pub fn display_name(&self) -> String {
        let chosen = self.name.trim();
        if !chosen.is_empty() {
            return chosen.to_string();
        }
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "без имени".to_string())
    }

    pub fn require_yandex_token(&self) -> Result<&str> {
        if self.yandex_token.trim().is_empty() {
            bail!(
                "нет токена Яндекса: впишите yandex_token в конфиг или задайте \
                 переменную окружения YM_TOKEN (см. README)"
            );
        }
        Ok(self.yandex_token.trim())
    }

    pub fn require_room_token(&self) -> Result<&str> {
        if self.room_token.trim().is_empty() {
            bail!(
                "нет токена комнаты: впишите room_token в конфиг или задайте \
                 переменную окружения YMSYNC_ROOM_TOKEN — значение должно совпадать \
                 с --token у релея"
            );
        }
        Ok(self.room_token.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_a_toml_roundtrip() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.relay, DEFAULT_RELAY);
        assert_eq!(parsed.sync.seek_threshold_ms, 300);
    }

    #[test]
    fn partial_config_keeps_defaults_for_the_rest() {
        let parsed: Config = toml::from_str("room = \"kitchen\"\n").unwrap();
        assert_eq!(parsed.room, "kitchen");
        assert_eq!(parsed.relay, DEFAULT_RELAY);
        assert_eq!(parsed.sync.correction_interval_ms, 250);
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_silently_ignored() {
        let err = toml::from_str::<Config>("romm = \"typo\"\n").unwrap_err();
        assert!(err.to_string().contains("romm"), "{err}");
    }

    #[test]
    fn missing_tokens_are_reported() {
        let config = Config::default();
        assert!(config.require_yandex_token().is_err());
        assert!(config.require_room_token().is_err());
    }

    #[test]
    fn sync_config_maps_onto_proto_params() {
        let params = SyncConfig::default().params();
        assert_eq!(params, SyncParams::default());
    }

    /// The desktop player reports its buffer honestly, so calibration is opt-in.
    #[test]
    fn the_position_bias_defaults_to_no_correction() {
        assert_eq!(SyncConfig::default().position_bias_ms, 0);
    }

    /// The Android client hands settings over as JSON rather than TOML, with only
    /// the sync field it knows about. `deny_unknown_fields` makes that shape worth
    /// pinning down.
    #[test]
    fn the_android_config_shape_parses() {
        let json = r#"{
            "relay": "ws://192.168.3.203:8787",
            "room": "home",
            "room_token": "secret",
            "yandex_token": "y0_token",
            "volume": 0.8,
            "sync": { "position_bias_ms": 400 },
            "cache": { "dir": "/data/user/0/dev.mshiv.ymsync/files/tracks", "limit_gb": 2.0 },
            "share": { "enabled": true, "port": 0 },
            "host": { "enabled": true, "port": 8787, "advertise": "192.168.3.50" }
        }"#;
        let config: Config = serde_json::from_str(json).expect("android config");
        assert_eq!(config.relay, "ws://192.168.3.203:8787");
        assert_eq!(config.sync.position_bias_ms, 400);
        assert_eq!(
            config.cache.dir,
            "/data/user/0/dev.mshiv.ymsync/files/tracks"
        );
        assert!(config.host.enabled);
        assert_eq!(config.host.advertise, "192.168.3.50");
        // Everything the client left out keeps its default.
        assert_eq!(config.sync.seek_threshold_ms, 300);
        assert_eq!(config.sync.correction_interval_ms, 250);
        // The phone asks the system for a port on purpose: there is no firewall
        // rule to name one, and a fixed port could already be taken by another app.
        assert_eq!(config.share.port, 0);
        assert_eq!(config.host.bind, "0.0.0.0");
    }

    /// Caching, sharing and hosting arrived after people already had config
    /// files, and all three are optional, so an older file has to keep working
    /// untouched.
    #[test]
    fn a_config_from_before_the_offline_features_still_loads() {
        let text = "\
relay = \"ws://192.168.1.10:8787\"
room = \"home\"
room_token = \"secret\"
yandex_token = \"y0_x\"
volume = 0.8

[sync]
seek_threshold_ms = 300
";
        let parsed: Config = toml::from_str(text).expect("an older config");
        assert_eq!(parsed.relay, "ws://192.168.1.10:8787");
        assert_eq!(parsed.cache.limit_gb, 8.0);
        assert!(!parsed.cache.auto, "automatic caching is opt-in");
        assert!(parsed.share.enabled);
        // A fixed port is what a firewall rule can name; a config written before
        // this existed picks it up without being edited.
        assert_eq!(parsed.share.port, 8788);
        assert!(!parsed.host.enabled, "hosting is opt-in");
        assert!(
            parsed.host.discoverable,
            "an older config still answers a search for rooms"
        );
    }

    /// The window writes back what was typed into it, so a saved file has to load
    /// again as exactly the same settings — including the room's password, which
    /// is the one value nobody wants to retype.
    #[test]
    fn a_saved_config_loads_back_unchanged() {
        let dir = std::env::temp_dir().join(format!("ymsync-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let config = Config {
            relay: "ws://192.168.1.10:8787".to_string(),
            room: "кухня".to_string(),
            room_token: "пароль".to_string(),
            yandex_token: "y0_x".to_string(),
            host: HostConfig {
                enabled: true,
                ..HostConfig::default()
            },
            ..Config::default()
        };
        config.save(&path).expect("save");

        let (loaded, _) = Config::load(Some(&path)).expect("load");
        assert_eq!(loaded.room, "кухня");
        assert_eq!(loaded.room_token, "пароль");
        assert_eq!(loaded.relay, "ws://192.168.1.10:8787");
        assert!(loaded.host.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_limit_converts_to_bytes() {
        let cache = CacheConfig {
            limit_gb: 2.0,
            ..CacheConfig::default()
        };
        assert_eq!(cache.limit_bytes(), 2 * 1024 * 1024 * 1024);
    }

    /// 0 is how the config says "no limit", and a negative figure is a typo that
    /// must not come out as an enormous unsigned number.
    #[test]
    fn a_zero_or_negative_limit_means_unlimited() {
        for limit_gb in [0.0, -1.0] {
            let cache = CacheConfig {
                limit_gb,
                ..CacheConfig::default()
            };
            assert_eq!(cache.limit_bytes(), 0, "limit_gb = {limit_gb}");
        }
    }

    /// Android has to write inside its own sandbox, so an explicit directory
    /// always wins over the per-user default.
    #[test]
    fn an_explicit_cache_directory_is_used_as_given() {
        let cache = CacheConfig {
            dir: "  /tmp/ymsync-tracks  ".to_string(),
            ..CacheConfig::default()
        };
        assert_eq!(
            cache.directory().unwrap(),
            std::path::PathBuf::from("/tmp/ymsync-tracks")
        );
    }
}
