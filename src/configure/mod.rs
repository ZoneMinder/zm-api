use std::str::FromStr;

use ::tracing::info;
use config::{ConfigError, Environment};
use serde::Deserialize;

use crate::util::dir::get_project_root;

use self::{
    daemon::DaemonConfig, db::DatabaseConfig, http::HttpClientConfig,
    maintenance::MaintenanceConfig, retention::RetentionConfig, search::SearchConfig,
    secret::SecretConfig, sentry::SentryConfig, server::ServerConfig, streaming::StreamingConfig,
    synopsis::SynopsisConfig, web::WebConfig, zmnext::ZmNextConfig,
};

pub mod cluster;
pub mod daemon;
pub mod db;
pub mod env;
pub mod http;
pub mod maintenance;
pub mod retention;
pub mod search;
pub mod secret;
pub mod sentry;
pub mod server;
pub mod streaming;
pub mod synopsis;
pub mod tracing;
pub mod web;
pub mod zmconf;
pub mod zmnext;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub profile: Profile,
    pub server: ServerConfig,
    pub db: DatabaseConfig,
    pub sentry: SentryConfig,
    pub secret: SecretConfig,
    pub http: HttpClientConfig,
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// zm-next worker integration (spawn + pipeline + ingest). Defaults to a
    /// disabled, zero-impact configuration when the `[zmnext]` block is absent.
    #[serde(default)]
    pub zmnext: ZmNextConfig,
    /// Motion-synopsis optimiser + renderer + serving. Defaults to disabled
    /// when the `[synopsis]` block is absent.
    #[serde(default)]
    pub synopsis: SynopsisConfig,
    /// Natural-language / semantic event search. Off by default.
    #[serde(default)]
    pub search: SearchConfig,
    /// Automatic recording-disk cleanup. Off by default; bounds per-storage
    /// usage by free-space floor / age / quota.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Serving the zm-web browser UI from this binary. Off by default.
    #[serde(default)]
    pub web: WebConfig,
    /// Native replacements for the Perl maintenance daemons (zmaudit, zmstats,
    /// zmtelemetry). Each independently switchable; all off by default.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    /// Multi-server installs: reaching the zm-api on the other servers.
    #[serde(default)]
    pub cluster: cluster::ClusterConfig,
}

impl AppConfig {
    /// Read application configuration from files and environment
    ///
    /// Configuration is loaded in this priority order (highest to lowest):
    /// 1. Environment variables (APP_DB__HOST, etc.)
    /// 2. Profile TOML file (settings/{profile}.toml)
    /// 3. Base TOML file (settings/base.toml)
    /// 4. ZoneMinder zm.conf (/etc/zm/zm.conf + /etc/zm/conf.d/*.conf)
    ///
    /// This allows zm-api to work out of the box on ZoneMinder installations.
    pub fn read(env_src: Environment) -> Result<Self, config::ConfigError> {
        let config_dir = get_settings_dir()?;
        let profile = std::env::var("APP_PROFILE")
            .map(|env| Profile::from_str(&env).map_err(|e| ConfigError::Message(e.to_string())))
            .unwrap_or_else(|_e| Ok(Profile::Dev))?;
        let profile_filename = format!("{profile}.toml");
        let config = config::Config::builder()
            .add_source(config::File::from(config_dir.join("base.toml")))
            .add_source(config::File::from(config_dir.join(profile_filename)))
            .add_source(env_src)
            .set_override("profile", profile.to_string())?
            .build()?;
        info!("Successfully read config profile: {profile}.");

        let mut app_config: Self = config.try_deserialize()?;

        // Apply ZoneMinder zm.conf as fallback for database config
        app_config.db.apply_zmconf_fallback();
        // ...and for where ZoneMinder keeps its sockets and binaries, which
        // its packaging decides differently on each distribution.
        zmconf::ZmConfig::load()
            .apply_paths(&mut app_config.daemon, &mut app_config.streaming.zoneminder);

        Ok(app_config)
    }
}

pub fn get_settings_dir() -> Result<std::path::PathBuf, ConfigError> {
    if let Ok(dir) = std::env::var("APP_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Ok(std::path::PathBuf::from(dir));
        }
    }
    Ok(get_project_root()
        .map_err(|e| ConfigError::Message(e.to_string()))?
        .join("settings"))
}

pub fn get_static_dir() -> Result<std::path::PathBuf, ConfigError> {
    if let Ok(dir) = std::env::var("APP_STATIC_DIR") {
        if !dir.trim().is_empty() {
            return Ok(std::path::PathBuf::from(dir));
        }
    }
    Ok(get_project_root()
        .map_err(|e| ConfigError::Message(e.to_string()))?
        .join("static"))
}

#[derive(
    Debug,
    strum::Display,
    strum::EnumString,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Clone,
    Copy,
)]
pub enum Profile {
    #[serde(rename = "test")]
    #[strum(serialize = "test")]
    Test,
    #[serde(rename = "test-db")]
    #[strum(serialize = "test-db")]
    TestDb,
    #[serde(rename = "dev")]
    #[strum(serialize = "dev")]
    Dev,
    #[serde(rename = "prod")]
    #[strum(serialize = "prod")]
    Prod,
}

#[cfg(test)]
mod tests {
    use self::env::get_env_source;

    pub use super::*;

    #[test]
    pub fn test_read_app_config() {
        let _config = AppConfig::read(get_env_source("TEST_APP")).unwrap();
    }

    #[test]
    pub fn test_profile_to_string() {
        let profile: Profile = Profile::try_from("dev").unwrap();
        assert_eq!(profile, Profile::Dev)
    }
}
