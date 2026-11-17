//! ZoneMinder shared memory interface.
//!
//! This module provides native Rust access to ZoneMinder's shared memory structures,
//! allowing direct communication with zmc/zma daemons without shelling out to `zmu`.
//!
//! # Shared Memory Layout
//!
//! ZoneMinder uses memory-mapped files (typically in `/dev/shm/`) for IPC between
//! its daemons. Each monitor has its own shared memory segment containing:
//!
//! - `SharedData`: Monitor state, FPS, timestamps, settings. Its size depends
//!   on the ZoneMinder version (872 bytes on 1.38, 888 on 1.39/master, which
//!   appended fields at the end); zmc records it in the struct's first field.
//! - `TriggerData` (560 bytes): External trigger state for forcing alarms. It
//!   starts right after `SharedData`, so its offset is `SharedData.size`.
//! - `VideoStoreData` (4104 bytes): Video recording state
//!
//! zm-api reads only the first 872 bytes of `SharedData` (the layout every
//! supported version shares) and finds `TriggerData` from the `size` zmc
//! wrote, refusing a segment whose sizes it does not recognise rather than
//! writing at a guessed offset.
//!
//! # Usage
//!
//! ```ignore
//! use zm_api::zm_shm::{MonitorShm, TriggerState};
//!
//! // Connect to monitor's shared memory
//! let shm = MonitorShm::connect(1)?;
//!
//! // Read current state
//! let state = shm.get_state();
//! let fps = shm.get_capture_fps();
//!
//! // Trigger an alarm
//! shm.trigger_alarm(100, "API", "Triggered via REST API")?;
//!
//! // Cancel the alarm
//! shm.cancel_alarm()?;
//! ```

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use memmap2::MmapMut;
use thiserror::Error;
use tracing::debug;

/// Default base path for ZoneMinder shared memory files.
pub const DEFAULT_SHM_PATH: &str = "/dev/shm";

/// Default shared memory file prefix.
pub const DEFAULT_SHM_PREFIX: &str = "zm.mmap";

/// How stale zmc's heartbeat may be before its audio reading counts as gone.
/// The same window the event stream uses for "zmc is running".
const LIVE_AUDIO_MAX_HEARTBEAT_AGE: Duration = Duration::from_secs(10);

/// Errors that can occur when accessing ZoneMinder shared memory.
#[derive(Debug, Error)]
pub enum ShmError {
    #[error("Monitor {0} shared memory not found at {1}")]
    NotFound(u32, PathBuf),

    #[error("Failed to open shared memory file: {0}")]
    OpenFailed(#[from] std::io::Error),

    #[error("Shared memory is invalid or monitor not running")]
    Invalid,

    #[error("Monitor {0} heartbeat stale (last: {1}s ago, max: {2}s)")]
    HeartbeatStale(u32, u64, u64),

    #[error("Shared memory size mismatch: expected at least {expected}, got {actual}")]
    SizeMismatch { expected: usize, actual: usize },

    #[error(
        "Monitor {monitor_id} shared memory has an unsupported layout: \
         SharedData.size {shared_data}, TriggerData.size {trigger_data} \
         (zm-api reads SharedData of at least {SHARED_DATA_SIZE} bytes and a \
         {TRIGGER_DATA_SIZE}-byte TriggerData)"
    )]
    UnsupportedLayout {
        monitor_id: u32,
        shared_data: usize,
        trigger_data: usize,
    },

    #[error("String too long: {field} max {max} bytes, got {actual}")]
    StringTooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
}

pub type Result<T> = std::result::Result<T, ShmError>;

/// Monitor state as stored in shared memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    Unknown = 0,
    Idle = 1,
    PreAlarm = 2,
    Alarm = 3,
    Alert = 4,
}

impl From<u32> for State {
    fn from(value: u32) -> Self {
        match value {
            0 => State::Unknown,
            1 => State::Idle,
            2 => State::PreAlarm,
            3 => State::Alarm,
            4 => State::Alert,
            _ => State::Unknown,
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Unknown => write!(f, "Unknown"),
            State::Idle => write!(f, "Idle"),
            State::PreAlarm => write!(f, "PreAlarm"),
            State::Alarm => write!(f, "Alarm"),
            State::Alert => write!(f, "Alert"),
        }
    }
}

/// Trigger state for external alarm control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TriggerState {
    Cancel = 0,
    On = 1,
    Off = 2,
}

