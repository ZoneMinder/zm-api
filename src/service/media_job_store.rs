//! Where the media job queue keeps its jobs.
//!
//! ZoneMinder owns the database schema, so zm-api doesn't create tables in
//! it. A database that already has zm-api's `media_jobs` table (created by
//! an earlier `zm-api-db bridge`) keeps using it; otherwise the queue lives
//! in a JSON file in zm-api's own state directory (`/var/lib/zm-api`, which
//! systemd's `StateDirectory=` creates). Which one is decided once, by a
//! read-only look at `information_schema`.

use crate::util::time;
use std::path::PathBuf;
use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    EntityTrait, NotSet, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::entity::media_jobs::{self, Model};
use crate::error::{AppError, AppResult};

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const CANCELLED: &str = "cancelled";

/// Most jobs a listing returns, and most finished jobs the file keeps.
pub const LIST_LIMIT: usize = 500;

/// A job not yet stored: everything but its id.
pub struct NewJob {
    pub kind: String,
    pub event_id: u64,
    pub monitor_id: u32,
    pub params: String,
    pub created_by: Option<u32>,
    pub created_at: sea_orm::prelude::DateTime,
}

/// Which jobs a listing wants.
#[derive(Default)]
pub struct JobFilter {
    pub event_id: Option<u64>,
    pub status: Option<String>,
    /// `None`: every monitor.
    pub monitor_ids: Option<Vec<u32>>,
}

pub enum JobStore {
    Table,
    File(FileJobs),
}

/// Whether ZoneMinder's database has zm-api's `media_jobs` table.
pub async fn table_exists(db: &DatabaseConnection) -> AppResult<bool> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Postgres => {
            "SELECT COUNT(*) AS n FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = 'media_jobs'"
        }
        _ => {
            "SELECT COUNT(*) AS n FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = 'media_jobs'"
        }
    };
    let row = db
        .query_one_raw(Statement::from_string(backend, sql.to_string()))
        .await?;
    Ok(row
        .and_then(|r| r.try_get::<i64>("", "n").ok())
        .unwrap_or(0)
        > 0)
}

