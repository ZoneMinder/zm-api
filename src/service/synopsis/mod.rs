//! Motion-synopsis optimiser + renderer + serving (zm-api side).
//!
//! zm-next emits the ingredients (object tubes of pre-rendered cutout JPEGs plus
//! background plates) via the `review_assets` (0x0306) EVENT; the ingest layer
//! records a [`crate::entity::event_synopsis`] row. This service turns that row
//! into glanceable stills (P1) and condensed mp4 clips (P3), caches them, and
//! serves them under ACL.
//!
//! **Anti-recompute:** rendering only composites the referenced JPEGs — it never
//! re-decodes the clip or re-runs detection (see [`compositor`]).

pub mod compositor;
pub mod optimiser;
pub mod render;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use sea_orm::DatabaseConnection;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::configure::synopsis::SynopsisConfig;
use crate::entity::event_synopsis;
use crate::entity::sea_orm_active_enums::SynopsisStatus;
use crate::repo;
use crate::service::zmnext::detail::TubeManifest;

/// Errors surfaced by the synopsis service. The HTTP layer maps these onto
/// `AppError` (see `handlers::synopsis`).
#[derive(Debug, thiserror::Error)]
pub enum SynopsisError {
    /// The feature is disabled in configuration.
    #[error("motion synopsis is disabled")]
    Disabled,
    /// No synopsis row exists for the requested event/range.
    #[error("no synopsis for the requested event")]
    NotFound,
    /// The stored manifest is unusable (e.g. missing source dimensions).
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    /// Nothing renderable remained after pruned/missing assets were skipped.
    #[error("no renderable assets")]
    NoAssets,
    /// Compositing/encoding failed.
    #[error("render failed: {0}")]
    RenderFailed(String),
    /// The configured encoder backend is unavailable/unsupported.
    #[error("encoder unavailable: {0}")]
    EncoderUnavailable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Db(#[from] sea_orm::error::DbErr),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

/// What a `GET …/synopsis` poll reports to the client.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct SynopsisStatusView {
    /// `pending` | `generating` | `ready` | `failed`.
    pub status: crate::entity::sea_orm_active_enums::SynopsisStatus,
    /// Populated once `ready`: the mp4 endpoint to fetch.
    pub url: Option<String>,
    pub tube_count: u32,
    #[schema(value_type = Option<String>, format = "date-time", example = "2026-07-02T12:34:56")]
    pub expires_at: Option<chrono::NaiveDateTime>,
}

/// Renders, caches and serves motion synopses. Mirrors `SnapshotService`: holds
/// the db handle, config, and an in-memory cache of rendered artifacts.
pub struct SynopsisService {
    db: Arc<DatabaseConnection>,
    config: SynopsisConfig,
    /// Rendered still bytes keyed by synopsis row id (cheap, idempotent).
    still_cache: DashMap<u64, Arc<Vec<u8>>>,
    /// Caps concurrent mp4 renders (`max_concurrent_renders`); excess queue.
    render_slots: Arc<Semaphore>,
    /// Synopsis row ids with a render task in flight (dedupes spawns).
    in_flight: Arc<DashMap<u64, ()>>,
}

impl SynopsisService {
    pub fn new(db: Arc<DatabaseConnection>, config: SynopsisConfig) -> Self {
        let permits = config.max_concurrent_renders.max(1);
        Self {
            db,
            config,
            still_cache: DashMap::new(),
            render_slots: Arc::new(Semaphore::new(permits)),
            in_flight: Arc::new(DashMap::new()),
        }
    }

