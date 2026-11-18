//! Service for a monitor's stored zm-next processing graph (the "free graph").
//!
//! The graph is the part of the pipeline ZoneMinder has no schema for; it is
//! validated here ([`crate::service::zmnext::graph`]), persisted to the
//! `monitor_pipeline` table, and composed with the monitor-derived capture/store
//! nodes at worker spawn (`service::zmnext::pipeline::compose_pipeline`). A
//! successful write best-effort restarts the worker so the change takes effect.

use serde_json::Value;

use crate::dto::response::monitor_pipeline::MonitorPipelineResponse;
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::service::zmnext::graph;

/// Fetch a monitor's stored processing graph, after verifying the caller can
/// access the monitor. 404 if the monitor is unknown/forbidden or has no graph.
pub async fn get(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
) -> AppResult<MonitorPipelineResponse> {
    // Enforce monitor row ACL + existence (404 for unknown/forbidden monitor).
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    let row = repo::monitor_pipeline::find_by_monitor(state.db(), monitor_id).await?;
    let row = match row {
        // A graph saved before secrets were split out: move them now, so the
        // response never carries a secret value.
        Some(row) => Some(
            crate::service::zmnext::secrets::migrate_row(
                state.db(),
                &state.config.zmnext.secrets.key_file,
                row,
            )
            .await?,
        ),
        None => None,
    };
    row.map(MonitorPipelineResponse::from).ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("monitor_id".into(), monitor_id.to_string())],
            resource_type: ResourceType::Monitor,
        })
    })
}

/// Validate and replace a monitor's processing graph, then best-effort restart
/// its worker. The graph document is `{ "plugins": [...] }`. Verifies the monitor
/// exists/accessible (404) before writing, so we never persist an orphan row.
pub async fn replace(
    state: &AppState,
    monitor_id: u32,
    graph_doc: Value,
    scope: &MonitorScope,
) -> AppResult<MonitorPipelineResponse> {
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    graph::validate_graph(&graph_doc).map_err(AppError::BadRequestError)?;
    if let Some((schemas, _)) = worker_schemas(state, monitor_id).await {
        let errors = crate::service::zmnext::control::validate_graph_with_schemas(
            &graph_doc,
            &schemas,
            crate::service::zmnext::control::SecretRule::LiteralOrReference,
        );
        if !errors.is_empty() {
            return Err(invalid_pipeline(
                "the graph doesn't match the worker's plugin schemas",
                errors,
            ));
        }
    }

    // Secret values go to the encrypted store; the saved graph holds only
    // `{"$secret": ...}` references.
    let previous = repo::monitor_pipeline::find_by_monitor(state.db(), monitor_id).await?;
    let mut graph_doc = graph_doc;
    let key_file = &state.config.zmnext.secrets.key_file;
    crate::service::zmnext::secrets::stash_secrets(
        state.db(),
        key_file,
        monitor_id,
        &mut graph_doc,
    )
    .await?;
    let body = serde_json::to_string(&graph_doc)?;
    let now = crate::util::time::utc_now();
    let row = repo::monitor_pipeline::upsert(state.db(), monitor_id, body, 1, now).await?;

    if let WorkerApply::Refused(errors) = apply_to_worker(state, monitor_id).await {
        // The worker kept its pipeline; put the stored graph back in step.
        restore_graph(state, monitor_id, previous, now).await?;
        return Err(invalid_pipeline(
            "the worker refused the graph and kept its current pipeline",
            errors,
        ));
    }
    crate::service::zmnext::secrets::prune_secrets(state.db(), monitor_id, Some(&graph_doc))
        .await?;
    Ok(MonitorPipelineResponse::from(row))
}

