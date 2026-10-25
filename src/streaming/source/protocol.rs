//! Wire protocol for ZoneMinder's per-monitor stream socket.
//!
//! Each `zmc` process serves the monitor's compressed media on
//! `{PATH_SOCKS}/stream_{monitor_id}.sock` (see `docs/stream_socket.rst` in
//! the ZoneMinder tree). All integers are little-endian. Every message starts
//! with a 24-byte fixed header:
//!
//! ```text
//! u32  length      bytes following this field (20 + payload size)
//! u8   version     protocol version, 1
//! u8   type        message type
//! u8   stream      0 = video, 1 = audio
//! u8   flags       bit 0: keyframe (video); other bits reserved
//! u32  sequence    per-stream, counts every message produced
//! u32  generation  stream epoch; a bump means re-init the decoder
//! u64  pts_us      microseconds (AV_TIME_BASE_Q), shared clock
//! ```
//!
//! Canonical version 1 has no client-to-server messages and zmc ignores
//! inbound bytes. zm-next workers additionally accept `0x11 Command` and reply
//! with `0x12 Response` on the same connection.

use super::media::{AudioCodec, VideoCodec};

pub const PROTOCOL_VERSION: u8 = 1;

/// Total serialized header size, including the leading length field.
pub const HEADER_SIZE: usize = 24;

/// Bytes of fixed header counted by the `length` field (everything after the
/// length field itself).
const HEADER_LENGTH_BYTES: u32 = (HEADER_SIZE - 4) as u32;

/// Sanity cap on the length field; larger values mean a corrupt or hostile
/// peer (matches zmc's `kMaxMessageLength`).
pub const MAX_MESSAGE_LENGTH: u32 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Hello,
    Media,
    Keyframe,
    Stats,
    Bye,
    /// Monitor lifecycle / analysis event (see [`MonitorEvent`]). Carried on
    /// [`StreamId::Monitor`]; payload is a u16 code plus a TLV tail.
    Event,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(MessageType::Hello),
            0x02 => Some(MessageType::Media),
            0x03 => Some(MessageType::Keyframe),
            0x04 => Some(MessageType::Stats),
            0x05 => Some(MessageType::Bye),
            0x06 => Some(MessageType::Event),
            // 0x10..=0x13 are the optional client→server control extension
            // (Subscribe/Command/Response/Talkback); canonical consumers never
            // send them and skip them on the wire like any other unknown type.
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamId {
    Video,
    Audio,
    /// Neither video nor audio: monitor lifecycle / analysis EVENT frames.
    Monitor,
}

impl StreamId {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(StreamId::Video),
            1 => Some(StreamId::Audio),
            2 => Some(StreamId::Monitor),
            _ => None,
        }
    }
}

/// Header flag: the message carries a video keyframe access unit.
pub const FLAG_KEYFRAME: u8 = 0x01;

// HELLO TLV tags
const TLV_CODEC_ID: u8 = 0x01; // u32, AVCodecID value
const TLV_EXTRADATA: u8 = 0x02; // raw codecpar->extradata
const TLV_WIDTH: u8 = 0x03; // u32
const TLV_HEIGHT: u8 = 0x04; // u32
const TLV_FPS_NUM: u8 = 0x05; // u32
const TLV_FPS_DEN: u8 = 0x06; // u32
const TLV_SAMPLE_RATE: u8 = 0x07; // u32
const TLV_CHANNELS: u8 = 0x08; // u32
const TLV_PROFILE: u8 = 0x09; // u32
const TLV_LEVEL: u8 = 0x0A; // u32
/// utf8, FFmpeg's codec name (`h264`, `hevc`, `aac`). Unlike the id, it
/// doesn't change between FFmpeg versions.
const TLV_CODEC_NAME: u8 = 0x0B;

// EVENT codes. Lifecycle codes are canonical (also emitted by stock zmc on the
// `feature/stream-socket-events` branch); the 0x03xx range is zm-next's
// additive analysis/AI extension. Unknown codes are surfaced verbatim, never
// errored.
pub const EVENT_SNAPSHOT: u16 = 0x0001; // current health+state, replayed on connect
pub const EVENT_CONNECTION_FAILED: u16 = 0x0101;
pub const EVENT_CONNECTION_RESTORED: u16 = 0x0102;
pub const EVENT_PRIME_CAPTURE_FAILED: u16 = 0x0103;
pub const EVENT_PRIME_CAPTURE_RESTORED: u16 = 0x0104;
pub const EVENT_CAPTURE_FAILED: u16 = 0x0105;
pub const EVENT_CAPTURE_RESUMED: u16 = 0x0106;
pub const EVENT_STATE_CHANGED: u16 = 0x0201;
// zm-next analysis/AI extension codes (reserved 0x03xx range):
pub const EVENT_DETECTION: u16 = 0x0301; // motion / object detection
pub const EVENT_DESCRIPTION: u16 = 0x0302; // VLM scene description
pub const EVENT_RECORDING_SAVED: u16 = 0x0303; // a clip was written to storage
pub const EVENT_RECORDING_OPENING: u16 = 0x0304; // a clip segment opened; awaits an event-id assignment
                                                 // 0x0305 is reserved for a future `reasoning` event.
pub const EVENT_REVIEW_ASSETS: u16 = 0x0306; // motion-synopsis ingredients: tube + plate manifest in json_detail
pub const EVENT_SNAPSHOT_SAVED: u16 = 0x0307; // a JPEG snapshot was written (routine, or the snapshot_now result)
                                              // zm-next worker status codes (reserved 0x04xx range; zm-next
                                              // docs/Worker_Control_Protocol.md, "Status"):
