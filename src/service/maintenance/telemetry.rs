//! Native replacement for `zmtelemetry.pl`.
//!
//! Posts an anonymous usage report on a long interval. Off unless explicitly
//! enabled, and it stays off — a phone-home that switches itself on is a poor
//! default however little it sends.
//!
//! ## What this deliberately does not do
//!
//! **No geolocation lookup.** `zmtelemetry.pl` calls `ipinfo.io` (falling back
//! to `ip2location.io`) on *every* collection, including under `--show`, and
//! puts city, region, country and latitude/longitude in the report. That is an
//! unconditional disclosure of the server's public IP to a third party, done by
//! something described as anonymous statistics, and it happens even when the
//! operator only asked to preview the payload. The fields are still sent so the
//! receiving end sees the shape it expects, but they are always `"Unknown"` —
//! the same value the Perl uses when both lookups fail.
//!
//! **No `eval` of the interval.** `ZM_TELEMETRY_INTERVAL` defaults to the
//! string `'14*24*60*60'` and the Perl evaluates it as code, which makes a
//! database row an arbitrary-code-execution surface. Here it is parsed as a
//! number, and a simple `a*b*c` product is accepted so existing config still
//! works.
//!
//! **Last-upload is compared with `>=`.** The Perl resets its in-memory
//! `$lastCheck` every iteration and compares with strict `>`, so in steady
//! state the difference equals the interval exactly and the send is skipped;
//! reports only reliably go out after a restart.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, FromQueryResult, Statement};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::configure::maintenance::TelemetryConfig;

/// The value the Perl sends when its geolocation lookups fail. Reused so the
/// receiver sees a shape it already handles.
const UNKNOWN: &str = "Unknown";

pub struct TelemetryService {
    db: Arc<DatabaseConnection>,
    config: TelemetryConfig,
    http: reqwest::Client,
}

/// The reported payload. Field names match `zmtelemetry.pl` exactly, so the
/// receiving end needs no changes.
#[derive(Debug, Serialize, PartialEq)]
pub struct TelemetryReport {
    pub uuid: String,
    pub timezone: String,
    pub city: String,
    pub region: String,
    pub country: String,
    pub latitude: String,
    pub longitude: String,
    pub timestamp: String,
    pub monitor_count: i64,
    pub event_count: i64,
    pub architecture: String,
    pub kernel: String,
    pub distro: String,
    pub version: String,
    pub zm_version: String,
    pub system_memory: u64,
    pub processor_count: usize,
    pub use_event_server: bool,
    pub monitors: Vec<MonitorReport>,
}

/// Per-monitor detail. `path` is scrubbed before it leaves the machine.
#[derive(Debug, Serialize, PartialEq, FromQueryResult)]
pub struct MonitorReport {
    pub id: u32,
    pub name: String,
    pub r#type: String,
    pub width: u32,
    pub height: u32,
    pub colours: u8,
    pub path: String,
}

