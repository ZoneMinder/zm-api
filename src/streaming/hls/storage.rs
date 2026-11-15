//! HLS segment storage and lifecycle management
//!
//! Manages segment files on disk with automatic cleanup of old segments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Information about a stored segment
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub sequence: u64,
    pub path: PathBuf,
    pub size: u64,
    pub duration: Duration,
    pub created_at: Instant,
    pub is_partial: bool,
}

/// Storage configuration
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub base_path: PathBuf,
    pub retention: Duration,
    pub cleanup_interval: Duration,
    pub max_segments_per_stream: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("/tmp/hls"),
            retention: Duration::from_secs(60 * 30), // 30 minutes
            cleanup_interval: Duration::from_secs(60),
            max_segments_per_stream: 100,
        }
    }
}

/// Per-monitor storage state
struct MonitorStorage {
    init_segment: Option<Vec<u8>>,
    segments: HashMap<u64, SegmentInfo>,
    partial_segments: HashMap<(u64, u32), SegmentInfo>, // (sequence, part_index)
    last_cleanup: Instant,
}

impl MonitorStorage {
    fn new() -> Self {
        Self {
            init_segment: None,
            segments: HashMap::new(),
            partial_segments: HashMap::new(),
            last_cleanup: Instant::now(),
        }
    }
}

/// HLS segment storage manager
pub struct HlsStorage {
    config: StorageConfig,
    monitors: Arc<RwLock<HashMap<u32, MonitorStorage>>>,
}