pub const EVENT_WORKER_STATE: u16 = 0x0401; // unconfigured / configuring / running / stopping
pub const EVENT_STREAM_AUTH_FAILED: u16 = 0x0402; // camera answered 401/403; slow retry
pub const EVENT_WORKER_DEGRADED: u16 = 0x0403; // a dependency is down and a stage is skipped

/// Client→server control message type (the `0x11 Command` of zm-next's control
/// extension). zm-api is the client; the canonical media producer (zmc) ignores
/// inbound bytes, while zm-next's worker consumes it: `assign_recording` for the
/// id-assignment handshake, and the on-demand `snapshot_now` / `describe_now`.
pub const MSG_TYPE_COMMAND: u8 = 0x11;

/// Server→client reply to a `0x11 Command`, sent only to the connection that
/// sent the command. Payload is UTF-8 JSON (see [`CommandResponse`]).
pub const MSG_TYPE_RESPONSE: u8 = 0x12;

/// zm-next worker hello, sent to every peer right after accept and before the
/// cached HELLO/snapshot/keyframe replay. Payload is UTF-8 JSON (see
/// [`WorkerHello`]). A worker that predates the control protocol never sends it.
pub const MSG_TYPE_WORKER_HELLO: u8 = 0x14;

/// A zm-next worker's hello: versions, installed plugins, hardware and state.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct WorkerHello {
    #[serde(default)]
    pub protocol: HelloProtocol,
    #[serde(default)]
    pub zm_next: HelloZmNext,
    #[serde(default)]
    pub monitor_id: Option<u32>,
    /// `unconfigured` | `configuring` | `running` | `stopping`.
    #[serde(default)]
    pub state: String,
    /// Hash of the active pipeline; absent while unconfigured.
    #[serde(default)]
    pub pipeline_hash: Option<String>,
    #[serde(default)]
    pub plugins: Vec<HelloPlugin>,
    #[serde(default)]
    pub hw: serde_json::Value,
    /// Whether this connection may send commands and configure.
    #[serde(default)]
    pub control_peer: bool,
    /// Salted hash of the active pipeline's secret values (control peers
    /// only); tells zm-api whether the worker already has the secrets it
    /// would send.
    #[serde(default)]
    pub secrets_fingerprint: Option<String>,
    /// Present when the worker couldn't read its plugin manifest.
    #[serde(default)]
    pub catalog_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct HelloProtocol {
    #[serde(default)]
    pub canonical: u32,
    #[serde(default)]
    pub control: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct HelloZmNext {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub plugin_abi: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct HelloPlugin {
    pub kind: String,
    /// `null` for a plugin without a schema.
    #[serde(default)]
    pub version: Option<String>,
    /// `"sha256:…"`, or `null` for a plugin without a schema.
    #[serde(default)]
    pub schema_sha256: Option<String>,
}

/// Parse a WorkerHello payload. `None` when it isn't the expected JSON.
pub fn parse_worker_hello(payload: &[u8]) -> Option<WorkerHello> {
    serde_json::from_slice(payload).ok()
}

/// zm-core's immediate reply to a Command. `ok:true` with message `dispatched`
/// only means the command was handed to the plugins; the result, if any plugin
/// owns the command, arrives separately as an EVENT carrying the same
/// `request_id`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CommandResponse {
    #[serde(default)]
    pub request_id: u64,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    /// Command-specific result: `describe_plugins` schemas, `configure`
    /// errors. zm-core has sent this as a JSON-encoded string, and the control
    /// protocol doc shows an object; [`CommandResponse::data_json`] accepts
    /// both.
    #[serde(default)]
    pub data: serde_json::Value,
}

impl CommandResponse {
    /// `data` as a JSON value: an object as-is, a string holding JSON parsed,
    /// and an empty string or null as `Null`.
    pub fn data_json(&self) -> serde_json::Value {
        match &self.data {
            serde_json::Value::String(s) if s.trim().is_empty() => serde_json::Value::Null,
            serde_json::Value::String(s) => {
                serde_json::from_str(s).unwrap_or_else(|_| self.data.clone())
            }
            other => other.clone(),
        }
    }
}

impl MonitorEvent {
    /// The event's JSON detail object. zm-next puts it in the JSON detail TLV
    /// for its own codes and in the message TLV for the canonical lifecycle
    /// codes (zm-next `docs/Worker_Control_Protocol.md`, "Status"), so both
    /// are tried.
    pub fn detail_object(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        [self.json_detail.as_deref(), self.message.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|text| match serde_json::from_str(text) {
                Ok(serde_json::Value::Object(map)) => Some(map),
                _ => None,
            })
    }
}

/// Parse a Response payload. `None` when it isn't a JSON object.
pub fn parse_command_response(payload: &[u8]) -> Option<CommandResponse> {
    serde_json::from_slice(payload).ok()
}

// EVENT TLV tags
const TLV_WALL_CLOCK_US: u8 = 0x01; // u64, unix-epoch microseconds
const TLV_MESSAGE: u8 = 0x02; // utf8, human-readable detail
const TLV_STATE_ID: u8 = 0x03; // u32, current monitor state
const TLV_PREV_STATE_ID: u8 = 0x04; // u32, previous state (state_changed)
const TLV_DETAIL: u8 = 0x05; // u32, errno / ffmpeg error code
const TLV_STATE_NAME: u8 = 0x06; // utf8, "Idle"/"Alarm"/...
const TLV_HEALTH_CODE: u8 = 0x07; // u16, active fault code in a snapshot (0 = healthy)
const TLV_JSON_DETAIL: u8 = 0x10; // utf8 JSON, zm-next structured analysis/AI detail

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported stream socket protocol version {0}")]
    UnsupportedVersion(u8),

    #[error("implausible message length {0}")]
    BadLength(u32),

    #[error("truncated TLV in HELLO payload")]
    TruncatedTlv,

    #[error("HELLO payload missing codec id")]
    MissingCodecId,

    #[error("payload too short for {0} message")]
    ShortPayload(&'static str),

    #[error("truncated TLV in EVENT payload")]
    TruncatedEventTlv,
}

/// A parsed message header. `msg_type` and `stream` are kept raw so unknown
/// values can be skipped (forward compatibility) instead of erroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub msg_type: u8,
    pub stream: u8,
    pub flags: u8,
    pub sequence: u32,
    pub generation: u32,
    pub pts_us: i64,
    pub payload_len: usize,
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

/// Parse a fixed message header. Fails when the version is unsupported or the
/// length field is impossible (shorter than the fixed header remainder or
/// above the cap) — both mean the byte stream cannot be trusted further.
pub fn parse_header(buf: &[u8; HEADER_SIZE]) -> Result<Header, ProtocolError> {
    let length = read_u32(buf, 0);
    let version = buf[4];
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    if !(HEADER_LENGTH_BYTES..=MAX_MESSAGE_LENGTH).contains(&length) {
        return Err(ProtocolError::BadLength(length));
    }
    Ok(Header {
        msg_type: buf[5],
        stream: buf[6],
        flags: buf[7],
        sequence: read_u32(buf, 8),
        generation: read_u32(buf, 12),
        pts_us: u64::from_le_bytes(buf[16..24].try_into().expect("8 bytes")) as i64,
        payload_len: (length - HEADER_LENGTH_BYTES) as usize,
    })
}

/// Decoded HELLO parameters. Zero / empty means "not present on the wire".
/// A HELLO must carry a codec id, a codec name, or both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelloInfo {
    /// ffmpeg `AVCodecID` value, in zmc's FFmpeg numbering
    pub codec_id: u32,
    /// FFmpeg codec name; empty from a zmc that predates the tag
    pub codec_name: String,
    /// Raw `codecpar->extradata`: SPS/PPS/VPS for H.26x (Annex B or
    /// avcC/hvcC), AudioSpecificConfig for AAC.
    pub extradata: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub profile: u32,
    pub level: u32,
}