    pub fn config(&self) -> &SynopsisConfig {
        &self.config
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Render (or serve the cached) P1 composite still for an event's synopsis.
    ///
    /// Looks up the synopsis row, parses its stored manifest, and composites one
    /// representative cutout per tube over the time-central plate. CPU/IO-bound
    /// work runs on a blocking thread. Degrades over missing assets; only fails
    /// when there is no row or the manifest is unusable.
    pub async fn still_for_event(&self, event_id: u64) -> Result<Arc<Vec<u8>>, SynopsisError> {
        let row = repo::event_synopsis::find_by_event_id(&self.db, event_id)
            .await?
            .ok_or(SynopsisError::NotFound)?;
        self.still_for_row(row).await
    }

    /// Render (or serve cached) the still for a specific synopsis row.
    pub async fn still_for_row(
        &self,
        row: crate::entity::event_synopsis::Model,
    ) -> Result<Arc<Vec<u8>>, SynopsisError> {
        if let Some(hit) = self.still_cache.get(&row.id) {
            return Ok(hit.clone());
        }

        let manifest = TubeManifest::parse(&row.manifest_json)
            .map_err(|e| SynopsisError::InvalidManifest(e.to_string()))?;
        let asset_dir = row.asset_dir.clone();
        let feather = self.config.mask_feather_px;

        let bytes = tokio::task::spawn_blocking(move || {
            compositor::render_still(&manifest, Path::new(&asset_dir), feather)
        })
        .await??;

        let arc = Arc::new(bytes);
        self.still_cache.insert(row.id, arc.clone());
        Ok(arc)
    }

    /// Compute the P2 temporal layout (per-tube time-shifts) for an event's
    /// synopsis. `class_filter`, when non-empty, keeps only tubes whose
    /// `class_id` is listed (e.g. a people-only synopsis).
    pub async fn layout_for_event(
        &self,
        event_id: u64,
        class_filter: &[i64],
    ) -> Result<optimiser::SynopsisLayout, SynopsisError> {
        let row = repo::event_synopsis::find_by_event_id(&self.db, event_id)
            .await?
            .ok_or(SynopsisError::NotFound)?;
        let manifest = TubeManifest::parse(&row.manifest_json)
            .map_err(|e| SynopsisError::InvalidManifest(e.to_string()))?;
        Ok(self.layout_from_manifest(&manifest, class_filter))
    }

    /// Run the optimiser over a manifest's tubes (optionally class-filtered),
    /// using the service's configured condensation knob.
    pub fn layout_from_manifest(
        &self,
        manifest: &TubeManifest,
        class_filter: &[i64],
    ) -> optimiser::SynopsisLayout {
        let tubes: Vec<_> = manifest
            .tubes
            .iter()
            .filter(|t| class_filter.is_empty() || class_filter.contains(&t.class_id))
            .cloned()
            .collect();
        optimiser::optimise(&tubes, &params_from_config(&self.config))
    }

    /// Build the poll view for an event's synopsis row (status/url/tube count).
    pub async fn status_for_event(
        &self,
        event_id: u64,
    ) -> Result<SynopsisStatusView, SynopsisError> {
        let row = repo::event_synopsis::find_by_event_id(&self.db, event_id)
            .await?
            .ok_or(SynopsisError::NotFound)?;
        let url = matches!(
            row.status,
            crate::entity::sea_orm_active_enums::SynopsisStatus::Ready
        )
        .then(|| {
            crate::configure::server::public_path(&format!(
                "/api/v3/events/{event_id}/synopsis/mp4"
            ))
        });
        Ok(SynopsisStatusView {
            status: row.status,
            url,
            tube_count: row.tube_count,
            expires_at: row.expires_at,
        })
    }

    /// Ensure a rendered mp4 exists (or is being produced) for an event, and
    /// return the current poll view. Cache → DB status → spawn a capped render:
    ///   * `Ready` with the file present → returns `ready` (no work);
    ///   * already generating / in flight → returns `generating`;
    ///   * otherwise marks the row `generating` and spawns a background render
    ///     (bounded by `max_concurrent_renders`), returning `generating`.
    pub async fn render_or_get(&self, event_id: u64) -> Result<SynopsisStatusView, SynopsisError> {
        let row = repo::event_synopsis::find_by_event_id(&self.db, event_id)
            .await?
            .ok_or(SynopsisError::NotFound)?;

        // Already rendered and the artifact is still on disk → nothing to do.
        if row.status == SynopsisStatus::Ready {
            if let Some(p) = row.rendered_path.as_deref() {
                if tokio::fs::metadata(p).await.is_ok() {
                    return self.status_for_event(event_id).await;
                }
            }
        }

        // A render already running (this process) or marked generating → poll.
        if self.in_flight.contains_key(&row.id) || row.status == SynopsisStatus::Generating {
            return self.status_for_event(event_id).await;
        }

        // Claim the row and spawn the render.
        repo::event_synopsis::update_status(&self.db, row.id, SynopsisStatus::Generating, None)
            .await?;
        self.spawn_render(row);
        self.status_for_event(event_id).await
    }

    /// Spawn a background render for a row, bounded by the render-slot semaphore.
    /// Updates the row to `Ready`(+path) or `Failed` when done. Never panics.
    fn spawn_render(&self, row: event_synopsis::Model) {
        if self.in_flight.insert(row.id, ()).is_some() {
            return; // a task is already running for this row
        }
        let db = self.db.clone();
        let config = self.config.clone();
        let slots = self.render_slots.clone();
        let in_flight = self.in_flight.clone();

        tokio::spawn(async move {
            let _permit = slots.acquire_owned().await;
            let row_id = row.id;
            let outcome = run_render(&db, &config, row).await;
            let (status, path) = match outcome {
                Ok(p) => (SynopsisStatus::Ready, Some(p)),
                Err(e) => {
                    warn!("synopsis render for row {row_id} failed: {e}");
                    (SynopsisStatus::Failed, None)
                }
            };
            if let Err(e) = repo::event_synopsis::update_status(&db, row_id, status, path).await {
                warn!("synopsis: could not record render status for row {row_id}: {e}");
            }
            in_flight.remove(&row_id);
        });
    }

    /// Resolve the on-disk mp4 for a `Ready` synopsis. `NotFound` when there is no
    /// row, it is not ready, or the cached file is gone (the client should poll
    /// `render_or_get` to (re)build it).
    pub async fn mp4_path_for_event(&self, event_id: u64) -> Result<PathBuf, SynopsisError> {
        let row = repo::event_synopsis::find_by_event_id(&self.db, event_id)
            .await?
            .ok_or(SynopsisError::NotFound)?;
        if row.status != SynopsisStatus::Ready {
            return Err(SynopsisError::NotFound);
        }
        let path = row.rendered_path.ok_or(SynopsisError::NotFound)?;
        if tokio::fs::metadata(&path).await.is_err() {
            return Err(SynopsisError::NotFound);
        }
        Ok(PathBuf::from(path))
    }

    /// Render a P4 **overview** still across every synopsis in `[from, to]` for a
    /// monitor: a montage of one cutout per tube from many events on one canvas.
    /// `class_filter`, when non-empty, keeps only those classes. Capped — the
    /// number of rows and tubes is bounded and any excess is logged.
    pub async fn overview_still(
        &self,
        monitor_id: u32,
        from: chrono::NaiveDateTime,
        to: chrono::NaiveDateTime,
        class_filter: Vec<i64>,
    ) -> Result<Vec<u8>, SynopsisError> {
        let rows = repo::event_synopsis::find_by_monitor_created_between(
            &self.db,
            monitor_id,
            from,
            to,
            OVERVIEW_ROW_LIMIT,
        )
        .await?;
        if rows.is_empty() {
            return Err(SynopsisError::NotFound);
        }
        let total =
            repo::event_synopsis::count_by_monitor_created_between(&self.db, monitor_id, from, to)
                .await?;
        if total > rows.len() as u64 {
            warn!(
                "synopsis overview: monitor {monitor_id} window has {total} synopses; \
                 rendering the newest {}",
                rows.len()
            );
        }

        let parsed: Vec<(String, TubeManifest)> = rows
            .iter()
            .filter_map(|r| {
                TubeManifest::parse(&r.manifest_json)
                    .ok()
                    .map(|m| (r.asset_dir.clone(), m))
            })
            .collect();
        let max_tubes = self.config.max_tubes_per_frame.max(1).saturating_mul(8);
        let feather = self.config.mask_feather_px;

        let bytes = tokio::task::spawn_blocking(move || {
            compositor::render_overview_still(&parsed, &class_filter, max_tubes, feather)
        })
        .await??;
        Ok(bytes)
    }

    /// Run one retention pass: remove the cached mp4 (only if it lives inside the
    /// configured `cache_dir` — zm-api never deletes zm-next's source assets) and
    /// the DB row for every synopsis whose `expires_at` has passed. Returns the
    /// number of rows cleaned.
    pub async fn run_retention_once(&self) -> Result<usize, SynopsisError> {
        let now = chrono::Utc::now().naive_utc();
        let expired = repo::event_synopsis::find_expired(&self.db, now).await?;
        let mut cleaned = 0usize;
        for row in expired {
            if let Some(path) = row.rendered_path.as_deref() {
                if path_within(&self.config.cache_dir, Path::new(path)) {
                    let _ = tokio::fs::remove_file(path).await;
                }
            }
            repo::event_synopsis::delete_by_id(&self.db, row.id).await?;
            cleaned += 1;
        }
        if cleaned > 0 {
            info!("synopsis retention: cleaned {cleaned} expired synopses");
        }
        Ok(cleaned)
    }

    /// Spawn the periodic retention loop (hourly by default). No-op-friendly: a
    /// failed pass is logged and retried on the next tick.
    pub fn spawn_retention_task(self: Arc<Self>, interval: Duration) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Err(e) = self.run_retention_once().await {
                    warn!("synopsis retention pass failed: {e}");
                }
            }
        });
    }
}

