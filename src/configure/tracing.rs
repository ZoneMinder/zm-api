#![allow(clippy::result_large_err)]
use std::path::PathBuf;

use tracing::{subscriber, Subscriber};
use tracing_appender::{
    non_blocking::{NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt, EnvFilter, Registry};

use crate::error::AppResult;

/// Get the log directory from environment or use default.
///
/// Reads from `APP_LOG_DIR` environment variable. If not set:
/// - Returns `/var/log/zm-api` if it exists (production)
/// - Otherwise returns `./logs` (development)
pub fn get_log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("APP_LOG_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    // Default to /var/log/zm-api for production if it exists
    let prod_path = PathBuf::from("/var/log/zm-api");
    if prod_path.exists() {
        return prod_path;
    }
    // Fall back to relative path for development
    PathBuf::from("logs")
}

fn create_subscriber<O, W>(
    name: &str,
    env_filter: EnvFilter,
    stdout: O,
    writer: W,
) -> impl Subscriber + Sync + Send
where
    O: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    Registry::default()
        .with(env_filter)
        .with(JsonStorageLayer)
        .with(BunyanFormattingLayer::new(name.into(), stdout))
        .with(BunyanFormattingLayer::new(name.into(), writer))
}

/// Keeps both background log writers alive; dropping it flushes them.
pub struct LogGuards {
    _stdout: WorkerGuard,
    _file: WorkerGuard,
}

pub fn init_subscriber<S>(subscriber: S) -> anyhow::Result<()>
where
    S: Subscriber + Send + Sync + 'static,
{
    LogTracer::init()?;
    subscriber::set_global_default(subscriber)?;
    Ok(())
}

/// webrtc-rs's mDNS client warns about every A/AAAA answer on the LAN that
/// has no A record, whether or not it answers one of our queries: about 100
/// lines a minute on a network with printers and TVs (#258). Its errors
/// still show. Naming `rtc_mdns` in `RUST_LOG` overrides this.
const QUIET_TARGETS: &[&str] = &["rtc_mdns=error"];

/// The filter for `RUST_LOG` (`info` when unset or unparsable), with the
/// quiet targets added unless `RUST_LOG` names them itself.
fn log_filter(rust_log: Option<&str>) -> EnvFilter {
    let spec = rust_log.filter(|s| !s.trim().is_empty()).unwrap_or("info");
    let mut filter = EnvFilter::try_new(spec).unwrap_or_else(|_| EnvFilter::new("info"));
    for quiet in QUIET_TARGETS {
        let target = quiet.split('=').next().unwrap_or_default();
        if !spec.contains(target) {
            if let Ok(directive) = quiet.parse() {
                filter = filter.add_directive(directive);
            }
        }
    }
    filter
}

pub fn init() -> AppResult<LogGuards> {
    let log_dir = get_log_dir();
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, "zm-api.log");
    let (file_appender, file_appender_guard) = tracing_appender::non_blocking(file_appender);
    // Stdout (the journal, under systemd) is written from a background thread
    // like the file, so a request never blocks on the write. Not lossy: when the
    // buffer is full the logging thread waits rather than dropping lines.
    let (stdout, stdout_guard) = NonBlockingBuilder::default()
        .lossy(false)
        .finish(std::io::stdout());
    init_subscriber(create_subscriber(
        "zm-api",
        log_filter(std::env::var(EnvFilter::DEFAULT_ENV).ok().as_deref()),
        stdout,
        file_appender,
    ))?;
    Ok(LogGuards {
        _stdout: stdout_guard,
        _file: file_appender_guard,
    })
}

#[cfg(test)]
mod log_filter_tests {
    use super::log_filter;

    #[test]
    fn mdns_warnings_are_quiet_by_default() {
        for env in [None, Some(""), Some("info"), Some("debug,hyper=warn")] {
            let f = log_filter(env).to_string();
            assert!(f.contains("rtc_mdns=error"), "{env:?} -> {f}");
        }
    }

    #[test]
    fn rust_log_naming_mdns_wins() {
        let f = log_filter(Some("info,rtc_mdns=trace")).to_string();
        assert!(f.contains("rtc_mdns=trace"), "{f}");
        assert!(!f.contains("rtc_mdns=error"), "{f}");
    }

    #[test]
    fn an_unparsable_rust_log_falls_back_to_info() {
        let f = log_filter(Some("info,=[bad")).to_string();
        assert!(f.contains("info"), "{f}");
        assert!(f.contains("rtc_mdns=error"), "{f}");
    }
}