/// Parse a HELLO payload: a TLV list of (u8 tag, u16 length, value). Unknown
/// tags are skipped per the protocol spec.
pub fn parse_hello(data: &[u8]) -> Result<HelloInfo, ProtocolError> {
    let mut info = HelloInfo::default();
    let mut saw_codec_id = false;
    let mut i = 0usize;
    while i < data.len() {
        if i + 3 > data.len() {
            return Err(ProtocolError::TruncatedTlv);
        }
        let tag = data[i];
        let len = u16::from_le_bytes([data[i + 1], data[i + 2]]) as usize;
        i += 3;
        if i + len > data.len() {
            return Err(ProtocolError::TruncatedTlv);
        }
        let value = &data[i..i + len];
        i += len;

        let u32_value = || {
            if len == 4 {
                Some(read_u32(value, 0))
            } else {
                None
            }
        };
        match tag {
            TLV_CODEC_ID => {
                if let Some(v) = u32_value() {
                    info.codec_id = v;
                    saw_codec_id = true;
                }
            }
            TLV_EXTRADATA => info.extradata = value.to_vec(),
            TLV_CODEC_NAME => info.codec_name = String::from_utf8_lossy(value).into_owned(),
            TLV_WIDTH => info.width = u32_value().unwrap_or(0),
            TLV_HEIGHT => info.height = u32_value().unwrap_or(0),
            TLV_FPS_NUM => info.fps_num = u32_value().unwrap_or(0),
            TLV_FPS_DEN => info.fps_den = u32_value().unwrap_or(0),
            TLV_SAMPLE_RATE => info.sample_rate = u32_value().unwrap_or(0),
            TLV_CHANNELS => info.channels = u32_value().unwrap_or(0),
            TLV_PROFILE => info.profile = u32_value().unwrap_or(0),
            TLV_LEVEL => info.level = u32_value().unwrap_or(0),
            _ => {} // unknown tag: skip
        }
    }
    if !saw_codec_id && info.codec_name.is_empty() {
        return Err(ProtocolError::MissingCodecId);
    }
    Ok(info)
}

/// Decoded EVENT payload. A field is `Some`/non-empty only when its TLV tag
/// was present on the wire; producers omit tags that do not apply to the code.
/// `code` is always present (it is the fixed u16 prefix).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorEvent {
    /// One of the `EVENT_*` codes. Unknown codes are surfaced, not errored —
    /// consumers map what they understand and ignore the rest.
    pub code: u16,
    /// Unix-epoch microseconds — the timestamp to surface for this event.
    pub wall_clock_us: Option<u64>,
    /// Human-readable detail.
    pub message: Option<String>,
    /// Current monitor state id.
    pub state_id: Option<u32>,
    /// Previous monitor state id (state_changed).
    pub prev_state_id: Option<u32>,
    /// errno / ffmpeg error code accompanying a fault.
    pub detail: Option<u32>,
    /// Current state name ("Idle"/"Alarm"/...).
    pub state_name: Option<String>,
    /// Active fault code in a snapshot (0 = healthy).
    pub health_code: Option<u16>,
    /// zm-next extension: structured analysis/AI detail as a UTF-8 JSON
    /// document (detection object list, description text, recording metadata).
    pub json_detail: Option<String>,
}

