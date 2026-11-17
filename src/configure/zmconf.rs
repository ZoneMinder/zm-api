//! ZoneMinder configuration file parser
//!
//! Parses ZoneMinder's zm.conf format (KEY=VALUE) with support for
//! the conf.d override directory pattern.
//!
//! Configuration is loaded from:
//! 1. `/etc/zm/zm.conf` (base configuration)
//! 2. `/etc/zm/conf.d/*.conf` (overrides, loaded alphabetically)
//!
//! Later files override earlier values.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, warn};

use super::streaming::ZoneMinderConfig;
use crate::daemon::config::DaemonConfig;

/// Parsed ZoneMinder configuration
#[derive(Debug, Clone, Default)]
pub struct ZmConfig {
    values: HashMap<String, String>,
    /// The file that last set each key (a later conf.d file wins).
    sources: HashMap<String, std::path::PathBuf>,
    /// The file being read, for `sources`.
    current: Option<std::path::PathBuf>,
}

impl ZmConfig {
    /// Load ZoneMinder configuration from the standard paths
    ///
    /// Loads `/etc/zm/zm.conf` and then all files in `/etc/zm/conf.d/`
    /// with a `.conf` extension, in alphabetical order.
    pub fn load() -> Self {
        let dir = std::env::var("ZM_CONF_DIR").unwrap_or_else(|_| "/etc/zm".to_string());
        Self::load_from_path(&dir)
    }

    /// Load ZoneMinder configuration from a custom base path
    ///
    /// Useful for testing or non-standard installations.
    pub fn load_from_path(base_path: &str) -> Self {
        let mut config = Self::default();

        let base = Path::new(base_path);

        // Load main zm.conf
        let main_conf = base.join("zm.conf");
        if main_conf.exists() {
            debug!("Loading ZoneMinder config from {:?}", main_conf);
            config.load_file(&main_conf);
        } else {
            debug!("ZoneMinder config not found at {:?}", main_conf);
        }

        // Load conf.d overrides
        let conf_d = base.join("conf.d");
        if conf_d.is_dir() {
            config.load_conf_d(&conf_d);
        }

        config
    }

    /// Load a single configuration file
    fn load_file(&mut self, path: &Path) {
        self.current = Some(path.to_path_buf());
        match fs::read_to_string(path) {
            Ok(contents) => {
                for line in contents.lines() {
                    self.parse_line(line);
                }
                debug!("Loaded {} values from {:?}", self.values.len(), path);
            }
            Err(e) => {
                warn!("Failed to read {:?}: {}", path, e);
            }
        }
    }