/// Remove a monitor's stored graph (reverts it to the default generated
/// pipeline), then best-effort restart its worker.
pub async fn delete(state: &AppState, monitor_id: u32, scope: &MonitorScope) -> AppResult<()> {
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    repo::monitor_pipeline::delete_by_monitor(state.db(), monitor_id).await?;
    crate::service::zmnext::secrets::prune_secrets(state.db(), monitor_id, None).await?;
    apply_or_log(state, monitor_id).await;
    Ok(())
}

/// "Make this monitor zm-next": set `UseZmNext=1` and, if it has no stored graph
/// yet, materialize the default processing graph so it behaves like the legacy
/// default until edited. Best-effort restarts the worker. Reversible via
/// [`disable_zmnext`]. Errors if the `UseZmNext` column is absent (the ZoneMinder
/// fork migration is required).
pub async fn enable_zmnext(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
) -> AppResult<MonitorPipelineResponse> {
    // Setting the flag with no runtime to honour it returned 200 and
    // restarted the legacy zmc for nothing (#121).
    if !state.config.zmnext.enabled || state.daemon_manager.is_none() {
        return Err(AppError::ServiceUnavailableError(
            "zm-next is not available on this server: [zmnext].enabled is off, \
             or daemon control is passive"
                .to_string(),
        ));
    }
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    // Don't switch a camera to a worker that can't run.
    if let Some(mgr) = &state.daemon_manager {
        if let Some((path, false)) = mgr.zmcore_installed() {
            return Err(AppError::ServiceUnavailableError(format!(
                "zm-next isn't installed on this server: {} not found \
                 (set [zmnext.worker].binary)",
                path.display()
            )));
        }
    }
    crate::repo::monitors::set_use_zmnext(state.db(), monitor_id, true)
        .await
        .map_err(|e| {
            AppError::BadRequestError(format!(
                "could not set UseZmNext for monitor {monitor_id} \
                 (the ZoneMinder fork migration adding this column may be required): {e}"
            ))
        })?;

    // Seed a default graph only if one doesn't already exist (re-enabling keeps
    // any previously configured graph).
    let row = match repo::monitor_pipeline::find_by_monitor(state.db(), monitor_id).await? {
        Some(existing) => existing,
        None => {
            let synopsis = state.config.synopsis.enabled
                && state.config.synopsis.enabled_monitors.contains(&monitor_id);
            let graph = crate::service::zmnext::pipeline::default_processing_graph(
                &state.config.zmnext.pipeline,
                synopsis,
            );
            let body = serde_json::to_string(&graph)?;
            let now = crate::util::time::utc_now();
            repo::monitor_pipeline::upsert(state.db(), monitor_id, body, 1, now).await?
        }
    };
    apply_or_log(state, monitor_id).await;
    Ok(MonitorPipelineResponse::from(row))
}

/// Revert a monitor from zm-next: clear `UseZmNext` so legacy zmc/zma resume. The
/// stored graph is kept (dormant) so re-enabling restores the configuration.
pub async fn disable_zmnext(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
) -> AppResult<()> {
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    crate::repo::monitors::set_use_zmnext(state.db(), monitor_id, false)
        .await
        .map_err(|e| {
            AppError::BadRequestError(format!(
                "could not clear UseZmNext for monitor {monitor_id}: {e}"
            ))
        })?;
    reload_worker(state, monitor_id).await;
    Ok(())
}

/// The monitor's zm-next worker state, including why supervision gave up on it.
pub async fn worker_status(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
) -> AppResult<crate::dto::response::monitor_pipeline::ZmNextWorkerStatusResponse> {
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    let use_zmnext = state.config.zmnext.enabled
        && crate::repo::monitors::use_zmnext(state.db(), monitor_id).await;
    let worker = match &state.daemon_manager {
        Some(mgr) => mgr.zmnext_worker_status(monitor_id).await.map(Into::into),
        None => None,
    };
    Ok(
        crate::dto::response::monitor_pipeline::ZmNextWorkerStatusResponse {
            monitor_id,
            use_zmnext,
            supervised: state.daemon_manager.is_some(),
            worker,
            live: state
                .source_router
                .as_ref()
                .and_then(|r| r.worker_status(monitor_id)),
        },
    )
}