/// Parse an EVENT payload: a u16 `code` followed by a TLV tail of
/// (u8 tag, u16 length, value). Unknown tags are skipped per the protocol
/// spec, exactly like HELLO. Fails only when the fixed code or a TLV header
/// runs past the end of the payload.
pub fn parse_event(data: &[u8]) -> Result<MonitorEvent, ProtocolError> {
    if data.len() < 2 {
        return Err(ProtocolError::ShortPayload("EVENT"));
    }
    let mut ev = MonitorEvent {
        code: u16::from_le_bytes([data[0], data[1]]),
        ..MonitorEvent::default()
    };

    let mut i = 2usize;
    while i < data.len() {
        if i + 3 > data.len() {
            return Err(ProtocolError::TruncatedEventTlv);
        }
        let tag = data[i];
        let len = u16::from_le_bytes([data[i + 1], data[i + 2]]) as usize;
        i += 3;
        if i + len > data.len() {
            return Err(ProtocolError::TruncatedEventTlv);
        }
        let value = &data[i..i + len];
        i += len;

        match tag {
            TLV_WALL_CLOCK_US if len == 8 => {
                ev.wall_clock_us = Some(read_u64(value, 0));
            }
            TLV_MESSAGE => ev.message = Some(String::from_utf8_lossy(value).into_owned()),
            TLV_STATE_ID if len == 4 => ev.state_id = Some(read_u32(value, 0)),
            TLV_PREV_STATE_ID if len == 4 => ev.prev_state_id = Some(read_u32(value, 0)),
            TLV_DETAIL if len == 4 => ev.detail = Some(read_u32(value, 0)),
            TLV_STATE_NAME => ev.state_name = Some(String::from_utf8_lossy(value).into_owned()),
            TLV_HEALTH_CODE if len == 2 => {
                ev.health_code = Some(u16::from_le_bytes([value[0], value[1]]));
            }
            TLV_JSON_DETAIL => ev.json_detail = Some(String::from_utf8_lossy(value).into_owned()),
            // Unknown tag, or a known tag with an unexpected length: skip.
            _ => {}
        }
    }
    Ok(ev)
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
}

/// Serialize a client→server control message (header + raw payload) for
/// sending back up the stream socket. Control messages ride the Monitor stream
/// with their own sequence counter; `generation` and `pts` are unused (0).
/// Used for the `0x11 Command` id-assignment reply, whose payload is UTF-8 JSON.
pub fn build_control_message(msg_type: u8, sequence: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&(HEADER_LENGTH_BYTES + payload.len() as u32).to_le_bytes());
    out.push(PROTOCOL_VERSION);
    out.push(msg_type);
    out.push(StreamId::Monitor as u8); // 2 — control rides the monitor stream
    out.push(0); // flags
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // generation (unused)
    out.extend_from_slice(&0u64.to_le_bytes()); // pts_us (unused)
    out.extend_from_slice(payload);
    out
}

/// Parse a STATS payload: u64 messages sent, u64 messages dropped for this
/// consumer.
pub fn parse_stats(data: &[u8]) -> Result<(u64, u64), ProtocolError> {
    if data.len() < 16 {
        return Err(ProtocolError::ShortPayload("STATS"));
    }
    let sent = u64::from_le_bytes(data[0..8].try_into().expect("8 bytes"));
    let dropped = u64::from_le_bytes(data[8..16].try_into().expect("8 bytes"));
    Ok((sent, dropped))
}

/// Map an ffmpeg `AVCodecID` from a video HELLO to the pipeline's codec enum.
pub fn video_codec_from_id(codec_id: u32) -> VideoCodec {
    use ffmpeg_next::ffi::AVCodecID;
    match codec_id {
        x if x == AVCodecID::AV_CODEC_ID_H264 as u32 => VideoCodec::H264,
        x if x == AVCodecID::AV_CODEC_ID_HEVC as u32 => VideoCodec::H265,
        _ => VideoCodec::Unknown,
    }
}

/// Map the FFmpeg codec name from a video HELLO to the pipeline's codec enum.
pub fn video_codec_from_name(name: &str) -> VideoCodec {
    match name {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::H265,
        _ => VideoCodec::Unknown,
    }
}

/// Map the FFmpeg codec name from an audio HELLO to the pipeline's codec enum.
pub fn audio_codec_from_name(name: &str) -> AudioCodec {
    match name {
        "aac" => AudioCodec::Aac,
        "pcm_alaw" => AudioCodec::G711Alaw,
        "pcm_mulaw" => AudioCodec::G711Ulaw,
        "opus" => AudioCodec::Opus,
        _ => AudioCodec::Unknown,
    }
}

/// Map an ffmpeg `AVCodecID` from an audio HELLO to the pipeline's codec enum.
pub fn audio_codec_from_id(codec_id: u32) -> AudioCodec {
    use ffmpeg_next::ffi::AVCodecID;
    match codec_id {
        x if x == AVCodecID::AV_CODEC_ID_AAC as u32 => AudioCodec::Aac,
        x if x == AVCodecID::AV_CODEC_ID_PCM_ALAW as u32 => AudioCodec::G711Alaw,
        x if x == AVCodecID::AV_CODEC_ID_PCM_MULAW as u32 => AudioCodec::G711Ulaw,
        x if x == AVCodecID::AV_CODEC_ID_OPUS as u32 => AudioCodec::Opus,
        _ => AudioCodec::Unknown,
    }
}