    /// Load all .conf files from a conf.d directory
    fn load_conf_d(&mut self, conf_d: &Path) {
        let mut conf_files: Vec<_> = match fs::read_dir(conf_d) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|ext| ext == "conf").unwrap_or(false))
                .collect(),
            Err(e) => {
                debug!("Could not read conf.d directory {:?}: {}", conf_d, e);
                return;
            }
        };

        // Sort alphabetically for deterministic override order
        conf_files.sort();

        for conf_file in conf_files {
            debug!("Loading override config from {:?}", conf_file);
            self.load_file(&conf_file);
        }
    }

    /// Parse a single line from a config file
    fn parse_line(&mut self, line: &str) {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            return;
        }

        // Parse KEY=VALUE
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            // Quotes around a value aren't part of it: ZoneMinder's C++, Perl
            // and PHP readers all strip them (#192).
            let value = value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();

            if !key.is_empty() {
                if let Some(file) = &self.current {
                    self.sources.insert(key.clone(), file.clone());
                }
                self.values.insert(key, value);
            }
        }
    }

    /// The file (zm.conf or a conf.d file) that sets `key`, if any.
    pub fn source_of(&self, key: &str) -> Option<&Path> {
        self.sources.get(key).map(|p| p.as_path())
    }

    /// Get a configuration value
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    /// Get database host (ZM_DB_HOST)
    ///
    /// Handles the format "hostname[:port]" or "localhost:/path/to/socket"
    /// Returns just the hostname part.
    pub fn db_host(&self) -> Option<&str> {
        self.get("ZM_DB_HOST").map(|host| {
            // Handle socket path: localhost:/path/to/socket
            if let Some(idx) = host.find(":/") {
                &host[..idx]
            }
            // Handle port: hostname:port
            else if let Some(idx) = host.rfind(':') {
                // Only split if it looks like a port (numeric after colon)
                let after_colon = &host[idx + 1..];
                if after_colon.chars().all(|c| c.is_ascii_digit()) {
                    &host[..idx]
                } else {
                    host
                }
            } else {
                host
            }
        })
    }

    /// Get the Unix socket path from ZM_DB_HOST ("localhost:/path/to/socket")
    ///
    /// Returns None for "hostname[:port]".
    pub fn db_socket(&self) -> Option<&str> {
        self.get("ZM_DB_HOST")
            .and_then(|host| host.find(":/").map(|idx| &host[idx + 1..]))
    }

    /// Get database port from ZM_DB_HOST if specified
    ///
    /// Returns None if no port is specified (use MySQL default 3306)
    pub fn db_port(&self) -> Option<u16> {
        self.get("ZM_DB_HOST").and_then(|host| {
            // Skip socket paths
            if host.contains(":/") {
                return None;
            }
            // Extract port from hostname:port
            if let Some(idx) = host.rfind(':') {
                let port_str = &host[idx + 1..];
                port_str.parse().ok()
            } else {
                None
            }
        })
    }

    /// Get database name (ZM_DB_NAME)
    pub fn db_name(&self) -> Option<&str> {
        self.get("ZM_DB_NAME")
    }

    /// Get database user (ZM_DB_USER)
    pub fn db_user(&self) -> Option<&str> {
        self.get("ZM_DB_USER")
    }

    /// Get database password (ZM_DB_PASS)
    pub fn db_pass(&self) -> Option<&str> {
        self.get("ZM_DB_PASS")
    }

    /// Check if any database configuration is available
    pub fn has_db_config(&self) -> bool {
        self.db_host().is_some()
            || self.db_name().is_some()
            || self.db_user().is_some()
            || self.db_pass().is_some()
    }

    /// A path setting, ignoring one that is present but empty.
    fn path(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }

    /// Follow zm.conf for the paths ZoneMinder's packaging decides.
    ///
    /// Where the sockets and binaries live differs by distribution: Debian
    /// and Ubuntu use `/run/zm` and `/usr/bin`, the Red Hat family keeps its
    /// sockets in `/var/lib/zoneminder/sock`. zm.conf is where zmc and
    /// zmdc.pl read those from, so it is where the stream sockets and
    /// `zmdc.sock` clients actually are.
    ///
    /// Only a value still at its built-in default is replaced. Anything else
    /// was set in TOML or the environment and is kept.
    pub fn apply_paths(&self, daemon: &mut DaemonConfig, streaming: &mut ZoneMinderConfig) {
        let daemon_defaults = DaemonConfig::default();
        if let Some(socks) = self.path("ZM_PATH_SOCKS") {
            if daemon.socket_path == daemon_defaults.socket_path {
                daemon.socket_path = socks.into();
            }
            if streaming.socks_path == ZoneMinderConfig::default().socks_path {
                streaming.socks_path = socks.to_string();
            }
        }
        if let Some(bin) = self.path("ZM_PATH_BIN") {
            if daemon.bin_path == daemon_defaults.bin_path {
                daemon.bin_path = bin.into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_config(dir: &Path, filename: &str, content: &str) {
        let path = dir.join(filename);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_parse_simple_config() {
        let tmp = TempDir::new().unwrap();
        create_test_config(
            tmp.path(),
            "zm.conf",
            r#"
# Comment line
ZM_DB_HOST=localhost
ZM_DB_NAME=zm
ZM_DB_USER=zmuser
ZM_DB_PASS=zmpass
"#,
        );

        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        assert_eq!(config.db_host(), Some("localhost"));
        assert_eq!(config.db_name(), Some("zm"));
        assert_eq!(config.db_user(), Some("zmuser"));
        assert_eq!(config.db_pass(), Some("zmpass"));
        assert_eq!(config.db_port(), None);
    }

    #[test]
    fn quotes_around_a_value_are_not_part_of_it() {
        let tmp = TempDir::new().unwrap();
        create_test_config(
            tmp.path(),
            "zm.conf",
            "ZM_PATH_FFMPEG=\"/usr/bin/ffmpeg\"\nZM_DB_PASS='zm pass'\nZM_DB_USER= \"zmuser\" \nZM_DB_NAME=zm\n",
        );
        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());
        assert_eq!(config.get("ZM_PATH_FFMPEG"), Some("/usr/bin/ffmpeg"));
        assert_eq!(config.db_pass(), Some("zm pass"));
        assert_eq!(config.db_user(), Some("zmuser"));
        assert_eq!(config.db_name(), Some("zm"));
    }

    #[test]
    fn test_parse_host_with_port() {
        let tmp = TempDir::new().unwrap();
        create_test_config(tmp.path(), "zm.conf", "ZM_DB_HOST=dbserver:3307\n");

        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        assert_eq!(config.db_host(), Some("dbserver"));
        assert_eq!(config.db_port(), Some(3307));
    }

    #[test]
    fn test_parse_host_with_socket() {
        let tmp = TempDir::new().unwrap();
        create_test_config(
            tmp.path(),
            "zm.conf",
            "ZM_DB_HOST=localhost:/var/run/mysqld/mysqld.sock\n",
        );

        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        assert_eq!(config.db_host(), Some("localhost"));
        assert_eq!(config.db_port(), None);
        assert_eq!(config.db_socket(), Some("/var/run/mysqld/mysqld.sock"));
    }

    #[test]
    fn records_which_file_sets_each_key() {
        let tmp = TempDir::new().unwrap();
        create_test_config(
            tmp.path(),
            "zm.conf",
            "ZM_DB_HOST=db\nZM_PATH_SOCKS=/run/zm\n",
        );
        let conf_d = tmp.path().join("conf.d");
        fs::create_dir(&conf_d).unwrap();
        create_test_config(&conf_d, "02-local.conf", "ZM_PATH_SOCKS=/var/run/zm\n");
        let c = ZmConfig::load_from_path(tmp.path().to_str().unwrap());
        assert_eq!(
            c.source_of("ZM_DB_HOST"),
            Some(tmp.path().join("zm.conf").as_path())
        );
        // A later conf.d file wins, and is the one named.
        assert_eq!(
            c.source_of("ZM_PATH_SOCKS"),
            Some(conf_d.join("02-local.conf").as_path())
        );
        assert_eq!(c.get("ZM_PATH_SOCKS"), Some("/var/run/zm"));
        assert_eq!(c.source_of("ZM_NOT_SET"), None);
    }

    #[test]
    fn test_conf_d_override() {
        let tmp = TempDir::new().unwrap();

        // Create main config
        create_test_config(
            tmp.path(),
            "zm.conf",
            r#"
ZM_DB_HOST=localhost
ZM_DB_NAME=zm
ZM_DB_USER=zmuser
ZM_DB_PASS=zmpass
"#,
        );

        // Create conf.d directory
        let conf_d = tmp.path().join("conf.d");
        fs::create_dir(&conf_d).unwrap();

        // Create override config
        create_test_config(
            &conf_d,
            "01-db.conf",
            r#"
ZM_DB_HOST=dbserver
ZM_DB_PASS=secretpass
"#,
        );

        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        // Overridden values
        assert_eq!(config.db_host(), Some("dbserver"));
        assert_eq!(config.db_pass(), Some("secretpass"));
        // Original values kept
        assert_eq!(config.db_name(), Some("zm"));
        assert_eq!(config.db_user(), Some("zmuser"));
    }

    #[test]
    fn test_conf_d_alphabetical_order() {
        let tmp = TempDir::new().unwrap();

        create_test_config(tmp.path(), "zm.conf", "ZM_DB_PASS=original\n");

        let conf_d = tmp.path().join("conf.d");
        fs::create_dir(&conf_d).unwrap();

        // Create files that would sort differently if not alphabetical
        create_test_config(&conf_d, "99-last.conf", "ZM_DB_PASS=last\n");
        create_test_config(&conf_d, "01-first.conf", "ZM_DB_PASS=first\n");

        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        // 99-last.conf should override 01-first.conf
        assert_eq!(config.db_pass(), Some("last"));
    }

    #[test]
    fn test_empty_config() {
        let tmp = TempDir::new().unwrap();
        let config = ZmConfig::load_from_path(tmp.path().to_str().unwrap());

        assert!(!config.has_db_config());
        assert_eq!(config.db_host(), None);
    }

    fn paths_from(content: &str) -> (DaemonConfig, ZoneMinderConfig, ZmConfig) {
        let tmp = TempDir::new().unwrap();
        create_test_config(tmp.path(), "zm.conf", content);
        let zmconf = ZmConfig::load_from_path(tmp.path().to_str().unwrap());
        (DaemonConfig::default(), ZoneMinderConfig::default(), zmconf)
    }

    /// The Red Hat family keeps ZoneMinder's sockets outside /run/zm. Left at
    /// the Debian default, live streaming and zmdc.sock both look in a
    /// directory that does not exist there.
    #[test]
    fn apply_paths_follows_zm_conf_when_settings_are_at_their_defaults() {
        let (mut daemon, mut streaming, zmconf) =
            paths_from("ZM_PATH_SOCKS=/var/lib/zoneminder/sock\nZM_PATH_BIN=/usr/local/bin\n");

        zmconf.apply_paths(&mut daemon, &mut streaming);

        assert_eq!(daemon.socket_path, Path::new("/var/lib/zoneminder/sock"));
        assert_eq!(streaming.socks_path, "/var/lib/zoneminder/sock");
        assert_eq!(daemon.bin_path, Path::new("/usr/local/bin"));
    }

    #[test]
    fn apply_paths_keeps_explicit_settings() {
        let (mut daemon, mut streaming, zmconf) =
            paths_from("ZM_PATH_SOCKS=/var/lib/zoneminder/sock\nZM_PATH_BIN=/opt/zm/bin\n");
        daemon.socket_path = "/srv/zm/sock".into();
        daemon.bin_path = "/srv/zm/bin".into();
        streaming.socks_path = "/srv/zm/sock".to_string();

        zmconf.apply_paths(&mut daemon, &mut streaming);

        assert_eq!(daemon.socket_path, Path::new("/srv/zm/sock"));
        assert_eq!(daemon.bin_path, Path::new("/srv/zm/bin"));
        assert_eq!(streaming.socks_path, "/srv/zm/sock");
    }

    #[test]
    fn apply_paths_ignores_missing_and_empty_values() {
        let (mut daemon, mut streaming, zmconf) = paths_from("ZM_PATH_SOCKS=\n");

        zmconf.apply_paths(&mut daemon, &mut streaming);

        assert_eq!(daemon.socket_path, Path::new("/run/zm"));
        assert_eq!(streaming.socks_path, "/run/zm");
        assert_eq!(daemon.bin_path, Path::new("/usr/bin"));
    }
}
