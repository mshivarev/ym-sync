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
    /// behind TLS.
    pub relay: String,
    /// Players sharing a room name sync with each other.
    pub room: String,
    /// Shared secret; must match the relay's `--token`.
    pub room_token: String,
    /// Yandex OAuth token for *this machine's* account. See the README.
    pub yandex_token: String,
    /// 0.0 to 1.0 (values above 1.0 amplify and may clip).
    pub volume: f32,
    pub sync: SyncConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay: DEFAULT_RELAY.to_string(),
            room: "home".to_string(),
            room_token: String::new(),
            yandex_token: String::new(),
            volume: 0.8,
            sync: SyncConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Re-align the slave once it is off by more than this.
    pub seek_threshold_ms: i64,
    /// Aim this far ahead when seeking, to cover the seek's own cost.
    pub seek_lead_ms: i64,
    /// Skip corrections while the relay round trip is worse than this.
    pub max_rtt_ms: i64,
    /// How often the master publishes its playhead.
    pub heartbeat_ms: u64,
    /// How often the slave compares itself against the master.
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
            heartbeat_ms: 1_000,
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(&Self::default()).context("serialising defaults")?;
        let text = format!(
            "# ym-sync configuration.\n\
             #\n\
             # yandex_token: OAuth token for this machine's Yandex account (see README).\n\
             # room_token:   shared secret, must match the relay's --token.\n\
             # Both can also be supplied as YM_TOKEN and YMSYNC_ROOM_TOKEN.\n\
             \n{body}"
        );
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
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
        assert_eq!(parsed.sync.heartbeat_ms, 1_000);
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
            "sync": { "position_bias_ms": 400 }
        }"#;
        let config: Config = serde_json::from_str(json).expect("android config");
        assert_eq!(config.relay, "ws://192.168.3.203:8787");
        assert_eq!(config.sync.position_bias_ms, 400);
        // Everything the client left out keeps its default.
        assert_eq!(config.sync.seek_threshold_ms, 300);
        assert_eq!(config.sync.heartbeat_ms, 1_000);
    }
}