/// Test helpers that build wire messages the way zmc's encoder does. Used by
/// the reader and router tests to run a scripted fake zmc.
#[cfg(test)]
pub(crate) mod test_encode {
    use super::*;

    /// Serialize one complete message (header + payload).
    pub fn encode_message(
        msg_type: u8,
        stream: u8,
        flags: u8,
        sequence: u32,
        generation: u32,
        pts_us: i64,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
        out.extend_from_slice(&(HEADER_LENGTH_BYTES + payload.len() as u32).to_le_bytes());
        out.push(PROTOCOL_VERSION);
        out.push(msg_type);
        out.push(stream);
        out.push(flags);
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&generation.to_le_bytes());
        out.extend_from_slice(&(pts_us as u64).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    pub fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + value.len());
        out.push(tag);
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(value);
        out
    }

    pub fn tlv_u32(tag: u8, value: u32) -> Vec<u8> {
        tlv(tag, &value.to_le_bytes())
    }

    pub fn tlv_u64(tag: u8, value: u64) -> Vec<u8> {
        tlv(tag, &value.to_le_bytes())
    }

    pub fn tlv_u16(tag: u8, value: u16) -> Vec<u8> {
        tlv(tag, &value.to_le_bytes())
    }

    /// Build an EVENT payload: the u16 code followed by a pre-built TLV tail.
    pub fn event_payload(code: u16, tlv_tail: &[u8]) -> Vec<u8> {
        let mut p = code.to_le_bytes().to_vec();
        p.extend_from_slice(tlv_tail);
        p
    }

    /// Build a HELLO payload with a codec id and optional extradata.
    pub fn hello_payload(codec_id: u32, extradata: &[u8]) -> Vec<u8> {
        let mut p = tlv_u32(TLV_CODEC_ID, codec_id);
        if !extradata.is_empty() {
            p.extend_from_slice(&tlv(TLV_EXTRADATA, extradata));
        }
        p
    }

    pub fn h264_codec_id() -> u32 {
        ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264 as u32
    }

    pub fn h265_codec_id() -> u32 {
        ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC as u32
    }

    pub fn aac_codec_id() -> u32 {
        ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_AAC as u32
    }

    pub fn alaw_codec_id() -> u32 {
        ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_PCM_ALAW as u32
    }
}

#[cfg(test)]
mod tests {
    use super::test_encode::*;
    use super::*;