impl From<u32> for TriggerState {
    fn from(value: u32) -> Self {
        match value {
            0 => TriggerState::Cancel,
            1 => TriggerState::On,
            2 => TriggerState::Off,
            _ => TriggerState::Cancel,
        }
    }
}

/// Monitor statistics read from shared memory.
#[derive(Debug, Clone)]
pub struct MonitorStats {
    pub monitor_id: u32,
    pub state: State,
    pub trigger_state: TriggerState,
    pub capture_fps: f64,
    pub analysis_fps: f64,
    pub last_event_id: u64,
    pub last_frame_score: u32,
    pub is_capturing: bool,
    pub is_analysing: bool,
    pub is_recording: bool,
    pub has_signal: bool,
    pub alarm_x: i32,
    pub alarm_y: i32,
    pub alarm_cause: String,
    pub image_count: i32,
    pub last_write_index: i32,
    pub last_read_index: i32,
}

/// Shared memory layout for monitor data: the part of the C++
/// `Monitor::SharedData` struct that every supported ZoneMinder version
/// shares. 1.38's struct is exactly this; 1.39 renamed some reserved fields
/// (`audio_level`/`audio_alarm` at +98, `stream_socket_path` at +680) and
/// appended 16 bytes after `janus_pin`, so its struct is 888 bytes. The real
/// size is read from `size` at runtime ([`MonitorShm::connect_with_path`]).
/// Uses `#[repr(C)]` with explicit padding to match C++ alignment.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct SharedData {
    size: u32,                      // +0
    last_write_index: i32,          // +4
    last_read_index: i32,           // +8
    image_count: i32,               // +12
    state: u32,                     // +16
    _pad1: u32,                     // +20 (padding for f64 alignment)
    capture_fps: f64,               // +24
    analysis_fps: f64,              // +32
    latitude: f64,                  // +40
    longitude: f64,                 // +48
    last_event_id: u64,             // +56
    action: u32,                    // +64
    brightness: i32,                // +68
    hue: i32,                       // +72
    colour: i32,                    // +76
    contrast: i32,                  // +80
    alarm_x: i32,                   // +84
    alarm_y: i32,                   // +88
    valid: u8,                      // +92
    capturing: u8,                  // +93
    analysing: u8,                  // +94
    recording: u8,                  // +95
    signal: u8,                     // +96
    format: u8,                     // +97
    audio_level: u8,                // +98 (1.39; reserved on 1.38) 0-100
    audio_alarm: u8,                // +99 (1.39) audio_level crossed AudioThreshold
    imagesize: u32,                 // +100
    last_frame_score: u32,          // +104
    audio_frequency: u32,           // +108
    audio_channels: u32,            // +112
    _pad2: u32,                     // +116 (padding for i64 alignment)
    startup_time: i64,              // +120
    heartbeat_time: i64,            // +128
    last_write_time: i64,           // +136
    last_read_time: i64,            // +144
    last_viewed_time: i64,          // +152
    last_analysis_viewed_time: i64, // +160
    control_state: [u8; 256],       // +168
    alarm_cause: [u8; 256],         // +424
    // Formerly video_fifo_path/audio_fifo_path. The media FIFOs were
    // replaced by the per-monitor stream socket (its path is the documented
    // convention PATH_SOCKS/stream_{monitor_id}.sock, not published via
    // shm); zmc keeps these as zeroed reserved padding so the layout and
    // total size are unchanged.
    _reserved_path1: [u8; 64], // +680
    _reserved_path2: [u8; 64], // +744
    janus_pin: [u8; 64],       // +808
                               // = 872 (1.39 appends 16 more bytes here)
}

/// Bytes of `SharedData` zm-api reads: the shared prefix, and the whole
/// struct on ZoneMinder 1.38. Not where `TriggerData` starts on every
/// version; that is `SharedData.size` (see [`trigger_offset`]).
const SHARED_DATA_SIZE: usize = 872;

// Verify struct size at compile time
const _: () = assert!(
    std::mem::size_of::<SharedData>() == SHARED_DATA_SIZE,
    "SharedData size mismatch"
);

/// Trigger data layout for external alarm triggering.
///
/// This must match the C++ `Monitor::TriggerData` struct exactly.
/// Total size: 560 bytes
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct TriggerData {
    size: u32,               // +0
    trigger_state: u32,      // +4 (TriggerState enum)
    trigger_score: u32,      // +8
    padding: u32,            // +12
    trigger_cause: [u8; 32], // +16
    trigger_text: [u8; 256], // +48
    trigger_showtext: [u8; 256], // +304
                             // = 560 total
}