impl HlsStorage {
    /// Create a new storage manager
    pub fn new(config: StorageConfig) -> Self {
        Self {
            config,
            monitors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(StorageConfig::default())
    }

    /// Initialize storage directory for a monitor
    pub async fn init_monitor(&self, monitor_id: u32) -> Result<PathBuf, StorageError> {
        secure_base_dir(&self.config.base_path).map_err(|e| StorageError::IoError {
            path: self.config.base_path.clone(),
            source: e,
        })?;
        let monitor_path = self.config.base_path.join(monitor_id.to_string());

        // Create directory if it doesn't exist
        if !monitor_path.exists() {
            fs::create_dir_all(&monitor_path)
                .await
                .map_err(|e| StorageError::IoError {
                    path: monitor_path.clone(),
                    source: e,
                })?;
            info!(
                "Created HLS directory for monitor {}: {:?}",
                monitor_id, monitor_path
            );
        }

        // Initialize monitor storage state
        let mut monitors = self.monitors.write().await;
        monitors
            .entry(monitor_id)
            .or_insert_with(MonitorStorage::new);

        Ok(monitor_path)
    }

    /// Store initialization segment
    pub async fn store_init_segment(
        &self,
        monitor_id: u32,
        data: &[u8],
    ) -> Result<PathBuf, StorageError> {
        let monitor_path = self.init_monitor(monitor_id).await?;
        let init_path = monitor_path.join("init.mp4");

        fs::write(&init_path, data)
            .await
            .map_err(|e| StorageError::IoError {
                path: init_path.clone(),
                source: e,
            })?;

        // Cache in memory
        let mut monitors = self.monitors.write().await;
        if let Some(storage) = monitors.get_mut(&monitor_id) {
            storage.init_segment = Some(data.to_vec());
        }

        debug!(
            "Stored init segment for monitor {}: {} bytes",
            monitor_id,
            data.len()
        );
        Ok(init_path)
    }

    /// Get cached init segment
    pub async fn get_init_segment(&self, monitor_id: u32) -> Option<Vec<u8>> {
        let monitors = self.monitors.read().await;
        monitors
            .get(&monitor_id)
            .and_then(|s| s.init_segment.clone())
    }

    /// Store a media segment
    pub async fn store_segment(
        &self,
        monitor_id: u32,
        sequence: u64,
        data: &[u8],
        duration: Duration,
    ) -> Result<SegmentInfo, StorageError> {
        let monitor_path = self.init_monitor(monitor_id).await?;
        let segment_name = format!("segment_{:05}.m4s", sequence);
        let segment_path = monitor_path.join(&segment_name);

        fs::write(&segment_path, data)
            .await
            .map_err(|e| StorageError::IoError {
                path: segment_path.clone(),
                source: e,
            })?;

        let info = SegmentInfo {
            sequence,
            path: segment_path,
            size: data.len() as u64,
            duration,
            created_at: Instant::now(),
            is_partial: false,
        };

        // Track segment and enforce the per-stream segment cap *atomically* with
        // the insert. Deciding which sequences to evict under the same write lock
        // that performs the insert closes a TOCTOU window where two concurrent
        // inserts could each observe the count below the cap and skip eviction
        // (unbounded growth) or both evict the same boundary segment. Only the
        // filesystem deletes are deferred until after the lock is released — the
        // in-memory map is always bounded. See REVIEW_FIXES_PLAN §3.3.
        let mut due_for_interval_cleanup = false;
        let evicted_paths: Vec<PathBuf> = {
            let mut monitors = self.monitors.write().await;
            match monitors.get_mut(&monitor_id) {
                Some(storage) => {
                    storage.segments.insert(sequence, info.clone());

                    let mut evicted = Vec::new();
                    if storage.segments.len() > self.config.max_segments_per_stream {
                        let mut seqs: Vec<u64> = storage.segments.keys().copied().collect();
                        seqs.sort_unstable();
                        let excess = storage.segments.len() - self.config.max_segments_per_stream;
                        for seq in seqs.into_iter().take(excess) {
                            if let Some(removed) = storage.segments.remove(&seq) {
                                evicted.push(removed.path);
                            }
                        }
                    }

                    // Time-based retention (and partial-segment cleanup) still
                    // runs on the slower interval path below.
                    due_for_interval_cleanup =
                        storage.last_cleanup.elapsed() >= self.config.cleanup_interval;
                    evicted
                }
                None => Vec::new(),
            }
        };

        for path in &evicted_paths {
            if let Err(e) = fs::remove_file(path).await {
                warn!("Failed to remove evicted segment file {:?}: {}", path, e);
            }
        }

        if due_for_interval_cleanup {
            self.cleanup_monitor(monitor_id).await;
        }

        debug!(
            "Stored segment {} for monitor {}: {} bytes, {:.2}s",
            sequence,
            monitor_id,
            data.len(),
            duration.as_secs_f64()
        );

        Ok(info)
    }

    /// Store a partial segment (for LL-HLS)
    pub async fn store_partial_segment(
        &self,
        monitor_id: u32,
        sequence: u64,
        part_index: u32,
        data: &[u8],
        duration: Duration,
    ) -> Result<SegmentInfo, StorageError> {
        let monitor_path = self.init_monitor(monitor_id).await?;
        let segment_name = format!("segment_{:05}.{}.m4s", sequence, part_index);
        let segment_path = monitor_path.join(&segment_name);

        fs::write(&segment_path, data)
            .await
            .map_err(|e| StorageError::IoError {
                path: segment_path.clone(),
                source: e,
            })?;

        let info = SegmentInfo {
            sequence,
            path: segment_path,
            size: data.len() as u64,
            duration,
            created_at: Instant::now(),
            is_partial: true,
        };

        // Track partial segment
        let mut monitors = self.monitors.write().await;
        if let Some(storage) = monitors.get_mut(&monitor_id) {
            storage
                .partial_segments
                .insert((sequence, part_index), info.clone());
        }

        debug!(
            "Stored partial segment {}.{} for monitor {}: {} bytes",
            sequence,
            part_index,
            monitor_id,
            data.len()
        );

        Ok(info)
    }

    /// Get segment info
    pub async fn get_segment_info(&self, monitor_id: u32, sequence: u64) -> Option<SegmentInfo> {
        let monitors = self.monitors.read().await;
        monitors
            .get(&monitor_id)
            .and_then(|s| s.segments.get(&sequence).cloned())
    }

    /// Read segment data from disk
    pub async fn read_segment(
        &self,
        monitor_id: u32,
        sequence: u64,
    ) -> Result<Vec<u8>, StorageError> {
        let info =
            self.get_segment_info(monitor_id, sequence)
                .await
                .ok_or(StorageError::NotFound {
                    monitor_id,
                    sequence,
                })?;

        fs::read(&info.path)
            .await
            .map_err(|e| StorageError::IoError {
                path: info.path,
                source: e,
            })
    }

    /// Read init segment from disk
    pub async fn read_init_segment(&self, monitor_id: u32) -> Result<Vec<u8>, StorageError> {
        // Try cache first
        if let Some(data) = self.get_init_segment(monitor_id).await {
            return Ok(data);
        }

        // Read from disk
        let path = self
            .config
            .base_path
            .join(monitor_id.to_string())
            .join("init.mp4");

        fs::read(&path)
            .await
            .map_err(|e| StorageError::IoError { path, source: e })
    }

    /// Get list of available segments for a monitor
    pub async fn list_segments(&self, monitor_id: u32) -> Vec<SegmentInfo> {
        let monitors = self.monitors.read().await;
        monitors
            .get(&monitor_id)
            .map(|s| {
                let mut segments: Vec<_> = s.segments.values().cloned().collect();
                segments.sort_by_key(|s| s.sequence);
                segments
            })
            .unwrap_or_default()
    }

    /// Get the latest N segments
    pub async fn get_latest_segments(&self, monitor_id: u32, count: usize) -> Vec<SegmentInfo> {
        let mut segments = self.list_segments(monitor_id).await;
        if segments.len() > count {
            segments.drain(0..segments.len() - count);
        }
        segments
    }

    /// Cleanup old segments for a monitor
    pub async fn cleanup_monitor(&self, monitor_id: u32) {
        let now = Instant::now();
        let mut to_remove = Vec::new();
        let mut partial_to_remove = Vec::new();

        {
            let monitors = self.monitors.read().await;
            if let Some(storage) = monitors.get(&monitor_id) {
                // Find expired segments
                for (seq, info) in &storage.segments {
                    if now.duration_since(info.created_at) > self.config.retention {
                        to_remove.push((*seq, info.path.clone()));
                    }
                }

                // Find expired partial segments
                for (key, info) in &storage.partial_segments {
                    if now.duration_since(info.created_at) > self.config.retention {
                        partial_to_remove.push((*key, info.path.clone()));
                    }
                }

                // Also limit total segment count
                let mut all_seqs: Vec<_> = storage.segments.keys().copied().collect();
                all_seqs.sort();
                if all_seqs.len() > self.config.max_segments_per_stream {
                    let excess = all_seqs.len() - self.config.max_segments_per_stream;
                    for seq in all_seqs.into_iter().take(excess) {
                        if let Some(info) = storage.segments.get(&seq) {
                            if !to_remove.iter().any(|(s, _)| *s == seq) {
                                to_remove.push((seq, info.path.clone()));
                            }
                        }
                    }
                }
            }
        }

        // Delete files and update state
        if !to_remove.is_empty() || !partial_to_remove.is_empty() {
            let mut monitors = self.monitors.write().await;
            if let Some(storage) = monitors.get_mut(&monitor_id) {
                for (seq, path) in &to_remove {
                    if let Err(e) = fs::remove_file(path).await {
                        warn!("Failed to remove segment file {:?}: {}", path, e);
                    }
                    storage.segments.remove(seq);
                }

                for (key, path) in &partial_to_remove {
                    if let Err(e) = fs::remove_file(path).await {
                        warn!("Failed to remove partial segment file {:?}: {}", path, e);
                    }
                    storage.partial_segments.remove(key);
                }

                storage.last_cleanup = now;
            }
        }

        if !to_remove.is_empty() {
            debug!(
                "Cleaned up {} segments for monitor {}",
                to_remove.len(),
                monitor_id
            );
        }
    }

    /// Cleanup all monitors
    pub async fn cleanup_all(&self) {
        let monitor_ids: Vec<u32> = {
            let monitors = self.monitors.read().await;
            monitors.keys().copied().collect()
        };

        for monitor_id in monitor_ids {
            self.cleanup_monitor(monitor_id).await;
        }
    }

    /// Clean stale segment files and init.mp4 from a monitor directory without removing the directory.
    ///
    /// Called at session start to prevent leftover files from a previous session
    /// from misleading clients (e.g., a stale `init.mp4` with 0 packets processed).
    pub async fn clean_monitor(&self, monitor_id: u32) -> Result<(), StorageError> {
        let monitor_path = self.config.base_path.join(monitor_id.to_string());

        if !monitor_path.exists() {
            return Ok(());
        }

        let mut dir = fs::read_dir(&monitor_path)
            .await
            .map_err(|e| StorageError::IoError {
                path: monitor_path.clone(),
                source: e,
            })?;

        while let Some(entry) = dir.next_entry().await.map_err(|e| StorageError::IoError {
            path: monitor_path.clone(),
            source: e,
        })? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "init.mp4" || (name.starts_with("segment_") && name.ends_with(".m4s")) {
                if let Err(e) = fs::remove_file(entry.path()).await {
                    warn!("Failed to remove stale file {:?}: {}", entry.path(), e);
                }
            }
        }

        // Clear in-memory state for this monitor
        let mut monitors = self.monitors.write().await;
        if let Some(storage) = monitors.get_mut(&monitor_id) {
            storage.init_segment = None;
            storage.segments.clear();
            storage.partial_segments.clear();
        }

        debug!("Cleaned stale HLS files for monitor {}", monitor_id);
        Ok(())
    }