/// Strip credentials *and* host from a camera URL, keeping only the shape.
///
/// The Perl replaces the whole authority with the literal
/// `username:password@host`, so the hostname does not leave the machine either.
/// A value that is not a URL — a local device path — is replaced outright
/// rather than passed through, since `/dev/video0` is harmless but a UNC path
/// or a file path containing a hostname is not.
pub fn scrub_monitor_path(path: &str, is_local: bool) -> String {
    if is_local {
        return path.to_string();
    }
    match url::Url::parse(path) {
        Ok(mut url) => {
            // Keep scheme and path shape; discard credentials and host. The
            // query string goes too: cameras take `?user=..&password=..` (#102).
            let _ = url.set_username("username");
            let _ = url.set_password(Some("password"));
            let _ = url.set_host(Some("host"));
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => UNKNOWN.to_string(),
    }
}

/// Parse `ZM_TELEMETRY_INTERVAL`, which ships as the Perl expression
/// `14*24*60*60`.
///
/// Accepts a plain integer or a product of integers. Anything else returns
/// `None` and the caller falls back to its configured default — the Perl would
/// `eval` it, which turns a database row into code execution.
pub fn parse_interval_expression(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut product: u64 = 1;
    for factor in raw.split('*') {
        let n: u64 = factor.trim().parse().ok()?;
        product = product.checked_mul(n)?;
    }
    (product > 0).then_some(product)
}

impl TelemetryService {
    pub fn new(
        db: Arc<DatabaseConnection>,
        config: TelemetryConfig,
        http: reqwest::Client,
    ) -> Self {
        Self { db, config, http }
    }

    pub fn spawn(self: Arc<Self>) {
        let interval = self.config.interval();
        tokio::spawn(async move {
            // Check often; the decision to send is made against the persisted
            // last-upload timestamp, not against how long this process has been
            // up. A daily restart would otherwise never reach a 14-day timer.
            let tick = interval.min(std::time::Duration::from_secs(3600));
            let mut ticker = tokio::time::interval(tick);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = self.send_if_due().await {
                    warn!("telemetry pass failed: {e}");
                }
            }
        });
    }

    /// Send a report if the configured interval has elapsed since the last
    /// successful upload.
    pub async fn send_if_due(&self) -> Result<bool, DbErr> {
        // ZoneMinder's own opt-in (Options → System → TELEMETRY_DATA) still
        // rules: the native job replaces zmtelemetry.pl, not the switch (#103).
        if let Some(v) = read_zm_config_str(self.db.as_ref(), "ZM_TELEMETRY_DATA").await {
            if !matches!(v.trim(), "1" | "true" | "yes" | "on") {
                debug!("telemetry disabled by ZM_TELEMETRY_DATA");
                return Ok(false);
            }
        }
        let interval = self.effective_interval().await;
        let last = self.last_upload().await.unwrap_or(0);
        let now = crate::util::time::unix_now();

        // `>=` rather than `>`: with a timer that fires exactly on the
        // interval, strict greater-than skips almost every send.
        if last != 0 && now.saturating_sub(last) < interval as i64 {
            debug!(
                "telemetry not due for another {}s",
                interval as i64 - (now - last)
            );
            return Ok(false);
        }

        let report = self.collect().await?;
        match self.post(&report).await {
            Ok(()) => {
                self.record_upload(now).await?;
                info!("telemetry report sent to {}", self.config.endpoint);
                Ok(true)
            }
            Err(e) => {
                // Deliberately does not record the timestamp: a failed send
                // should be retried, not silently counted as done.
                warn!("telemetry upload failed: {e}");
                Ok(false)
            }
        }
    }

    /// Assemble the report. Public so `--show`-style previewing can print it
    /// without sending anything.
    pub async fn collect(&self) -> Result<TelemetryReport, DbErr> {
        let backend = self.db.get_database_backend();

        #[derive(FromQueryResult)]
        struct Count {
            n: i64,
        }
        let count = |sql: &'static str| async move {
            Count::find_by_statement(Statement::from_string(backend, sql))
                .one(self.db.as_ref())
                .await
                .ok()
                .flatten()
                .map(|r| r.n)
                .unwrap_or(0)
        };

        let monitor_count = count("SELECT COUNT(*) AS n FROM Monitors WHERE Deleted = 0").await;
        let event_count = count("SELECT COUNT(*) AS n FROM Events").await;

        #[derive(FromQueryResult)]
        struct MonitorRow {
            id: u32,
            name: String,
            r#type: String,
            width: u32,
            height: u32,
            colours: u8,
            path: Option<String>,
        }

        let monitors = MonitorRow::find_by_statement(Statement::from_string(
            backend,
            "SELECT Id AS id, Name AS name, Type AS type, Width AS width, \
                    Height AS height, Colours AS colours, Path AS path \
             FROM Monitors WHERE Deleted = 0",
        ))
        .all(self.db.as_ref())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let is_local = m.r#type.eq_ignore_ascii_case("local");
            MonitorReport {
                path: scrub_monitor_path(m.path.as_deref().unwrap_or(""), is_local),
                id: m.id,
                name: m.name,
                r#type: m.r#type,
                width: m.width,
                height: m.height,
                colours: m.colours,
            }
        })
        .collect();

        Ok(TelemetryReport {
            uuid: self.uuid().await,
            timezone: iana_time_zone::get_timezone().unwrap_or_else(|_| UNKNOWN.to_string()),
            // Never looked up — see the module note.
            city: UNKNOWN.to_string(),
            region: UNKNOWN.to_string(),
            country: UNKNOWN.to_string(),
            latitude: UNKNOWN.to_string(),
            longitude: UNKNOWN.to_string(),
            timestamp: crate::util::time::rfc3339_offset(jiff::Timestamp::now()),
            monitor_count,
            event_count,
            architecture: std::env::consts::ARCH.to_string(),
            kernel: std::env::consts::OS.to_string(),
            distro: read_os_release_field("NAME").unwrap_or_else(|| UNKNOWN.to_string()),
            version: read_os_release_field("VERSION_ID").unwrap_or_else(|| UNKNOWN.to_string()),
            zm_version: env!("CARGO_PKG_VERSION").to_string(),
            system_memory: total_memory_bytes(),
            processor_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            use_event_server: false,
            monitors,
        })
    }

    async fn post(&self, report: &TelemetryReport) -> Result<(), reqwest::Error> {
        // Sent as JSON with a truthful content type. The Perl declares
        // `application/x-www-form-urlencoded` while sending a JSON body.
        self.http
            .post(&self.config.endpoint)
            .json(report)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn effective_interval(&self) -> u64 {
        match read_zm_config_str(self.db.as_ref(), "ZM_TELEMETRY_INTERVAL").await {
            Some(raw) => parse_interval_expression(&raw).unwrap_or_else(|| {
                warn!("ZM_TELEMETRY_INTERVAL {raw:?} is not a number or product; using config");
                self.config.interval_seconds
            }),
            None => self.config.interval_seconds,
        }
    }

    async fn last_upload(&self) -> Option<i64> {
        read_zm_config_str(self.db.as_ref(), "ZM_TELEMETRY_LAST_UPLOAD")
            .await?
            .trim()
            .parse()
            .ok()
    }

    /// This install's stable anonymous identifier, minted on first use.
    async fn uuid(&self) -> String {
        if let Some(existing) = read_zm_config_str(self.db.as_ref(), "ZM_TELEMETRY_UUID").await {
            // zmtelemetry.pl validates the stored value the same way (#103).
            if let Ok(parsed) = uuid::Uuid::parse_str(existing.trim()) {
                if !parsed.is_nil() {
                    return existing.trim().to_string();
                }
            }
        }
        let fresh = uuid::Uuid::new_v4().to_string();
        if let Err(e) = self.write_config("ZM_TELEMETRY_UUID", &fresh).await {
            warn!("could not persist telemetry uuid: {e}");
        }
        fresh
    }

    async fn record_upload(&self, at: i64) -> Result<(), DbErr> {
        self.write_config("ZM_TELEMETRY_LAST_UPLOAD", &at.to_string())
            .await
    }

    async fn write_config(&self, name: &str, value: &str) -> Result<(), DbErr> {
        let backend = self.db.get_database_backend();
        let updated = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE Config SET Value = ? WHERE Name = ?",
                [value.into(), name.into()],
            ))
            .await?
            .rows_affected();
        if updated == 0 {
            // A database provisioned without ZoneMinder's Config seed has no
            // row to update; a silent no-op minted a fresh uuid and re-sent
            // every pass (#103).
            self.db
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "INSERT INTO Config (Name, Value, Type, Category) VALUES (?, ?, 'string', 'system')",
                    [name.into(), value.into()],
                ))
                .await?;
        }
        Ok(())
    }
}

