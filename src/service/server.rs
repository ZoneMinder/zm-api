use crate::constant::API_VERSION;
use crate::dto::response::{LocaleResponse, VersionResponse};
use crate::error::{AppError, AppResult};
use crate::repo;
use crate::server::state::AppState;
use tokio::process::Command;
use tracing::{error, info};

/// Resolve the server's effective locale: timezone (ZoneMinder `ZM_TIMEZONE`,
/// falling back to the host OS zone), current UTC offset, and the three
/// ZoneMinder date/time format patterns.
pub async fn get_locale(state: &AppState) -> AppResult<LocaleResponse> {
    let db = state.db();

    // ZM_TIMEZONE is often empty; fall back to the host OS IANA zone.
    let zm_tz = repo::config::get_config_value(db, "ZM_TIMEZONE")
        .await?
        .filter(|s| !s.trim().is_empty());
    let timezone = zm_tz.or_else(|| iana_time_zone::get_timezone().ok());

    // Current offset of the process's local zone.
    let now = jiff::Zoned::now();
    let utc_offset_seconds = now.offset().seconds();
    let utc_offset = now.strftime("%:z").to_string();

    Ok(LocaleResponse {
        timezone,
        utc_offset,
        utc_offset_seconds,
        date_format: repo::config::get_config_value(db, "ZM_DATE_FORMAT_PATTERN").await?,
        datetime_format: repo::config::get_config_value(db, "ZM_DATETIME_FORMAT_PATTERN").await?,
        time_format: repo::config::get_config_value(db, "ZM_TIME_FORMAT_PATTERN").await?,
    })
}

// API version (from Cargo.toml via constant)

pub async fn get_version(state: &AppState) -> AppResult<VersionResponse> {
    tracing::debug!("Getting ZoneMinder and API version information");

    // Fetch version information from the database
    let zm_version = repo::config::get_zm_version(state.db()).await?;
    let db_version = repo::config::get_zm_db_version(state.db()).await?;

    // Build version response
    let response = VersionResponse {
        version: zm_version,
        api_version: API_VERSION.to_string(),
        db_version,
    };

    Ok(response)
}

/// Restart ZoneMinder daemon
pub async fn restart_zoneminder(state: &AppState) -> AppResult<()> {
    control_zoneminder(state, "restart").await
}

/// Stop ZoneMinder daemon
pub async fn stop_zoneminder(state: &AppState) -> AppResult<()> {
    control_zoneminder(state, "stop").await
}

/// Start ZoneMinder daemon
pub async fn start_zoneminder(state: &AppState) -> AppResult<()> {
    control_zoneminder(state, "start").await
}

/// `systemctl <action> zoneminder`, falling back to `zmpkg.pl <action>` when
/// systemd is unavailable or refuses (containers, a service user without
/// systemd rights). zmpkg.pl is what ZoneMinder's own API runs for run-state
/// changes; the fallback used to be zmcontrol.pl, the PTZ script (#157).
async fn control_zoneminder(state: &AppState, action: &str) -> AppResult<()> {
    info!("{action} ZoneMinder");
    match Command::new("systemctl")
        .arg(action)
        .arg("zoneminder")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            info!("ZoneMinder {action} via systemctl succeeded");
            return Ok(());
        }
        Ok(output) => error!(
            "systemctl {action} zoneminder failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(e) => error!("Failed to execute systemctl: {e}"),
    }

    let zmpkg = zmpkg_path(&state.config.daemon.bin_path);
    let output = Command::new(&zmpkg)
        .arg(action)
        .output()
        .await
        .map_err(|e| {
            AppError::InternalServerError(format!("Failed to execute {}: {e}", zmpkg.display()))
        })?;
    if output.status.success() {
        info!("ZoneMinder {action} via zmpkg.pl succeeded");
        Ok(())
    } else {
        Err(AppError::InternalServerError(format!(
            "Failed to {action} ZoneMinder: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// zmpkg.pl in ZoneMinder's binary directory (`ZM_PATH_BIN`, applied to
/// `[daemon].bin_path` from zm.conf).
fn zmpkg_path(bin_path: &std::path::Path) -> std::path::PathBuf {
    bin_path.join("zmpkg.pl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_zmpkg_not_the_ptz_script() {
        assert_eq!(
            zmpkg_path(std::path::Path::new("/usr/bin")),
            std::path::Path::new("/usr/bin/zmpkg.pl")
        );
    }
    use crate::entity::config::Model as ConfigModel;
    use crate::repo::config::{ZM_DB_VERSION_KEY, ZM_VERSION_KEY};
    use sea_orm::{DatabaseBackend, MockDatabase};

    fn mk(name: &str, val: &str) -> ConfigModel {
        ConfigModel {
            id: 1,
            name: name.into(),
            value: val.into(),
            r#type: "string".into(),
            default_value: None,
            hint: None,
            pattern: None,
            format: None,
            prompt: None,
            help: None,
            category: "General".into(),
            readonly: 0,
            private: 0,
            system: 0,
            requires: None,
        }
    }

    #[tokio::test]
    async fn test_get_version_ok() {
        // Two queries: one for version and one for db_version
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ConfigModel, _, _>(vec![vec![mk(ZM_VERSION_KEY, "1.2.3")]])
            .append_query_results::<ConfigModel, _, _>(vec![vec![mk(ZM_DB_VERSION_KEY, "1.2.0")]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = get_version(&state).await.unwrap();
        assert_eq!(out.version, "1.2.3");
        assert_eq!(out.db_version, "1.2.0");
        assert!(!out.api_version.is_empty());
    }
}