/// Max synopsis rows merged into one overview (bounds cost on busy cameras).
const OVERVIEW_ROW_LIMIT: u64 = 200;

/// Lexical containment check: is `path` under `base`? Rendered mp4 paths are
/// always `cache_dir.join(...)`, so a prefix test is sufficient and safe even
/// when the file has already been removed (no canonicalisation needed).
fn path_within(base: &Path, path: &Path) -> bool {
    !base.as_os_str().is_empty() && path.starts_with(base)
}

/// Build optimiser parameters from the service config.
fn params_from_config(config: &SynopsisConfig) -> optimiser::OptimiserParams {
    optimiser::OptimiserParams {
        fps: config.output_fps,
        collision_budget: config.clamped_collision_budget(),
        max_tubes_per_frame: config.max_tubes_per_frame,
        ..Default::default()
    }
}

/// Render an event's synopsis mp4 to the cache dir, returning the file path.
/// Runs the (blocking) encode on a blocking thread, bounded by a wall-clock
/// timeout. Pure orchestration — all pixels come from the manifest's JPEGs.
async fn run_render(
    db: &DatabaseConnection,
    config: &SynopsisConfig,
    row: event_synopsis::Model,
) -> Result<String, SynopsisError> {
    let _ = db; // reserved for future progress reporting
    if !config.encoder_is_native() {
        return Err(SynopsisError::EncoderUnavailable(format!(
            "unsupported encoder_backend {:?}; only \"native\" (ffmpeg-next) is supported",
            config.encoder_backend
        )));
    }

    let manifest = TubeManifest::parse(&row.manifest_json)
        .map_err(|e| SynopsisError::InvalidManifest(e.to_string()))?;
    let layout = {
        let tubes: Vec<_> = manifest.tubes.clone();
        optimiser::optimise(&tubes, &params_from_config(config))
    };

    tokio::fs::create_dir_all(&config.cache_dir).await?;
    let event_key = row.event_id.unwrap_or(row.id);
    let out = render::mp4_cache_path(&config.cache_dir, event_key);

    let asset_dir = PathBuf::from(&row.asset_dir);
    let fps = config.output_fps;
    let feather = config.mask_feather_px;
    let out_blocking = out.clone();
    let timeout = Duration::from_secs(config.render_timeout_seconds.max(1));

    let render = tokio::task::spawn_blocking(move || {
        render::render_mp4(&manifest, &asset_dir, &layout, fps, feather, &out_blocking)
    });

    match tokio::time::timeout(timeout, render).await {
        Ok(joined) => joined??,
        Err(_) => {
            return Err(SynopsisError::RenderFailed(format!(
                "render exceeded {}s timeout",
                config.render_timeout_seconds
            )))
        }
    }

    Ok(out.to_string_lossy().into_owned())
}