    /// Remove all data for a monitor
    pub async fn remove_monitor(&self, monitor_id: u32) -> Result<(), StorageError> {
        let monitor_path = self.config.base_path.join(monitor_id.to_string());

        if monitor_path.exists() {
            fs::remove_dir_all(&monitor_path)
                .await
                .map_err(|e| StorageError::IoError {
                    path: monitor_path,
                    source: e,
                })?;
        }

        let mut monitors = self.monitors.write().await;
        monitors.remove(&monitor_id);

        info!("Removed HLS storage for monitor {}", monitor_id);
        Ok(())
    }

    /// Get storage statistics
    pub async fn get_stats(&self, monitor_id: u32) -> Option<StorageStats> {
        let monitors = self.monitors.read().await;
        monitors.get(&monitor_id).map(|storage| {
            let total_size: u64 = storage.segments.values().map(|s| s.size).sum();
            let total_duration: Duration = storage.segments.values().map(|s| s.duration).sum();

            StorageStats {
                segment_count: storage.segments.len(),
                partial_segment_count: storage.partial_segments.len(),
                total_size,
                total_duration,
                has_init_segment: storage.init_segment.is_some(),
                oldest_sequence: storage.segments.keys().min().copied(),
                newest_sequence: storage.segments.keys().max().copied(),
            }
        })
    }