    #[test]
    fn header_roundtrip() {
        let msg = encode_message(0x02, 0, FLAG_KEYFRAME, 7, 2, 1_234_567, &[0xAA, 0xBB]);
        assert_eq!(msg.len(), HEADER_SIZE + 2);
        let header = parse_header(msg[..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert_eq!(header.msg_type, 0x02);
        assert_eq!(header.stream, 0);
        assert_eq!(header.flags, FLAG_KEYFRAME);
        assert_eq!(header.sequence, 7);
        assert_eq!(header.generation, 2);
        assert_eq!(header.pts_us, 1_234_567);
        assert_eq!(header.payload_len, 2);
        assert_eq!(
            MessageType::from_u8(header.msg_type),
            Some(MessageType::Media)
        );
        assert_eq!(StreamId::from_u8(header.stream), Some(StreamId::Video));
    }

    #[test]
    fn header_rejects_bad_version() {
        let mut msg = encode_message(0x02, 0, 0, 0, 0, 0, &[]);
        msg[4] = 9;
        assert_eq!(
            parse_header(msg[..HEADER_SIZE].try_into().unwrap()),
            Err(ProtocolError::UnsupportedVersion(9))
        );
    }

    #[test]
    fn header_rejects_implausible_length() {
        // Length below the fixed-header remainder.
        let mut msg = encode_message(0x02, 0, 0, 0, 0, 0, &[]);
        msg[..4].copy_from_slice(&10u32.to_le_bytes());
        assert_eq!(
            parse_header(msg[..HEADER_SIZE].try_into().unwrap()),
            Err(ProtocolError::BadLength(10))
        );
        // Length above the cap.
        let huge = MAX_MESSAGE_LENGTH + 1;
        msg[..4].copy_from_slice(&huge.to_le_bytes());
        assert_eq!(
            parse_header(msg[..HEADER_SIZE].try_into().unwrap()),
            Err(ProtocolError::BadLength(huge))
        );
    }

    #[test]
    fn message_and_stream_ids() {
        assert_eq!(MessageType::from_u8(0x01), Some(MessageType::Hello));
        assert_eq!(MessageType::from_u8(0x03), Some(MessageType::Keyframe));
        assert_eq!(MessageType::from_u8(0x04), Some(MessageType::Stats));
        assert_eq!(MessageType::from_u8(0x05), Some(MessageType::Bye));
        assert_eq!(MessageType::from_u8(0x06), Some(MessageType::Event));
        assert_eq!(MessageType::from_u8(0x77), None);
        // The client→server control extension is "unknown" to canonical
        // consumers and must skip, not resolve.
        assert_eq!(MessageType::from_u8(0x10), None);
        assert_eq!(StreamId::from_u8(1), Some(StreamId::Audio));
        assert_eq!(StreamId::from_u8(2), Some(StreamId::Monitor));
        assert_eq!(StreamId::from_u8(3), None);
    }

    #[test]
    fn hello_parses_codec_and_extradata() {
        let payload = hello_payload(h264_codec_id(), &[0x00, 0x00, 0x00, 0x01, 0x67]);
        let info = parse_hello(&payload).unwrap();
        assert_eq!(info.codec_id, h264_codec_id());
        assert_eq!(info.extradata, vec![0x00, 0x00, 0x00, 0x01, 0x67]);
        assert_eq!(video_codec_from_id(info.codec_id), VideoCodec::H264);
    }

    #[test]
    fn hello_parses_all_u32_fields() {
        let mut payload = tlv_u32(0x01, aac_codec_id());
        payload.extend_from_slice(&tlv_u32(0x03, 1920));
        payload.extend_from_slice(&tlv_u32(0x04, 1080));
        payload.extend_from_slice(&tlv_u32(0x05, 30));
        payload.extend_from_slice(&tlv_u32(0x06, 1));
        payload.extend_from_slice(&tlv_u32(0x07, 16000));
        payload.extend_from_slice(&tlv_u32(0x08, 1));
        payload.extend_from_slice(&tlv_u32(0x09, 2));
        payload.extend_from_slice(&tlv_u32(0x0A, 120));
        let info = parse_hello(&payload).unwrap();
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.fps_num, 30);
        assert_eq!(info.fps_den, 1);
        assert_eq!(info.sample_rate, 16000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.profile, 2);
        assert_eq!(info.level, 120);
    }

    #[test]
    fn hello_skips_unknown_tags() {
        let mut payload = tlv(0x7F, &[1, 2, 3, 4, 5]); // unknown tag first
        payload.extend_from_slice(&tlv_u32(0x01, h265_codec_id()));
        let info = parse_hello(&payload).unwrap();
        assert_eq!(video_codec_from_id(info.codec_id), VideoCodec::H265);
    }

    #[test]
    fn hello_rejects_truncation_and_missing_codec() {
        // Tag + declared length running past the end.
        let mut payload = tlv_u32(0x01, h264_codec_id());
        payload.extend_from_slice(&[0x02, 0xFF, 0x00, 0xAA]); // claims 255 bytes, has 1
        assert_eq!(parse_hello(&payload), Err(ProtocolError::TruncatedTlv));
        // Tag header itself truncated.
        assert_eq!(parse_hello(&[0x01, 0x04]), Err(ProtocolError::TruncatedTlv));
        // No codec id at all.
        assert_eq!(
            parse_hello(&tlv_u32(0x03, 640)),
            Err(ProtocolError::MissingCodecId)
        );
        // Empty payload.
        assert_eq!(parse_hello(&[]), Err(ProtocolError::MissingCodecId));
    }

    #[test]
    fn hello_carries_the_codec_name() {
        // zmc sends the FFmpeg codec name (tag 0x0B) beside the numeric id.
        let mut payload = tlv_u32(0x01, u32::MAX - 1);
        payload.extend_from_slice(&tlv(0x0B, b"hevc"));
        let info = parse_hello(&payload).unwrap();
        assert_eq!(info.codec_name, "hevc");
        assert_eq!(info.codec_id, u32::MAX - 1);

        // A name alone is enough; an older zmc sends only the id.
        let info = parse_hello(&tlv(0x0B, b"pcm_mulaw")).unwrap();
        assert_eq!(info.codec_name, "pcm_mulaw");
        let info = parse_hello(&tlv_u32(0x01, h264_codec_id())).unwrap();
        assert_eq!(info.codec_name, "");
    }

    #[test]
    fn codec_name_mapping() {
        assert_eq!(video_codec_from_name("h264"), VideoCodec::H264);
        assert_eq!(video_codec_from_name("hevc"), VideoCodec::H265);
        assert_eq!(video_codec_from_name("mjpeg"), VideoCodec::Unknown);
        assert_eq!(audio_codec_from_name("aac"), AudioCodec::Aac);
        assert_eq!(audio_codec_from_name("pcm_alaw"), AudioCodec::G711Alaw);
        assert_eq!(audio_codec_from_name("pcm_mulaw"), AudioCodec::G711Ulaw);
        assert_eq!(audio_codec_from_name("opus"), AudioCodec::Opus);
        assert_eq!(audio_codec_from_name("mp3"), AudioCodec::Unknown);
    }

    #[test]
    fn event_parses_lifecycle_state_change() {
        // state_changed: wall clock + state ids + names.
        let mut tail = tlv_u64(0x01, 1_700_000_000_000_000);
        tail.extend_from_slice(&tlv_u32(0x03, 2));
        tail.extend_from_slice(&tlv_u32(0x04, 1));
        tail.extend_from_slice(&tlv(0x06, b"Alarm"));
        tail.extend_from_slice(&tlv(0x02, b"motion in zone 1"));
        let payload = event_payload(EVENT_STATE_CHANGED, &tail);

        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, EVENT_STATE_CHANGED);
        assert_eq!(ev.wall_clock_us, Some(1_700_000_000_000_000));
        assert_eq!(ev.state_id, Some(2));
        assert_eq!(ev.prev_state_id, Some(1));
        assert_eq!(ev.state_name.as_deref(), Some("Alarm"));
        assert_eq!(ev.message.as_deref(), Some("motion in zone 1"));
        assert_eq!(ev.health_code, None);
        assert_eq!(ev.json_detail, None);
    }