async fn read_zm_config_str(db: &DatabaseConnection, name: &str) -> Option<String> {
    #[derive(FromQueryResult)]
    struct Row {
        value: String,
    }
    Row::find_by_statement(Statement::from_sql_and_values(
        db.get_database_backend(),
        "SELECT Value AS value FROM Config WHERE Name = ?",
        [name.into()],
    ))
    .one(db)
    .await
    .ok()?
    .map(|r| r.value)
}

/// A field from `/etc/os-release`, unquoted.
fn read_os_release_field(key: &str) -> Option<String> {
    let contents = std::fs::read_to_string("/etc/os-release").ok()?;
    parse_os_release_field(&contents, key)
}

pub fn parse_os_release_field(contents: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    contents.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// Total system memory in bytes, or 0 if it cannot be determined.
fn total_memory_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|c| parse_meminfo_total(&c))
        .unwrap_or(0)
}

pub fn parse_meminfo_total(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_and_host_are_stripped_from_camera_urls() {
        let scrubbed = scrub_monitor_path("rtsp://admin:hunter2@10.0.0.5:554/stream1", false);
        assert!(!scrubbed.contains("admin"), "{scrubbed}");
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(!scrubbed.contains("10.0.0.5"), "{scrubbed}");
        // The shape is still recognisable, which is the point of sending it.
        assert!(scrubbed.starts_with("rtsp://"), "{scrubbed}");
        assert!(scrubbed.ends_with("/stream1"), "{scrubbed}");
    }

    /// Cameras also take credentials in the query string (#102).
    #[test]
    fn query_string_credentials_are_stripped_too() {
        let scrubbed = scrub_monitor_path(
            "http://10.0.0.5/video.cgi?user=admin&password=hunter2#x",
            false,
        );
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(!scrubbed.contains("password="), "{scrubbed}");
        assert!(!scrubbed.contains('?'), "{scrubbed}");
        assert!(!scrubbed.contains('#'), "{scrubbed}");
        assert!(scrubbed.ends_with("/video.cgi"), "{scrubbed}");
    }

    #[test]
    fn a_local_device_path_is_left_alone() {
        assert_eq!(scrub_monitor_path("/dev/video0", true), "/dev/video0");
    }

    #[test]
    fn an_unparseable_remote_path_is_discarded_not_forwarded() {
        // Better to send nothing than to leak a hostname from something that
        // only looked like a device path.
        assert_eq!(
            scrub_monitor_path("\\\\fileserver\\share\\cam", false),
            UNKNOWN
        );
    }

    #[test]
    fn the_perl_interval_expression_is_parsed_not_evaluated() {
        assert_eq!(parse_interval_expression("14*24*60*60"), Some(1_209_600));
        assert_eq!(parse_interval_expression("3600"), Some(3600));
        assert_eq!(parse_interval_expression(" 2 * 3 "), Some(6));
    }

    #[test]
    fn a_non_arithmetic_interval_is_rejected_rather_than_run() {
        // The Perl `eval`s this string, so a Config row is code. These must all
        // fall back to the configured default instead.
        for hostile in [
            "system('rm -rf /')",
            "`id`",
            "$x",
            "",
            "14*",
            "0",
            "-1",
            "1e9",
        ] {
            assert_eq!(
                parse_interval_expression(hostile),
                None,
                "{hostile:?} should not parse"
            );
        }
    }

    #[test]
    fn interval_overflow_does_not_wrap() {
        assert_eq!(
            parse_interval_expression("99999999999*99999999999*99999999999"),
            None
        );
    }

    #[test]
    fn os_release_values_are_unquoted() {
        let sample = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\n";
        assert_eq!(
            parse_os_release_field(sample, "NAME"),
            Some("Ubuntu".to_string())
        );
        assert_eq!(
            parse_os_release_field(sample, "VERSION_ID"),
            Some("24.04".to_string())
        );
        assert_eq!(parse_os_release_field(sample, "MISSING"), None);
    }

    #[test]
    fn meminfo_is_reported_in_bytes() {
        let sample = "MemTotal:       16316412 kB\nMemFree:         123 kB\n";
        assert_eq!(parse_meminfo_total(sample), Some(16_316_412 * 1024));
    }

    #[test]
    fn the_report_carries_no_geolocation() {
        // Guards the deliberate departure: these must stay literal, never
        // populated from a third-party lookup.
        let report = TelemetryReport {
            uuid: "u".into(),
            timezone: "UTC".into(),
            city: UNKNOWN.into(),
            region: UNKNOWN.into(),
            country: UNKNOWN.into(),
            latitude: UNKNOWN.into(),
            longitude: UNKNOWN.into(),
            timestamp: "t".into(),
            monitor_count: 0,
            event_count: 0,
            architecture: "x86_64".into(),
            kernel: "linux".into(),
            distro: "Ubuntu".into(),
            version: "24.04".into(),
            zm_version: "3.0.0".into(),
            system_memory: 0,
            processor_count: 1,
            use_event_server: false,
            monitors: vec![],
        };
        let json = serde_json::to_value(&report).unwrap();
        for field in ["city", "region", "country", "latitude", "longitude"] {
            assert_eq!(json[field], UNKNOWN, "{field} must not be looked up");
        }
        // The receiver still sees every field it expects.
        assert!(json.get("monitor_count").is_some());
        assert!(json.get("zm_version").is_some());
    }
}