/// Put back the graph a refused configure replaced (or remove the new one),
/// and drop secrets only the refused graph used.
async fn restore_graph(
    state: &AppState,
    monitor_id: u32,
    previous: Option<crate::entity::monitor_pipeline::Model>,
    now: jiff::civil::DateTime,
) -> AppResult<()> {
    match previous {
        Some(old) => {
            repo::monitor_pipeline::upsert(
                state.db(),
                monitor_id,
                old.graph_json.clone(),
                old.version,
                now,
            )
            .await?;
            let old_graph = serde_json::from_str::<Value>(&old.graph_json).ok();
            crate::service::zmnext::secrets::prune_secrets(
                state.db(),
                monitor_id,
                old_graph.as_ref(),
            )
            .await?;
        }
        None => {
            repo::monitor_pipeline::delete_by_monitor(state.db(), monitor_id).await?;
            crate::service::zmnext::secrets::prune_secrets(state.db(), monitor_id, None).await?;
        }
    }
    Ok(())
}

/// Check a graph without saving it, for the pipeline editor. Uses the
/// worker's plugin schemas when it offers them, otherwise zm-api's built-in
/// plugin list.
pub async fn validate(
    state: &AppState,
    monitor_id: u32,
    graph_doc: &Value,
    scope: &MonitorScope,
) -> AppResult<crate::dto::response::monitor_pipeline::PipelineValidationResponse> {
    use crate::dto::response::monitor_pipeline::PipelineValidationResponse;
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    if let Err(message) = graph::validate_graph(graph_doc) {
        return Ok(PipelineValidationResponse {
            valid: false,
            checked_against: "builtin".into(),
            errors: vec![crate::service::zmnext::control::PathError {
                path: String::new(),
                message,
            }],
        });
    }
    let Some((schemas, source)) = worker_schemas(state, monitor_id).await else {
        return Ok(PipelineValidationResponse {
            valid: true,
            checked_against: "builtin".into(),
            errors: vec![],
        });
    };
    let errors = crate::service::zmnext::control::validate_graph_with_schemas(
        graph_doc,
        &schemas,
        crate::service::zmnext::control::SecretRule::LiteralOrReference,
    );
    Ok(PipelineValidationResponse {
        valid: errors.is_empty(),
        checked_against: source,
        errors,
    })
}

fn invalid_pipeline(
    message: &str,
    errors: Vec<crate::service::zmnext::control::PathError>,
) -> AppError {
    AppError::InvalidPipelineError {
        message: message.to_string(),
        errors: errors.into_iter().map(|e| (e.path, e.message)).collect(),
    }
}

/// The monitor's worker hello, when it says the worker accepts control
/// commands from zm-api.
async fn control_hello(
    state: &AppState,
    monitor_id: u32,
) -> Option<crate::streaming::source::protocol::WorkerHello> {
    let router = state.source_router.as_ref()?;
    router
        .current_worker_hello(monitor_id, std::time::Duration::from_secs(2))
        .await
        .filter(|h| h.protocol.control >= 1 && h.control_peer)
}

/// The worker's plugin schemas and where they came from, or `None` when the
/// worker doesn't offer them (no hello, older zm-next, zmc).
async fn worker_schemas(
    state: &AppState,
    monitor_id: u32,
) -> Option<(std::collections::BTreeMap<String, Value>, String)> {
    let hello = control_hello(state, monitor_id).await?;
    let router = state.source_router.as_ref()?;
    match crate::service::zmnext::control::fetch_schemas(
        router,
        monitor_id,
        &hello,
        crate::service::zmnext::control::schema_cache(),
        std::time::Duration::from_secs(5),
    )
    .await
    {
        Ok(schemas) => Some((
            schemas,
            format!("zm-next {} schemas", hello.zm_next.version),
        )),
        Err(e) => {
            tracing::warn!("monitor {monitor_id}: couldn't fetch plugin schemas: {e}");
            None
        }
    }
}