const TRIGGER_DATA_SIZE: usize = 560;

// Verify struct size at compile time
const _: () = assert!(
    std::mem::size_of::<TriggerData>() == TRIGGER_DATA_SIZE,
    "TriggerData size mismatch"
);

/// Video store data layout.
///
/// Total size: ~4128 bytes (may vary by platform)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
#[allow(dead_code)]
struct VideoStoreData {
    size: u32,              // +0
    _padding: u32,          // +4 (alignment for u64)
    current_event: u64,     // +8
    event_file: [u8; 4096], // +16
    recording_sec: i64,     // +4112 (timeval.tv_sec)
    recording_usec: i64,    // +4120 (timeval.tv_usec)
                            // = 4128 total (may vary)
}

/// Minimum total shared memory size.
/// SharedData + TriggerData + VideoStoreData + zone_scores
const MIN_SHM_SIZE: usize = SHARED_DATA_SIZE + TRIGGER_DATA_SIZE;

/// Where `TriggerData` starts in a monitor's segment: right after
/// `SharedData`, whose size zmc writes into its first field
/// (`shared_data->size = sizeof(SharedData)`). Checks the `size` zmc wrote
/// into `TriggerData` as well, so a layout zm-api doesn't know is refused
/// instead of written at a guessed offset.
///
/// Returns `Ok(None)` when the segment isn't initialised yet (size 0).
fn trigger_offset(monitor_id: u32, shm: &[u8]) -> Result<Option<usize>> {
    let read_u32 = |at: usize| -> Option<usize> {
        shm.get(at..at + 4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let shared_data = read_u32(0).unwrap_or(0);
    if shared_data == 0 {
        return Ok(None);
    }
    let trigger_data = if shared_data >= SHARED_DATA_SIZE && shared_data % 8 == 0 {
        read_u32(shared_data).unwrap_or(0)
    } else {
        0
    };
    if trigger_data != TRIGGER_DATA_SIZE || shared_data + TRIGGER_DATA_SIZE > shm.len() {
        return Err(ShmError::UnsupportedLayout {
            monitor_id,
            shared_data,
            trigger_data,
        });
    }
    Ok(Some(shared_data))
}

/// Handle to a monitor's shared memory.
pub struct MonitorShm {
    monitor_id: u32,
    mmap: MmapMut,
    shm_path: PathBuf,
    /// Offset of `TriggerData`: the `SharedData.size` zmc wrote.
    trigger_offset: usize,
}

impl MonitorShm {
    /// Connect to a monitor's shared memory.
    ///
    /// # Arguments
    ///
    /// * `monitor_id` - The monitor ID to connect to
    ///
    /// # Returns
    ///
    /// A handle to the monitor's shared memory, or an error if the monitor
    /// is not running or the shared memory cannot be accessed.
    pub fn connect(monitor_id: u32) -> Result<Self> {
        Self::connect_with_path(monitor_id, DEFAULT_SHM_PATH, DEFAULT_SHM_PREFIX)
    }

    /// Connect to a monitor's shared memory with custom path.
    ///
    /// # Arguments
    ///
    /// * `monitor_id` - The monitor ID to connect to
    /// * `shm_base_path` - Base path for shared memory files (e.g., "/dev/shm")
    /// * `shm_prefix` - Prefix for shared memory files (e.g., "zm.mmap")
    pub fn connect_with_path(
        monitor_id: u32,
        shm_base_path: &str,
        shm_prefix: &str,
    ) -> Result<Self> {
        let shm_path = PathBuf::from(shm_base_path).join(format!("{}.{}", shm_prefix, monitor_id));

        if !shm_path.exists() {
            return Err(ShmError::NotFound(monitor_id, shm_path));
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&shm_path)?;

        let metadata = file.metadata()?;
        let file_size = metadata.len() as usize;

        if file_size < MIN_SHM_SIZE {
            return Err(ShmError::SizeMismatch {
                expected: MIN_SHM_SIZE,
                actual: file_size,
            });
        }

        // SAFETY: We've verified the file exists and has sufficient size.
        // The shared memory layout is well-defined by ZoneMinder.
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        // An uninitialised segment (zmc starting or gone) reads as not valid.
        let Some(trigger_offset) = trigger_offset(monitor_id, &mmap)? else {
            return Err(ShmError::Invalid);
        };

        let shm = Self {
            monitor_id,
            mmap,
            shm_path,
            trigger_offset,
        };

        // Verify the shared memory is valid
        if !shm.is_valid() {
            return Err(ShmError::Invalid);
        }

        debug!(
            "Connected to monitor {} shared memory at {:?}",
            monitor_id, shm.shm_path
        );

        Ok(shm)
    }

    /// Get a reference to the SharedData structure.
    fn shared_data(&self) -> &SharedData {
        // SAFETY: We verified size in connect() and SharedData is repr(C)
        unsafe { &*(self.mmap.as_ptr() as *const SharedData) }
    }

    /// Get a mutable reference to the SharedData structure.
    #[allow(dead_code)]
    fn shared_data_mut(&mut self) -> &mut SharedData {
        // SAFETY: We verified size in connect() and SharedData is repr(C)
        unsafe { &mut *(self.mmap.as_mut_ptr() as *mut SharedData) }
    }

    /// Get a reference to the TriggerData structure.
    fn trigger_data(&self) -> &TriggerData {
        // SAFETY: TriggerData follows SharedData in memory; connect() checked
        // that trigger_offset + TRIGGER_DATA_SIZE lies within the mapping.
        unsafe {
            let ptr = self.mmap.as_ptr().add(self.trigger_offset);
            &*(ptr as *const TriggerData)
        }
    }

    /// Get a mutable reference to the TriggerData structure.
    fn trigger_data_mut(&mut self) -> &mut TriggerData {
        // SAFETY: as in trigger_data().
        unsafe {
            let ptr = self.mmap.as_mut_ptr().add(self.trigger_offset);
            &mut *(ptr as *mut TriggerData)
        }
    }

    /// Check if the shared memory is valid (monitor is running).
    pub fn is_valid(&self) -> bool {
        self.shared_data().valid != 0
    }

    /// Check if the monitor is alive by verifying its heartbeat.
    ///
    /// # Arguments
    ///
    /// * `max_delay` - Maximum allowed time since last heartbeat
    ///
    /// # Returns
    ///
    /// `true` if the monitor's heartbeat is recent enough, `false` otherwise.
    pub fn is_alive(&self, max_delay: Duration) -> bool {
        if !self.is_valid() {
            return false;
        }

        let heartbeat = self.shared_data().heartbeat_time;
        if heartbeat == 0 {
            return false;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let elapsed = now - heartbeat;
        elapsed >= 0 && (elapsed as u64) < max_delay.as_secs()
    }

    /// Get the current monitor state.
    pub fn get_state(&self) -> State {
        State::from(self.shared_data().state)
    }

    /// Get the current trigger state.
    pub fn get_trigger_state(&self) -> TriggerState {
        TriggerState::from(self.trigger_data().trigger_state)
    }

    /// Get the current capture FPS.
    pub fn get_capture_fps(&self) -> f64 {
        self.shared_data().capture_fps
    }

    /// Get the current analysis FPS.
    pub fn get_analysis_fps(&self) -> f64 {
        self.shared_data().analysis_fps
    }

    /// Get the last event ID.
    pub fn get_last_event_id(&self) -> u64 {
        self.shared_data().last_event_id
    }

    /// Get the last frame score.
    pub fn get_last_frame_score(&self) -> u32 {
        self.shared_data().last_frame_score
    }

    /// Check if the monitor is currently capturing.
    pub fn is_capturing(&self) -> bool {
        self.shared_data().capturing != 0
    }

    /// Check if the monitor is currently analysing.
    pub fn is_analysing(&self) -> bool {
        self.shared_data().analysing != 0
    }

    /// Check if the monitor is currently recording.
    pub fn is_recording(&self) -> bool {
        self.shared_data().recording != 0
    }

    /// Check if the monitor has signal.
    pub fn has_signal(&self) -> bool {
        self.shared_data().signal != 0
    }

    /// Get the alarm location (x, y coordinates).
    pub fn get_alarm_location(&self) -> (i32, i32) {
        let sd = self.shared_data();
        (sd.alarm_x, sd.alarm_y)
    }

    /// Get the alarm cause string.
    pub fn get_alarm_cause(&self) -> String {
        bytes_to_string(&self.shared_data().alarm_cause)
    }

    /// Get the heartbeat timestamp.
    pub fn get_heartbeat_time(&self) -> Option<SystemTime> {
        let ts = self.shared_data().heartbeat_time;
        if ts > 0 {
            Some(UNIX_EPOCH + Duration::from_secs(ts as u64))
        } else {
            None
        }
    }

    /// Get comprehensive monitor statistics.
    pub fn get_stats(&self) -> MonitorStats {
        let sd = self.shared_data();
        let td = self.trigger_data();

        MonitorStats {
            monitor_id: self.monitor_id,
            state: State::from(sd.state),
            trigger_state: TriggerState::from(td.trigger_state),
            capture_fps: sd.capture_fps,
            analysis_fps: sd.analysis_fps,
            last_event_id: sd.last_event_id,
            last_frame_score: sd.last_frame_score,
            is_capturing: sd.capturing != 0,
            is_analysing: sd.analysing != 0,
            is_recording: sd.recording != 0,
            has_signal: sd.signal != 0,
            alarm_x: sd.alarm_x,
            alarm_y: sd.alarm_y,
            alarm_cause: bytes_to_string(&sd.alarm_cause),
            image_count: sd.image_count,
            last_write_index: sd.last_write_index,
            last_read_index: sd.last_read_index,
        }
    }

    /// Trigger an alarm on the monitor.
    ///
    /// This sets the trigger state to ON and configures the alarm parameters.
    /// The zmc/zma daemons will pick up this change and create an event.
    ///
    /// # Arguments
    ///
    /// * `score` - Alarm score (higher = more severe)
    /// * `cause` - Short cause string (max 31 chars)
    /// * `text` - Longer description text (max 255 chars)
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, or an error if strings are too long.
    pub fn trigger_alarm(&mut self, score: u32, cause: &str, text: &str) -> Result<()> {
        self.trigger_alarm_with_showtext(score, cause, text, text)
    }

    /// Trigger an alarm with separate show text.
    ///
    /// # Arguments
    ///
    /// * `score` - Alarm score (higher = more severe)
    /// * `cause` - Short cause string (max 31 chars)
    /// * `text` - Longer description text (max 255 chars)
    /// * `showtext` - Text to display on video (max 255 chars)
    pub fn trigger_alarm_with_showtext(
        &mut self,
        score: u32,
        cause: &str,
        text: &str,
        showtext: &str,
    ) -> Result<()> {
        // Validate string lengths
        if cause.len() > 31 {
            return Err(ShmError::StringTooLong {
                field: "cause",
                max: 31,
                actual: cause.len(),
            });
        }
        if text.len() > 255 {
            return Err(ShmError::StringTooLong {
                field: "text",
                max: 255,
                actual: text.len(),
            });
        }
        if showtext.len() > 255 {
            return Err(ShmError::StringTooLong {
                field: "showtext",
                max: 255,
                actual: showtext.len(),
            });
        }

        let td = self.trigger_data_mut();

        // Score, cause and texts before the state: the analysis thread reads
        // them once it sees TRIGGER_ON (ZoneMinder's Monitor::ForceAlarmOn
        // writes in this order for the same reason).
        td.trigger_score = score;

        // Copy cause string
        td.trigger_cause.fill(0);
        td.trigger_cause[..cause.len()].copy_from_slice(cause.as_bytes());

        // Copy text string
        td.trigger_text.fill(0);
        td.trigger_text[..text.len()].copy_from_slice(text.as_bytes());

        // Copy showtext string
        td.trigger_showtext.fill(0);
        td.trigger_showtext[..showtext.len()].copy_from_slice(showtext.as_bytes());

        // Set trigger state to ON, after the writes above (a release fence:
        // neither the compiler nor the CPU moves them past it)
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        td.trigger_state = TriggerState::On as u32;

        // Ensure changes are flushed to shared memory
        self.mmap.flush()?;

        debug!(
            "Triggered alarm on monitor {}: score={}, cause={}",
            self.monitor_id, score, cause
        );

        Ok(())
    }

    /// Cancel a triggered alarm.
    ///
    /// This sets the trigger state to CANCEL, which tells zmc/zma to
    /// stop the forced alarm state.
    pub fn cancel_alarm(&mut self) -> Result<()> {
        let td = self.trigger_data_mut();
        td.trigger_state = TriggerState::Cancel as u32;
        td.trigger_score = 0;
        td.trigger_cause.fill(0);
        td.trigger_text.fill(0);
        td.trigger_showtext.fill(0);

        self.mmap.flush()?;

        debug!("Cancelled alarm on monitor {}", self.monitor_id);

        Ok(())
    }

    /// Turn off alarm triggering (different from cancel).
    ///
    /// TRIGGER_OFF means "don't respond to triggers" whereas
    /// TRIGGER_CANCEL means "cancel current trigger and return to normal".
    pub fn disable_triggers(&mut self) -> Result<()> {
        let td = self.trigger_data_mut();
        td.trigger_state = TriggerState::Off as u32;

        self.mmap.flush()?;

        debug!("Disabled triggers on monitor {}", self.monitor_id);

        Ok(())
    }

    /// The audio level and alarm while zmc is running, else `None`. A
    /// segment a killed zmc left behind still reads as valid, so the
    /// heartbeat has to be recent too (#193).
    pub fn live_audio(&self) -> Option<(u8, bool)> {
        self.is_alive(LIVE_AUDIO_MAX_HEARTBEAT_AGE)
            .then(|| (self.get_audio_level(), self.get_audio_alarm()))
    }

    /// The audio level zmc last published, 0-100 (#180). Zero is a real
    /// reading: silence, or nothing measured.
    pub fn get_audio_level(&self) -> u8 {
        self.shared_data().audio_level
    }

    /// Whether the audio level is over the monitor's `AudioThreshold`.
    pub fn get_audio_alarm(&self) -> bool {
        self.shared_data().audio_alarm != 0
    }

    /// Ask zmc to keep measuring the audio level until the unix time `until`,
    /// even with AudioDetection off: what ZoneMinder's monitor editor does
    /// while its level meter is on screen (`audio_level_until`, 1.39's
    /// 888-byte `SharedData` at +880). Returns false, writing nothing, on a
    /// layout without the field.
    pub fn request_audio_level(&mut self, until: u32) -> Result<bool> {
        const AUDIO_LEVEL_UNTIL: usize = 880;
        if self.trigger_offset < AUDIO_LEVEL_UNTIL + 4 {
            return Ok(false);
        }
        self.mmap[AUDIO_LEVEL_UNTIL..AUDIO_LEVEL_UNTIL + 4].copy_from_slice(&until.to_ne_bytes());
        self.mmap.flush()?;
        Ok(true)
    }

    /// Get the path to the shared memory file.
    pub fn shm_path(&self) -> &PathBuf {
        &self.shm_path
    }

    /// Get the monitor ID.
    pub fn monitor_id(&self) -> u32 {
        self.monitor_id
    }
}

impl std::fmt::Debug for MonitorShm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorShm")
            .field("monitor_id", &self.monitor_id)
            .field("shm_path", &self.shm_path)
            .field("is_valid", &self.is_valid())
            .field("state", &self.get_state())
            .finish()
    }
}

/// Convert a null-terminated byte array to a String.
fn bytes_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

// ============================================================================
// Convenience functions for one-off operations
// ============================================================================

/// Get the current state of a monitor.
///
/// This is a convenience function that connects, reads the state, and disconnects.
/// For multiple operations, use `MonitorShm::connect()` directly.
pub fn get_monitor_state(monitor_id: u32) -> Result<State> {
    let shm = MonitorShm::connect(monitor_id)?;
    Ok(shm.get_state())
}

/// Get comprehensive stats for a monitor.
pub fn get_monitor_stats(monitor_id: u32) -> Result<MonitorStats> {
    let shm = MonitorShm::connect(monitor_id)?;
    Ok(shm.get_stats())
}

/// Check if a monitor is running (has valid shared memory and recent heartbeat).
pub fn is_monitor_running(monitor_id: u32, max_heartbeat_age: Duration) -> Result<bool> {
    match MonitorShm::connect(monitor_id) {
        Ok(shm) => Ok(shm.is_alive(max_heartbeat_age)),
        Err(ShmError::NotFound(_, _)) => Ok(false),
        Err(ShmError::Invalid) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Trigger an alarm on a monitor.
pub fn trigger_alarm(monitor_id: u32, score: u32, cause: &str, text: &str) -> Result<()> {
    let mut shm = MonitorShm::connect(monitor_id)?;
    shm.trigger_alarm(score, cause, text)
}

/// Cancel a triggered alarm on a monitor.
pub fn cancel_alarm(monitor_id: u32) -> Result<()> {
    let mut shm = MonitorShm::connect(monitor_id)?;
    shm.cancel_alarm()
}

/// Get the trigger state of a monitor.
pub fn get_trigger_state(monitor_id: u32) -> Result<TriggerState> {
    let shm = MonitorShm::connect(monitor_id)?;
    Ok(shm.get_trigger_state())
}

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for shared memory access.
#[derive(Debug, Clone)]
pub struct ShmConfig {
    /// Base path for shared memory files (default: "/dev/shm")
    pub shm_path: String,
    /// Prefix for shared memory files (default: "zm.mmap")
    pub shm_prefix: String,
    /// Maximum heartbeat age before considering a monitor dead
    pub max_heartbeat_age: Duration,
}

impl Default for ShmConfig {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_SHM_PATH.to_string(),
            shm_prefix: DEFAULT_SHM_PREFIX.to_string(),
            max_heartbeat_age: Duration::from_secs(60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_data_size() {
        assert_eq!(std::mem::size_of::<SharedData>(), SHARED_DATA_SIZE);
    }

    #[test]
    fn test_trigger_data_size() {
        assert_eq!(std::mem::size_of::<TriggerData>(), TRIGGER_DATA_SIZE);
    }

    #[test]
    fn test_state_conversion() {
        assert_eq!(State::from(0), State::Unknown);
        assert_eq!(State::from(1), State::Idle);
        assert_eq!(State::from(2), State::PreAlarm);
        assert_eq!(State::from(3), State::Alarm);
        assert_eq!(State::from(4), State::Alert);
        assert_eq!(State::from(99), State::Unknown);
    }

    #[test]
    fn test_trigger_state_conversion() {
        assert_eq!(TriggerState::from(0), TriggerState::Cancel);
        assert_eq!(TriggerState::from(1), TriggerState::On);
        assert_eq!(TriggerState::from(2), TriggerState::Off);
        assert_eq!(TriggerState::from(99), TriggerState::Cancel);
    }

    #[test]
    fn test_bytes_to_string() {
        let bytes = b"hello\0world";
        assert_eq!(bytes_to_string(bytes), "hello");

        let bytes = b"no null";
        assert_eq!(bytes_to_string(bytes), "no null");

        let bytes = b"\0";
        assert_eq!(bytes_to_string(bytes), "");
    }

    #[test]
    fn test_shm_config_default() {
        let config = ShmConfig::default();
        assert_eq!(config.shm_path, "/dev/shm");
        assert_eq!(config.shm_prefix, "zm.mmap");
        assert_eq!(config.max_heartbeat_age, Duration::from_secs(60));
    }

    /// A segment as zmc lays it out: `SharedData` of `shared_data` bytes with
    /// its `size` and `valid` set, then a `TriggerData` with its `size`, then
    /// some of the rest of the segment.
    fn fake_segment(dir: &std::path::Path, id: u32, shared_data: u32, trigger_data: u32) {
        let mut buf = vec![0u8; shared_data as usize + 4096];
        buf[0..4].copy_from_slice(&shared_data.to_ne_bytes());
        buf[92] = 1; // valid
        let t = shared_data as usize;
        buf[t..t + 4].copy_from_slice(&trigger_data.to_ne_bytes());
        std::fs::write(dir.join(format!("zm.mmap.{id}")), buf).unwrap();
    }

    fn read_segment(dir: &std::path::Path, id: u32) -> Vec<u8> {
        std::fs::read(dir.join(format!("zm.mmap.{id}"))).unwrap()
    }

    /// ZoneMinder 1.39/master: SharedData is 888 bytes, so TriggerData starts
    /// at 888. Writing at 872 put the trigger 16 bytes early, over the end of
    /// SharedData and onto TriggerData.size (zmwatch then restarted zmc).
    #[test]
    fn trigger_lands_after_an_888_byte_shared_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        fake_segment(dir.path(), 1, 888, 560);
        let mut shm = MonitorShm::connect_with_path(1, p, "zm.mmap").unwrap();
        shm.trigger_alarm(100, "zmapi", "text").unwrap();
        assert_eq!(shm.get_trigger_state(), TriggerState::On);
        drop(shm);

        let buf = read_segment(dir.path(), 1);
        let td = |at: usize| u32::from_ne_bytes(buf[888 + at..888 + at + 4].try_into().unwrap());
        assert_eq!(td(0), 560, "TriggerData.size untouched");
        assert_eq!(td(4), TriggerState::On as u32);
        assert_eq!(td(8), 100);
        assert_eq!(&buf[888 + 16..888 + 21], b"zmapi");
        assert!(
            buf[872..888].iter().all(|&b| b == 0),
            "SharedData tail untouched"
        );
    }

    /// The audio level and alarm are read at +98/+99, and the measuring
    /// request lands at +880 on 1.39 and is refused on 1.38's shorter struct
    /// (#180).
    #[test]
    fn audio_level_is_read_and_requested_where_zmc_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        fake_segment(dir.path(), 6, 888, 560);
        let path = dir.path().join("zm.mmap.6");
        let mut buf = std::fs::read(&path).unwrap();
        buf[98] = 42;
        buf[99] = 1;
        std::fs::write(&path, &buf).unwrap();
        let mut shm = MonitorShm::connect_with_path(6, p, "zm.mmap").unwrap();
        assert_eq!(shm.get_audio_level(), 42);
        assert!(shm.get_audio_alarm());
        assert!(shm.request_audio_level(1_760_000_010).unwrap());
        drop(shm);
        let buf = read_segment(dir.path(), 6);
        assert_eq!(
            u32::from_ne_bytes(buf[880..884].try_into().unwrap()),
            1_760_000_010
        );
        assert_eq!(
            u32::from_ne_bytes(buf[888..892].try_into().unwrap()),
            560,
            "TriggerData untouched"
        );

        fake_segment(dir.path(), 7, 872, 560);
        let mut old = MonitorShm::connect_with_path(7, p, "zm.mmap").unwrap();
        assert!(!old.request_audio_level(1).unwrap());
        drop(old);
        assert_eq!(
            u32::from_ne_bytes(read_segment(dir.path(), 7)[872..876].try_into().unwrap()),
            560
        );
    }

    fn set_heartbeat(dir: &std::path::Path, id: u32, at: i64) {
        let path = dir.join(format!("zm.mmap.{id}"));
        let mut buf = std::fs::read(&path).unwrap();
        buf[128..136].copy_from_slice(&at.to_ne_bytes());
        std::fs::write(&path, &buf).unwrap();
    }

    /// A segment left by a zmc that's gone has no live audio reading (#193).
    #[test]
    fn live_audio_needs_a_recent_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        fake_segment(dir.path(), 8, 888, 560);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Never beat.
        let shm = MonitorShm::connect_with_path(8, p, "zm.mmap").unwrap();
        assert_eq!(shm.live_audio(), None);
        drop(shm);

        // Last beat a minute ago: zmc was killed and left the segment valid.
        set_heartbeat(dir.path(), 8, now - 60);
        let shm = MonitorShm::connect_with_path(8, p, "zm.mmap").unwrap();
        assert_eq!(shm.live_audio(), None);
        drop(shm);

        set_heartbeat(dir.path(), 8, now);
        let shm = MonitorShm::connect_with_path(8, p, "zm.mmap").unwrap();
        assert_eq!(shm.live_audio(), Some((0, false)));
    }

    /// ZoneMinder 1.38: SharedData is 872 bytes.
    #[test]
    fn trigger_lands_after_an_872_byte_shared_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        fake_segment(dir.path(), 2, 872, 560);
        let mut shm = MonitorShm::connect_with_path(2, p, "zm.mmap").unwrap();
        shm.trigger_alarm(50, "API", "text").unwrap();
        drop(shm);

        let buf = read_segment(dir.path(), 2);
        assert_eq!(u32::from_ne_bytes(buf[872..876].try_into().unwrap()), 560);
        assert_eq!(u32::from_ne_bytes(buf[876..880].try_into().unwrap()), 1);
        assert_eq!(&buf[872 + 16..872 + 19], b"API");
    }

    /// A layout zm-api doesn't recognise is refused, and nothing is written.
    #[test]
    fn unknown_layout_is_refused_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        // TriggerData.size isn't 560 where SharedData.size says it starts.
        fake_segment(dir.path(), 3, 904, 999);
        let before = read_segment(dir.path(), 3);
        match MonitorShm::connect_with_path(3, p, "zm.mmap") {
            Err(ShmError::UnsupportedLayout {
                shared_data,
                trigger_data,
                ..
            }) => assert_eq!((shared_data, trigger_data), (904, 999)),
            other => panic!("expected UnsupportedLayout, got {other:?}"),
        }
        // A SharedData smaller than the part zm-api reads.
        fake_segment(dir.path(), 4, 864, 560);
        assert!(matches!(
            MonitorShm::connect_with_path(4, p, "zm.mmap"),
            Err(ShmError::UnsupportedLayout { .. })
        ));
        assert_eq!(read_segment(dir.path(), 3), before);
    }

    /// A segment zmc hasn't initialised yet (size 0) is "not running".
    #[test]
    fn uninitialised_segment_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        fake_segment(dir.path(), 5, 0, 0);
        assert!(matches!(
            MonitorShm::connect_with_path(5, p, "zm.mmap"),
            Err(ShmError::Invalid)
        ));
    }

    // Integration tests would go here but require a running ZoneMinder instance
    // #[test]
    // #[ignore]
    // fn test_connect_to_monitor() {
    //     let shm = MonitorShm::connect(1).unwrap();
    //     println!("Monitor 1 state: {:?}", shm.get_state());
    // }
}