    /// Get the base path
    pub fn base_path(&self) -> &Path {
        &self.config.base_path
    }

    /// Start background cleanup task
    pub fn start_cleanup_task(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let interval = self.config.cleanup_interval;

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);

            loop {
                interval_timer.tick().await;
                self.cleanup_all().await;
            }
        })
    }
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub segment_count: usize,
    pub partial_segment_count: usize,
    pub total_size: u64,
    pub total_duration: Duration,
    pub has_init_segment: bool,
    pub oldest_sequence: Option<u64>,
    pub newest_sequence: Option<u64>,
}

/// Make `base` a directory only this process's user can enter, creating it
/// if needed.
///
/// The segments are live camera video, and the default base is under `/tmp`,
/// which every local user can write to. Left at the usual 0755, any local
/// user could read the video. Worse, one could create the path first, as
/// their own directory or as a symlink, and have segments written wherever
/// they chose. So: mode 0700, and refuse a base that is a symlink or belongs
/// to someone else.
fn secure_base_dir(base: &Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }

    // lstat, so a symlink is seen as one and not as what it points at.
    let meta = std::fs::symlink_metadata(base)?;
    if !meta.file_type().is_dir() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "the HLS storage path is a symlink or not a directory; \
             set streaming.hls.storage.path to a real directory",
        ));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "the HLS storage path belongs to another user; \
             remove it or set streaming.hls.storage.path elsewhere",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Storage errors
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("IO error for path {path:?}: {source}")]
    IoError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Segment not found: monitor={monitor_id}, sequence={sequence}")]
    NotFound { monitor_id: u32, sequence: u64 },

    #[error("Storage not initialized for monitor {0}")]
    NotInitialized(u32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_storage() -> (HlsStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = StorageConfig {
            base_path: temp_dir.path().to_path_buf(),
            retention: Duration::from_secs(60),
            cleanup_interval: Duration::from_secs(10),
            max_segments_per_stream: 10,
        };
        (HlsStorage::new(config), temp_dir)
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn storage_at(base: PathBuf) -> HlsStorage {
        HlsStorage::new(StorageConfig {
            base_path: base,
            ..StorageConfig::default()
        })
    }

    /// Live video must not be readable by other local users.
    #[tokio::test]
    async fn the_base_directory_is_created_private() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("hls");
        storage_at(base.clone()).init_monitor(1).await.unwrap();
        assert_eq!(mode_of(&base), 0o700);
    }

    #[tokio::test]
    async fn an_existing_open_base_directory_is_closed() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let base = temp.path().join("hls");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        storage_at(base.clone()).init_monitor(1).await.unwrap();
        assert_eq!(mode_of(&base), 0o700);
    }

    /// A symlink planted at the path would send segments wherever it points.
    #[tokio::test]
    async fn a_symlinked_base_directory_is_refused() {
        let temp = TempDir::new().unwrap();
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let base = temp.path().join("hls");
        std::os::unix::fs::symlink(&elsewhere, &base).unwrap();

        let storage = storage_at(base);
        assert!(storage.init_monitor(1).await.is_err());
        assert!(storage.store_init_segment(1, b"x").await.is_err());
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing may be written through the symlink"
        );
    }

    #[tokio::test]
    async fn test_init_monitor() {
        let (storage, _temp) = create_test_storage().await;

        let path = storage.init_monitor(1).await.unwrap();
        assert!(path.exists());
        assert!(path.ends_with("1"));
    }

    #[tokio::test]
    async fn test_store_init_segment() {
        let (storage, _temp) = create_test_storage().await;

        let data = b"fake init segment data";
        let path = storage.store_init_segment(1, data).await.unwrap();

        assert!(path.exists());
        assert!(path.ends_with("init.mp4"));

        // Verify cached
        let cached = storage.get_init_segment(1).await.unwrap();
        assert_eq!(cached, data);
    }

    #[tokio::test]
    async fn test_store_and_read_segment() {
        let (storage, _temp) = create_test_storage().await;

        let data = b"segment data";
        let duration = Duration::from_secs(4);

        let info = storage.store_segment(1, 0, data, duration).await.unwrap();

        assert_eq!(info.sequence, 0);
        assert_eq!(info.size, data.len() as u64);
        assert_eq!(info.duration, duration);

        // Read back
        let read_data = storage.read_segment(1, 0).await.unwrap();
        assert_eq!(read_data, data);
    }

    #[tokio::test]
    async fn test_list_segments() {
        let (storage, _temp) = create_test_storage().await;

        for i in 0..5 {
            storage
                .store_segment(1, i, b"data", Duration::from_secs(4))
                .await
                .unwrap();
        }

        let segments = storage.list_segments(1).await;
        assert_eq!(segments.len(), 5);

        // Should be sorted by sequence
        for (i, seg) in segments.iter().enumerate() {
            assert_eq!(seg.sequence, i as u64);
        }
    }

    #[tokio::test]
    async fn test_store_segment_enforces_cap_atomically() {
        // REVIEW_FIXES_PLAN §3.3: the in-memory segment map must never exceed
        // max_segments_per_stream once inserts push past the cap — eviction is
        // now performed under the insert's write lock, not only on the slow
        // interval path.
        let (storage, _temp) = create_test_storage().await; // cap = 10
        for i in 0..25 {
            storage
                .store_segment(1, i, b"data", Duration::from_secs(4))
                .await
                .unwrap();
        }

        let segments = storage.list_segments(1).await;
        assert_eq!(segments.len(), 10, "segment count must be capped");
        // The newest 10 (sequences 15..=24) must be the survivors.
        assert_eq!(segments.first().unwrap().sequence, 15);
        assert_eq!(segments.last().unwrap().sequence, 24);

        // Evicted segment files must be gone from disk, survivors present.
        assert!(storage.read_segment(1, 14).await.is_err());
        assert!(storage.read_segment(1, 24).await.is_ok());
    }

    #[tokio::test]
    async fn test_get_latest_segments() {
        let (storage, _temp) = create_test_storage().await;

        for i in 0..10 {
            storage
                .store_segment(1, i, b"data", Duration::from_secs(4))
                .await
                .unwrap();
        }

        let latest = storage.get_latest_segments(1, 3).await;
        assert_eq!(latest.len(), 3);
        assert_eq!(latest[0].sequence, 7);
        assert_eq!(latest[1].sequence, 8);
        assert_eq!(latest[2].sequence, 9);
    }

    #[tokio::test]
    async fn test_storage_stats() {
        let (storage, _temp) = create_test_storage().await;

        storage.store_init_segment(1, b"init").await.unwrap();

        for i in 0..3 {
            storage
                .store_segment(1, i, &vec![0u8; 1000], Duration::from_secs(4))
                .await
                .unwrap();
        }

        let stats = storage.get_stats(1).await.unwrap();
        assert_eq!(stats.segment_count, 3);
        assert_eq!(stats.total_size, 3000);
        assert_eq!(stats.total_duration, Duration::from_secs(12));
        assert!(stats.has_init_segment);
        assert_eq!(stats.oldest_sequence, Some(0));
        assert_eq!(stats.newest_sequence, Some(2));
    }

    #[tokio::test]
    async fn test_clean_monitor_removes_stale_files() {
        let (storage, _temp) = create_test_storage().await;

        // Create a session with init + segments
        storage.store_init_segment(1, b"init").await.unwrap();
        storage
            .store_segment(1, 0, b"seg0", Duration::from_secs(4))
            .await
            .unwrap();
        storage
            .store_segment(1, 1, b"seg1", Duration::from_secs(4))
            .await
            .unwrap();

        let monitor_path = storage.init_monitor(1).await.unwrap();
        assert!(monitor_path.join("init.mp4").exists());
        assert!(monitor_path.join("segment_00000.m4s").exists());
        assert!(monitor_path.join("segment_00001.m4s").exists());

        // Clean should remove files but keep directory
        storage.clean_monitor(1).await.unwrap();
        assert!(monitor_path.exists());
        assert!(!monitor_path.join("init.mp4").exists());
        assert!(!monitor_path.join("segment_00000.m4s").exists());
        assert!(!monitor_path.join("segment_00001.m4s").exists());

        // In-memory state should be cleared
        assert!(storage.get_init_segment(1).await.is_none());
        let segments = storage.list_segments(1).await;
        assert!(segments.is_empty());
    }

    #[tokio::test]
    async fn test_clean_monitor_nonexistent_is_ok() {
        let (storage, _temp) = create_test_storage().await;

        // Should not error for a monitor that was never initialized
        storage.clean_monitor(999).await.unwrap();
    }

    #[tokio::test]
    async fn test_remove_monitor() {
        let (storage, _temp) = create_test_storage().await;

        storage.store_init_segment(1, b"init").await.unwrap();
        storage
            .store_segment(1, 0, b"data", Duration::from_secs(4))
            .await
            .unwrap();

        let path = storage.init_monitor(1).await.unwrap();
        assert!(path.exists());

        storage.remove_monitor(1).await.unwrap();
        assert!(!path.exists());
    }
}