/// zm-api's state directory: systemd's `$STATE_DIRECTORY`, else
/// `/var/lib/zm-api`.
pub fn state_dir() -> PathBuf {
    std::env::var("STATE_DIRECTORY")
        .ok()
        .and_then(|v| v.split(':').next().map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/var/lib/zm-api"))
}

impl JobStore {
    /// The table when it exists, otherwise `media-jobs.json` in `dir`.
    pub async fn open(db: &DatabaseConnection, dir: PathBuf) -> Self {
        match table_exists(db).await {
            Ok(true) => {
                info!("media jobs: using the media_jobs table");
                JobStore::Table
            }
            Ok(false) => {
                let path = dir.join("media-jobs.json");
                info!(
                    "media jobs: no media_jobs table; keeping jobs in {}",
                    path.display()
                );
                JobStore::File(FileJobs::load(path).await)
            }
            Err(e) => {
                // Can't tell; don't touch the database's schema either way.
                let path = dir.join("media-jobs.json");
                warn!(
                    "media jobs: can't check for the media_jobs table ({e}); keeping jobs in {}",
                    path.display()
                );
                JobStore::File(FileJobs::load(path).await)
            }
        }
    }

    pub async fn insert(&self, db: &DatabaseConnection, job: NewJob) -> AppResult<Model> {
        match self {
            JobStore::Table => Ok(media_jobs::ActiveModel {
                id: NotSet,
                kind: Set(job.kind),
                event_id: Set(job.event_id),
                monitor_id: Set(job.monitor_id),
                params: Set(job.params),
                status: Set(QUEUED.into()),
                output: Set(None),
                error: Set(None),
                created_by: Set(job.created_by),
                created_at: Set(job.created_at),
                started_at: Set(None),
                finished_at: Set(None),
            }
            .insert(db)
            .await?),
            JobStore::File(f) => f.insert(job).await,
        }
    }

    /// Newest first, at most [`LIST_LIMIT`].
    pub async fn list(&self, db: &DatabaseConnection, filter: &JobFilter) -> AppResult<Vec<Model>> {
        match self {
            JobStore::Table => {
                let mut q = media_jobs::Entity::find()
                    .order_by_desc(media_jobs::Column::Id)
                    .limit(LIST_LIMIT as u64);
                if let Some(e) = filter.event_id {
                    q = q.filter(media_jobs::Column::EventId.eq(e));
                }
                if let Some(s) = &filter.status {
                    q = q.filter(media_jobs::Column::Status.eq(s.clone()));
                }
                if let Some(ids) = &filter.monitor_ids {
                    q = q.filter(media_jobs::Column::MonitorId.is_in(ids.iter().copied()));
                }
                Ok(q.all(db).await?)
            }
            JobStore::File(f) => Ok(f.list(filter).await),
        }
    }

    pub async fn get(&self, db: &DatabaseConnection, id: u32) -> AppResult<Option<Model>> {
        match self {
            JobStore::Table => Ok(media_jobs::Entity::find_by_id(id).one(db).await?),
            JobStore::File(f) => Ok(f.get(id).await),
        }
    }

    /// Cancel a job that is still queued; `false` when it isn't.
    pub async fn cancel_queued(
        &self,
        db: &DatabaseConnection,
        id: u32,
        at: sea_orm::prelude::DateTime,
    ) -> AppResult<bool> {
        match self {
            JobStore::Table => Ok(media_jobs::Entity::update_many()
                .col_expr(media_jobs::Column::Status, Expr::value(CANCELLED))
                .col_expr(media_jobs::Column::FinishedAt, Expr::value(at))
                .filter(media_jobs::Column::Id.eq(id))
                .filter(media_jobs::Column::Status.eq(QUEUED))
                .exec(db)
                .await?
                .rows_affected
                == 1),
            JobStore::File(f) => {
                f.update(id, |j| {
                    (j.status == QUEUED).then(|| {
                        j.status = CANCELLED.into();
                        j.finished_at = Some(at);
                    })
                })
                .await
            }
        }
    }

    pub async fn delete(&self, db: &DatabaseConnection, id: u32) -> AppResult<()> {
        match self {
            JobStore::Table => {
                media_jobs::Entity::delete_by_id(id).exec(db).await?;
                Ok(())
            }
            JobStore::File(f) => f.delete(id).await,
        }
    }

    /// The oldest queued job, leaving out the ids in `skip`: jobs this server
    /// has seen and can't run (their event's files are on another server).
    pub async fn next_queued(
        &self,
        db: &DatabaseConnection,
        skip: &[u32],
    ) -> AppResult<Option<Model>> {
        match self {
            JobStore::Table => Ok(media_jobs::Entity::find()
                .filter(media_jobs::Column::Status.eq(QUEUED))
                .filter(media_jobs::Column::Id.is_not_in(skip.iter().copied()))
                .order_by_asc(media_jobs::Column::Id)
                .one(db)
                .await?),
            JobStore::File(f) => Ok(f.next_queued(skip).await),
        }
    }

    /// Mark a queued job running; `false` when another worker got it first.
    pub async fn claim(
        &self,
        db: &DatabaseConnection,
        id: u32,
        at: sea_orm::prelude::DateTime,
    ) -> AppResult<bool> {
        match self {
            JobStore::Table => Ok(media_jobs::Entity::update_many()
                .col_expr(media_jobs::Column::Status, Expr::value(RUNNING))
                .col_expr(media_jobs::Column::StartedAt, Expr::value(at))
                .filter(media_jobs::Column::Id.eq(id))
                .filter(media_jobs::Column::Status.eq(QUEUED))
                .exec(db)
                .await?
                .rows_affected
                == 1),
            JobStore::File(f) => {
                f.update(id, |j| {
                    (j.status == QUEUED).then(|| {
                        j.status = RUNNING.into();
                        j.started_at = Some(at);
                    })
                })
                .await
            }
        }
    }

    /// Store a job's result (status, output or error, finish time).
    pub async fn finish(&self, db: &DatabaseConnection, job: Model) -> AppResult<()> {
        match self {
            JobStore::Table => {
                let mut am: media_jobs::ActiveModel = job.clone().into();
                am.status = Set(job.status);
                am.output = Set(job.output);
                am.error = Set(job.error);
                am.finished_at = Set(job.finished_at);
                am.update(db).await?;
                Ok(())
            }
            JobStore::File(f) => f
                .update(job.id, |j| {
                    *j = job.clone();
                    Some(())
                })
                .await
                .map(|_| ()),
        }
    }

    /// Put jobs a restart interrupted back in the queue.
    pub async fn requeue_running(&self, db: &DatabaseConnection) -> AppResult<()> {
        match self {
            JobStore::Table => {
                media_jobs::Entity::update_many()
                    .col_expr(media_jobs::Column::Status, Expr::value(QUEUED))
                    .filter(media_jobs::Column::Status.eq(RUNNING))
                    .exec(db)
                    .await?;
                Ok(())
            }
            JobStore::File(f) => f.requeue_running().await,
        }
    }
}

/// A job as the file stores it. Times are Unix milliseconds, UTC.
#[derive(Serialize, Deserialize)]
struct Stored {
    id: u32,
    kind: String,
    event_id: u64,
    monitor_id: u32,
    params: String,
    status: String,
    output: Option<String>,
    error: Option<String>,
    created_by: Option<u32>,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

/// The columns hold UTC digits. A stored job keeps them as epoch milliseconds.
fn to_ms(t: sea_orm::prelude::DateTime) -> i64 {
    time::utc_instant(t).as_millisecond()
}

fn from_ms(ms: i64) -> sea_orm::prelude::DateTime {
    let t = jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    time::db_datetime(time::utc_civil(t))
}

impl From<&Model> for Stored {
    fn from(m: &Model) -> Self {
        Self {
            id: m.id,
            kind: m.kind.clone(),
            event_id: m.event_id,
            monitor_id: m.monitor_id,
            params: m.params.clone(),
            status: m.status.clone(),
            output: m.output.clone(),
            error: m.error.clone(),
            created_by: m.created_by,
            created_at: to_ms(m.created_at),
            started_at: m.started_at.map(to_ms),
            finished_at: m.finished_at.map(to_ms),
        }
    }
}

impl From<Stored> for Model {
    fn from(s: Stored) -> Self {
        Self {
            id: s.id,
            kind: s.kind,
            event_id: s.event_id,
            monitor_id: s.monitor_id,
            params: s.params,
            status: s.status,
            output: s.output,
            error: s.error,
            created_by: s.created_by,
            created_at: from_ms(s.created_at),
            started_at: s.started_at.map(from_ms),
            finished_at: s.finished_at.map(from_ms),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct FileState {
    next_id: u32,
    jobs: Vec<Stored>,
}

/// The queue in a JSON file, rewritten (atomically) on every change. One
/// zm-api instance per state directory.
pub struct FileJobs {
    path: PathBuf,
    jobs: Mutex<Vec<Model>>,
    next_id: Mutex<u32>,
}

impl FileJobs {
    /// Read `path`, or start empty when it's missing or unreadable.
    pub async fn load(path: PathBuf) -> Self {
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<FileState>(&bytes).unwrap_or_else(|e| {
                warn!(
                    "media jobs: {} is unreadable ({e}); starting empty",
                    path.display()
                );
                FileState::default()
            }),
            Err(_) => FileState::default(),
        };
        let jobs: Vec<Model> = state.jobs.into_iter().map(Model::from).collect();
        let next_id = jobs
            .iter()
            .map(|j| j.id + 1)
            .max()
            .unwrap_or(1)
            .max(state.next_id)
            .max(1);
        Self {
            path,
            jobs: Mutex::new(jobs),
            next_id: Mutex::new(next_id),
        }
    }

    async fn save(&self, jobs: &[Model]) -> AppResult<()> {
        let state = FileState {
            next_id: *self.next_id.lock().await,
            jobs: jobs.iter().map(Stored::from).collect(),
        };
        let bytes = serde_json::to_vec(&state)?;
        if let Some(dir) = self.path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }

    /// Keep every unfinished job and the newest [`LIST_LIMIT`] finished
    /// ones, so the file doesn't grow forever.
    fn prune(jobs: &mut Vec<Model>) {
        let finished = jobs
            .iter()
            .filter(|j| j.status != QUEUED && j.status != RUNNING)
            .count();
        if finished <= LIST_LIMIT {
            return;
        }
        let mut drop = finished - LIST_LIMIT;
        jobs.retain(|j| {
            let done = j.status != QUEUED && j.status != RUNNING;
            if done && drop > 0 {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }

    async fn insert(&self, job: NewJob) -> AppResult<Model> {
        let mut jobs = self.jobs.lock().await;
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next = next
                .checked_add(1)
                .ok_or_else(|| AppError::InternalServerError("media job ids exhausted".into()))?;
            id
        };
        let model = Model {
            id,
            kind: job.kind,
            event_id: job.event_id,
            monitor_id: job.monitor_id,
            params: job.params,
            status: QUEUED.into(),
            output: None,
            error: None,
            created_by: job.created_by,
            created_at: job.created_at,
            started_at: None,
            finished_at: None,
        };
        jobs.push(model.clone());
        Self::prune(&mut jobs);
        self.save(&jobs).await?;
        Ok(model)
    }

    async fn list(&self, filter: &JobFilter) -> Vec<Model> {
        let jobs = self.jobs.lock().await;
        jobs.iter()
            .rev()
            .filter(|j| filter.event_id.is_none_or(|e| j.event_id == e))
            .filter(|j| filter.status.as_ref().is_none_or(|s| &j.status == s))
            .filter(|j| {
                filter
                    .monitor_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&j.monitor_id))
            })
            .take(LIST_LIMIT)
            .cloned()
            .collect()
    }

    async fn get(&self, id: u32) -> Option<Model> {
        self.jobs.lock().await.iter().find(|j| j.id == id).cloned()
    }

    /// Apply `change` to job `id`; it returns `None` to leave the job as
    /// it was. `true` when the job changed (and was saved).
    async fn update(
        &self,
        id: u32,
        change: impl FnOnce(&mut Model) -> Option<()>,
    ) -> AppResult<bool> {
        let mut jobs = self.jobs.lock().await;
        let Some(job) = jobs.iter_mut().find(|j| j.id == id) else {
            return Ok(false);
        };
        if change(job).is_none() {
            return Ok(false);
        }
        self.save(&jobs).await?;
        Ok(true)
    }

    async fn delete(&self, id: u32) -> AppResult<()> {
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|j| j.id != id);
        self.save(&jobs).await
    }

    async fn next_queued(&self, skip: &[u32]) -> Option<Model> {
        self.jobs
            .lock()
            .await
            .iter()
            .find(|j| j.status == QUEUED && !skip.contains(&j.id))
            .cloned()
    }

    async fn requeue_running(&self) -> AppResult<()> {
        let mut jobs = self.jobs.lock().await;
        let mut changed = false;
        for j in jobs.iter_mut().filter(|j| j.status == RUNNING) {
            j.status = QUEUED.into();
            changed = true;
        }
        if changed {
            self.save(&jobs).await?;
        }
        Ok(())
    }
}

/// The store, decided on first use.
pub type SharedJobStore = Arc<tokio::sync::OnceCell<JobStore>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn new_job(event_id: u64, monitor_id: u32) -> NewJob {
        NewJob {
            kind: "video".into(),
            event_id,
            monitor_id,
            params: "{}".into(),
            created_by: Some(1),
            created_at: from_ms(1_760_000_000_123),
        }
    }

    fn db() -> DatabaseConnection {
        sea_orm::MockDatabase::new(DatabaseBackend::MySql).into_connection()
    }

    #[tokio::test]
    async fn a_file_queue_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media-jobs.json");
        let store = JobStore::File(FileJobs::load(path.clone()).await);
        let db = db();
        let a = store.insert(&db, new_job(10, 1)).await.unwrap();
        let b = store.insert(&db, new_job(11, 2)).await.unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert!(store.claim(&db, a.id, from_ms(5)).await.unwrap());
        assert!(
            !store.claim(&db, a.id, from_ms(5)).await.unwrap(),
            "claimed once"
        );

        // A restart: the running job goes back in the queue, ids continue.
        let store = JobStore::File(FileJobs::load(path).await);
        store.requeue_running(&db).await.unwrap();
        let next = store.next_queued(&db, &[]).await.unwrap().unwrap();
        assert_eq!(next.id, a.id);
        assert_eq!(next.created_at, from_ms(1_760_000_000_123));
        let c = store.insert(&db, new_job(12, 1)).await.unwrap();
        assert_eq!(c.id, 3);
    }

    #[tokio::test]
    async fn the_next_job_can_pass_over_ones_this_server_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::File(FileJobs::load(dir.path().join("j.json")).await);
        let db = db();
        let a = store.insert(&db, new_job(10, 1)).await.unwrap();
        let b = store.insert(&db, new_job(11, 1)).await.unwrap();
        let next = store.next_queued(&db, &[a.id]).await.unwrap().unwrap();
        assert_eq!(next.id, b.id);
        assert!(store
            .next_queued(&db, &[a.id, b.id])
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn listing_filters_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::File(FileJobs::load(dir.path().join("j.json")).await);
        let db = db();
        for (e, m) in [(10, 1), (11, 2), (10, 2)] {
            store.insert(&db, new_job(e, m)).await.unwrap();
        }
        let ids = |v: Vec<Model>| v.iter().map(|j| j.id).collect::<Vec<_>>();
        let all = store.list(&db, &JobFilter::default()).await.unwrap();
        assert_eq!(ids(all), [3, 2, 1]);
        let f = JobFilter {
            event_id: Some(10),
            ..Default::default()
        };
        assert_eq!(ids(store.list(&db, &f).await.unwrap()), [3, 1]);
        let f = JobFilter {
            monitor_ids: Some(vec![1]),
            ..Default::default()
        };
        assert_eq!(ids(store.list(&db, &f).await.unwrap()), [1]);
    }

    #[tokio::test]
    async fn cancel_only_while_queued_and_finish_records_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::File(FileJobs::load(dir.path().join("j.json")).await);
        let db = db();
        let a = store.insert(&db, new_job(1, 1)).await.unwrap();
        let b = store.insert(&db, new_job(2, 1)).await.unwrap();
        assert!(store.cancel_queued(&db, a.id, from_ms(9)).await.unwrap());
        assert!(store.claim(&db, b.id, from_ms(9)).await.unwrap());
        assert!(!store.cancel_queued(&db, b.id, from_ms(9)).await.unwrap());
        let mut done = store.get(&db, b.id).await.unwrap().unwrap();
        done.status = "done".into();
        done.output = Some("Event-2.mp4".into());
        done.finished_at = Some(from_ms(10));
        store.finish(&db, done).await.unwrap();
        let got = store.get(&db, b.id).await.unwrap().unwrap();
        assert_eq!(got.output.as_deref(), Some("Event-2.mp4"));
        store.delete(&db, a.id).await.unwrap();
        assert!(store.get(&db, a.id).await.unwrap().is_none());
        assert!(store.next_queued(&db, &[]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_file_keeps_unfinished_jobs_and_the_newest_finished() {
        let mut jobs: Vec<Model> = (1..=LIST_LIMIT as u32 + 3)
            .map(|id| Model {
                id,
                kind: "video".into(),
                event_id: 1,
                monitor_id: 1,
                params: "{}".into(),
                status: if id == 2 {
                    QUEUED.into()
                } else {
                    "done".into()
                },
                output: None,
                error: None,
                created_by: None,
                created_at: from_ms(0),
                started_at: None,
                finished_at: None,
            })
            .collect();
        FileJobs::prune(&mut jobs);
        assert_eq!(jobs.len(), LIST_LIMIT + 1);
        assert!(jobs.iter().any(|j| j.id == 2), "the queued job stays");
        assert!(!jobs.iter().any(|j| j.id == 1), "the oldest finished goes");
    }

    #[tokio::test]
    async fn a_missing_table_means_the_file() {
        let db = sea_orm::MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results([[std::collections::BTreeMap::from([(
                "n",
                sea_orm::Value::BigInt(Some(0)),
            )])]])
            .into_connection();
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            JobStore::open(&db, dir.path().to_path_buf()).await,
            JobStore::File(_)
        ));
        let db = sea_orm::MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results([[std::collections::BTreeMap::from([(
                "n",
                sea_orm::Value::BigInt(Some(1)),
            )])]])
            .into_connection();
        assert!(matches!(
            JobStore::open(&db, dir.path().to_path_buf()).await,
            JobStore::Table
        ));
    }
}
