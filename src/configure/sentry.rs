use sentry::ClientInitGuard;
use serde::Deserialize;

#[derive(Deserialize, Clone)]
pub struct SentryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    dsn: String,
}

// Manual Debug: a DSN holds the project's ingest key.
impl std::fmt::Debug for SentryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentryConfig")
            .field("enabled", &self.enabled)
            .field("dsn", &"[REDACTED]")
            .finish()
    }
}

/// Initialize Sentry error reporting if enabled and DSN is configured.
/// Returns None if Sentry is disabled or DSN is empty.
pub fn init(config: &SentryConfig) -> Option<ClientInitGuard> {
    if !config.enabled || config.dsn.is_empty() {
        tracing::info!("Sentry error reporting is disabled");
        return None;
    }

    tracing::info!("Initializing Sentry error reporting");
    // ClientOptions is non-exhaustive since sentry 0.49, so it cannot be
    // built with struct syntax.
    let mut options = sentry::ClientOptions::default();
    options.release = sentry::release_name!();
    Some(sentry::init((config.dsn.clone(), options)))
}
