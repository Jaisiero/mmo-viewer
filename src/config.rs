//! Viewer-specific configuration.
//!
//! Wraps `mmo_cli::config::ClientConfig` (auth/gateway URLs, input_hz,
//! retry counts) and adds the viewer's own knobs: login credentials and
//! a default start position for the first gateway hand-off.
//!
//! Loaded from `viewer.toml` in the current directory, then overridden
//! by any `VIEWER_*` env vars. Falls back to sensible localhost defaults
//! so `cargo run -p mmo-viewer` "just works" against a local stack.

use config::{Config, Environment, File};
use mmo_cli::config::ClientConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ViewerConfig {
    /// Everything the CLI already needs (URLs, retry counts, input_hz…).
    #[serde(flatten)]
    pub client: ClientConfig,

    /// Account to log in as. The viewer `register`s this name first so
    /// you don't have to pre-provision the account on a dev stack.
    pub username: String,
    pub password: String,

    /// Seed position for the initial `gateway.player_connect`. If
    /// `load_player_state` returns a stored spawn, that wins. This is
    /// only used on very first login or if the DB is wiped.
    pub start_x: f32,
    pub start_z: f32,

    /// How wide a slice of the world to render (in world units). The
    /// camera is isotropic so this sets both horizontal and vertical
    /// extent based on window aspect ratio.
    pub view_range: f32,

    /// Entities we haven't heard from in this many seconds are dropped
    /// from the mirrored state. Needs to be larger than the shard's AOI
    /// broadcast period plus a small safety margin.
    pub stale_entity_secs: f32,
}

impl ViewerConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        let cfg = Config::builder()
            .set_default("auth_service_url", "http://127.0.0.1:50051")?
            .set_default("gateway_url", "http://127.0.0.1:8090")?
            .set_default("session_auth_retries", 3i64)?
            .set_default("session_auth_timeout_ms", 5000i64)?
            .set_default("input_hz", 30i64)?
            .set_default("input_redundancy", 3i64)?
            .set_default("username", "viewer")?
            .set_default("password", "viewer")?
            .set_default("start_x", 0.0)?
            .set_default("start_z", 0.0)?
            .set_default("view_range", 80.0)?
            .set_default("stale_entity_secs", 5.0)?
            .add_source(File::with_name("viewer").required(false))
            .add_source(Environment::with_prefix("VIEWER"))
            .build()?;
        cfg.try_deserialize()
    }
}