/// How a graph change reached the worker.
enum WorkerApply {
    /// Applied in place with `configure`.
    Configured,
    /// Restarted, or picked up at its next start.
    Restarted,
    /// The worker checked the pipeline and refused it; nothing changed.
    Refused(Vec<crate::service::zmnext::control::PathError>),
}

/// Apply the stored graph to the monitor's worker: `configure` when its hello
/// says it can take one, otherwise a restart as before.
async fn apply_to_worker(state: &AppState, monitor_id: u32) -> WorkerApply {
    use crate::service::zmnext::control::{configure_command, configure_errors};
    use crate::streaming::source::command::CommandError;

    let (Some(mgr), Some(router), Some(_)) = (
        state.daemon_manager.as_ref(),
        state.source_router.as_ref(),
        control_hello(state, monitor_id).await,
    ) else {
        reload_worker(state, monitor_id).await;
        return WorkerApply::Restarted;
    };
    let (pipeline, secrets) = match mgr.zmnext_configure_pipeline(monitor_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            reload_worker(state, monitor_id).await;
            return WorkerApply::Restarted;
        }
        Err(e) => {
            tracing::warn!("monitor {monitor_id}: couldn't build configure ({e}); restarting");
            reload_worker(state, monitor_id).await;
            return WorkerApply::Restarted;
        }
    };
    let salt = match crate::service::zmnext::secrets::secrets_salt(
        &state.config.zmnext.secrets.key_file,
        monitor_id,
    ) {
        Ok(salt) => salt,
        Err(e) => {
            tracing::warn!("monitor {monitor_id}: no secrets salt ({e}); restarting instead");
            reload_worker(state, monitor_id).await;
            return WorkerApply::Restarted;
        }
    };
    let command = configure_command(&pipeline, &secrets, &salt, "restart");
    match router
        .send_control(monitor_id, command, std::time::Duration::from_secs(30))
        .await
    {
        Ok(data) => {
            tracing::info!(
                "monitor {monitor_id}: worker reconfigured in place (pipeline {})",
                data.get("pipeline_hash")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("hash not reported")
            );
            WorkerApply::Configured
        }
        // A worker that has the hello but not configure yet.
        Err(CommandError::Refused { message, .. }) if message.starts_with("unknown_command") => {
            reload_worker(state, monitor_id).await;
            WorkerApply::Restarted
        }
        Err(CommandError::Refused { message, data }) => {
            WorkerApply::Refused(configure_errors(&message, &data))
        }
        Err(e) => {
            tracing::warn!("monitor {monitor_id}: configure didn't complete ({e}); restarting");
            reload_worker(state, monitor_id).await;
            WorkerApply::Restarted
        }
    }
}

/// [`apply_to_worker`] where there is nothing to roll back to: a refusal is
/// logged, and the worker keeps its current pipeline.
async fn apply_or_log(state: &AppState, monitor_id: u32) {
    if let WorkerApply::Refused(errors) = apply_to_worker(state, monitor_id).await {
        tracing::error!("monitor {monitor_id}: worker refused its default pipeline: {errors:?}");
    }
}

/// Best-effort: restart the monitor's worker so a graph change is applied now.
/// Never fails the request — the change is already persisted and applies on the
/// next (re)start regardless.
async fn reload_worker(state: &AppState, monitor_id: u32) {
    if let Some(mgr) = &state.daemon_manager {
        match mgr.restart_monitor(monitor_id).await {
            Ok(resp) if !resp.success => tracing::warn!(
                "monitor {monitor_id} pipeline graph saved; worker restart refused \
                 (applies on next start): {}",
                resp.message
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                "monitor {monitor_id} pipeline graph saved; worker restart failed \
                 (applies on next start): {e}"
            ),
        }
    }
}