    #[test]
    fn event_parses_zmnext_detection_json() {
        let json = r#"{"objects":[{"label":"person","confidence":0.91}],"frame_pts_us":42}"#;
        let mut tail = tlv_u64(0x01, 1_700_000_000_000_000);
        tail.extend_from_slice(&tlv(0x10, json.as_bytes()));
        let payload = event_payload(EVENT_DETECTION, &tail);

        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, EVENT_DETECTION);
        assert_eq!(ev.json_detail.as_deref(), Some(json));
        assert_eq!(ev.wall_clock_us, Some(1_700_000_000_000_000));
    }

    #[test]
    fn event_parses_review_assets_manifest_json() {
        // 0x0306 review_assets carries the synopsis tube/plate manifest in the
        // same json_detail TLV (0x10) as detection — no parser changes needed.
        let json = r#"{"type":"review_assets","schema":1,"monitor_id":3,"tubes":[]}"#;
        let mut tail = tlv_u64(0x01, 1_782_129_185_000_000);
        tail.extend_from_slice(&tlv(0x10, json.as_bytes()));
        let payload = event_payload(EVENT_REVIEW_ASSETS, &tail);

        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, EVENT_REVIEW_ASSETS);
        assert_eq!(ev.json_detail.as_deref(), Some(json));
        assert_eq!(ev.wall_clock_us, Some(1_782_129_185_000_000));
    }

    #[test]
    fn event_parses_snapshot_health_code() {
        let tail = tlv_u16(0x07, 0x0105); // capture_failed health code, healthy=0
        let payload = event_payload(EVENT_SNAPSHOT, &tail);
        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, EVENT_SNAPSHOT);
        assert_eq!(ev.health_code, Some(0x0105));
    }

    #[test]
    fn event_skips_unknown_tags_and_surfaces_unknown_codes() {
        // An unknown code (future lifecycle event) with an unknown tag mixed
        // in among known ones — both must be tolerated.
        let mut tail = tlv(0x7F, &[1, 2, 3]); // unknown tag
        tail.extend_from_slice(&tlv(0x02, b"hi"));
        let payload = event_payload(0x09FF, &tail); // unknown code
        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, 0x09FF);
        assert_eq!(ev.message.as_deref(), Some("hi"));
    }

    #[test]
    fn event_rejects_short_code_and_truncated_tlv() {
        // Fewer than the 2 fixed code bytes.
        assert_eq!(
            parse_event(&[0x01]),
            Err(ProtocolError::ShortPayload("EVENT"))
        );
        // Code present, TLV header claims more value than remains.
        let mut payload = 0x0301u16.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0x02, 0xFF, 0x00, 0xAA]); // claims 255 bytes
        assert_eq!(parse_event(&payload), Err(ProtocolError::TruncatedEventTlv));
        // Empty payload still has no code.
        assert_eq!(parse_event(&[]), Err(ProtocolError::ShortPayload("EVENT")));
    }

    #[test]
    fn control_message_round_trips_through_header() {
        let json = br#"{"cmd":"assign_recording","event_id":512}"#;
        let msg = build_control_message(MSG_TYPE_COMMAND, 3, json);
        let header = parse_header(msg[..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert_eq!(header.msg_type, MSG_TYPE_COMMAND);
        assert_eq!(header.stream, 2); // Monitor
        assert_eq!(header.sequence, 3);
        assert_eq!(header.payload_len, json.len());
        assert_eq!(&msg[HEADER_SIZE..], json);
        // The control type is not a media/parse type — consumers skip it.
        assert_eq!(MessageType::from_u8(header.msg_type), None);
    }

    #[test]
    fn snapshot_saved_event_code_matches_zmnext() {
        // zm-next kEventSnapshotSaved: routine snapshots and the snapshot_now
        // result both use it.
        assert_eq!(EVENT_SNAPSHOT_SAVED, 0x0307);
        let json = r#"{"event":"EventSnapshot","path":"/s/a.jpg","request_id":7,"on_demand":true,"ok":true}"#;
        let payload = event_payload(EVENT_SNAPSHOT_SAVED, &tlv(0x10, json.as_bytes()));
        let ev = parse_event(&payload).unwrap();
        assert_eq!(ev.code, EVENT_SNAPSHOT_SAVED);
        assert_eq!(ev.json_detail.as_deref(), Some(json));
    }

    #[test]
    fn command_response_parses_zmnext_reply() {
        assert_eq!(MSG_TYPE_RESPONSE, 0x12);
        // Exactly what zm-core's WorkerLink writes.
        let ok = parse_command_response(
            br#"{"data":"","message":"dispatched","ok":true,"request_id":7}"#,
        )
        .unwrap();
        assert_eq!(
            ok,
            CommandResponse {
                request_id: 7,
                ok: true,
                message: "dispatched".into(),
                data: serde_json::Value::String(String::new()),
            }
        );
        let rejected = parse_command_response(
            br#"{"data":"","message":"unknown_command: bogus","ok":false,"request_id":10}"#,
        )
        .unwrap();
        assert!(!rejected.ok);
        assert_eq!(rejected.message, "unknown_command: bogus");
        // A reply to a command sent without a request_id.
        assert_eq!(
            parse_command_response(br#"{"ok":true,"message":"x"}"#)
                .unwrap()
                .request_id,
            0
        );
        assert!(parse_command_response(b"not json").is_none());
    }

    /// The hello example from zm-next `docs/Worker_Control_Protocol.md`
    /// ("Worker hello"). Replace with `tests/contract/` transcripts once zm-next
    /// publishes them.
    #[test]
    fn worker_hello_parses_the_doc_example() {
        assert_eq!(MSG_TYPE_WORKER_HELLO, 0x14);
        let doc = br#"{
          "protocol": {"canonical": 1, "control": 1},
          "zm_next": {"version": "0.1.0", "commit": "9217bdf", "plugin_abi": 1},
          "monitor_id": 3,
          "state": "running",
          "pipeline_hash": "sha256:4f1c",
          "plugins": [
            {"kind": "capture_rtsp_multi", "version": "1.0.0", "schema_sha256": "9ab2"},
            {"kind": "tracker", "version": "1.2.0", "schema_sha256": "77e0"}
          ],
          "hw": {"backends": ["metal"], "decoders": ["videotoolbox"]},
          "control_peer": true
        }"#;
        let hello = parse_worker_hello(doc).expect("parses");
        assert_eq!(hello.protocol.control, 1);
        assert_eq!(hello.zm_next.commit, "9217bdf");
        assert_eq!(hello.monitor_id, Some(3));
        assert_eq!(hello.state, "running");
        assert_eq!(hello.pipeline_hash.as_deref(), Some("sha256:4f1c"));
        assert_eq!(hello.plugins[1].kind, "tracker");
        assert!(hello.control_peer);
        // zm-next's implemented hello: plugins without a schema report nulls.
        let real = parse_worker_hello(
            br#"{"protocol":{"canonical":1,"control":1},"zm_next":{"version":"0.1.0","commit":"525a391","plugin_abi":1},
                 "monitor_id":21,"state":"failed","pipeline_hash":null,
                 "plugins":[{"kind":"capture_rtsp_multi","version":"1.0.0","schema_sha256":"sha256:9ab2"},
                            {"kind":"capture_file","version":null,"schema_sha256":null}],
                 "hw":{"backends":["metal"]},"secrets_fingerprint":"sha256:8489","control_peer":true}"#,
        )
        .expect("nulls parse");
        assert_eq!(real.plugins[1].schema_sha256, None);
        assert_eq!(
            real.plugins[0].schema_sha256.as_deref(),
            Some("sha256:9ab2")
        );
        assert_eq!(real.state, "failed");
        assert_eq!(real.secrets_fingerprint.as_deref(), Some("sha256:8489"));
        // Unknown fields and a minimal unconfigured hello are fine.
        let min = parse_worker_hello(br#"{"state":"unconfigured","future":1}"#).unwrap();
        assert_eq!(min.pipeline_hash, None);
        assert!(!min.control_peer);
        assert!(parse_worker_hello(b"nope").is_none());
    }

    #[test]
    fn response_data_accepts_object_or_encoded_string() {
        let obj = parse_command_response(
            br#"{"request_id":1,"ok":true,"message":"","data":{"tracker":{"version":"1"}}}"#,
        )
        .unwrap();
        assert_eq!(obj.data_json()["tracker"]["version"], "1");
        let enc = parse_command_response(
            br#"{"request_id":1,"ok":true,"message":"","data":"{\"tracker\":{\"version\":\"1\"}}"}"#,
        )
        .unwrap();
        assert_eq!(enc.data_json()["tracker"]["version"], "1");
        let empty = parse_command_response(
            br#"{"request_id":1,"ok":true,"message":"dispatched","data":""}"#,
        )
        .unwrap();
        assert!(empty.data_json().is_null());
    }

    #[test]
    fn status_detail_is_read_from_either_tlv() {
        let from_detail = MonitorEvent {
            code: EVENT_STREAM_AUTH_FAILED,
            json_detail: Some(r#"{"stream_id":0,"retry_in_sec":60}"#.into()),
            ..MonitorEvent::default()
        };
        assert_eq!(from_detail.detail_object().unwrap()["retry_in_sec"], 60);
        let from_message = MonitorEvent {
            code: EVENT_CONNECTION_FAILED,
            message: Some(r#"{"stream_id":0,"error":"timeout","attempt":2}"#.into()),
            ..MonitorEvent::default()
        };
        assert_eq!(from_message.detail_object().unwrap()["attempt"], 2);
        let plain = MonitorEvent {
            code: EVENT_CONNECTION_FAILED,
            message: Some("camera unreachable".into()),
            ..MonitorEvent::default()
        };
        assert!(plain.detail_object().is_none());
    }

    #[test]
    fn stats_parse() {
        let mut payload = 1000u64.to_le_bytes().to_vec();
        payload.extend_from_slice(&42u64.to_le_bytes());
        assert_eq!(parse_stats(&payload), Ok((1000, 42)));
        assert_eq!(
            parse_stats(&[0u8; 8]),
            Err(ProtocolError::ShortPayload("STATS"))
        );
    }

    #[test]
    fn codec_id_mapping() {
        use ffmpeg_next::ffi::AVCodecID;
        assert_eq!(
            video_codec_from_id(AVCodecID::AV_CODEC_ID_H264 as u32),
            VideoCodec::H264
        );
        assert_eq!(
            video_codec_from_id(AVCodecID::AV_CODEC_ID_HEVC as u32),
            VideoCodec::H265
        );
        // AV1 is real but unsupported by this pipeline.
        assert_eq!(
            video_codec_from_id(AVCodecID::AV_CODEC_ID_AV1 as u32),
            VideoCodec::Unknown
        );
        assert_eq!(
            audio_codec_from_id(AVCodecID::AV_CODEC_ID_AAC as u32),
            AudioCodec::Aac
        );
        assert_eq!(
            audio_codec_from_id(AVCodecID::AV_CODEC_ID_PCM_ALAW as u32),
            AudioCodec::G711Alaw
        );
        assert_eq!(
            audio_codec_from_id(AVCodecID::AV_CODEC_ID_PCM_MULAW as u32),
            AudioCodec::G711Ulaw
        );
        assert_eq!(
            audio_codec_from_id(AVCodecID::AV_CODEC_ID_OPUS as u32),
            AudioCodec::Opus
        );
        assert_eq!(
            audio_codec_from_id(AVCodecID::AV_CODEC_ID_MP3 as u32),
            AudioCodec::Unknown
        );
    }
}
