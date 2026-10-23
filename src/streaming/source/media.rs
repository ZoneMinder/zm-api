//! Media types and bitstream helpers shared across the streaming pipeline.
//!
//! These are transport-agnostic: packet types carried on the per-monitor
//! broadcast channels, Annex B NAL inspection/splitting, ADTS framing, and
//! avcC/hvcC decoder-configuration parsing.

use tracing::debug;

/// Video codec of a monitor's stream
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    H264,
    H265,
    #[default]
    Unknown,
}

impl VideoCodec {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "H264",
            VideoCodec::H265 => "H265",
            VideoCodec::Unknown => "Unknown",
        }
    }
}

impl serde::Serialize for VideoCodec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// One video NAL unit (Annex B, with start code) from a monitor's stream.
///
/// Every NAL split from the same access unit shares that AU's timestamp,
/// which is exactly the grouping the segmenter and RTP packetizer need for
/// multi-slice frames.
#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub monitor_id: u32,
    pub timestamp_us: i64,
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub codec: VideoCodec,
}

/// Audio codec types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    G711Alaw,
    G711Ulaw,
    Opus,
    Unknown,
}

impl AudioCodec {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::G711Alaw => "G.711 A-law",
            AudioCodec::G711Ulaw => "G.711 u-law",
            AudioCodec::Opus => "Opus",
            AudioCodec::Unknown => "Unknown",
        }
    }
}

/// One audio frame from a monitor's stream. AAC frames are ADTS-framed (the
/// stream-socket reader wraps zmc's raw AAC using the HELLO extradata);
/// G.711/Opus frames are raw codec payloads.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub monitor_id: u32,
    pub timestamp_us: i64,
    pub data: Vec<u8>,
    pub codec: AudioCodec,
}

/// Return the H.264 NAL unit type (bits 0-4 of first byte after start code).
///
/// Accepts both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes.
/// Returns `None` if no start code is found.
pub fn h264_nal_type(nal_data: &[u8]) -> Option<u8> {
    let offset = if nal_data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal_data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal_data.get(offset).map(|b| b & 0x1F)
}

/// Extract the NAL unit type from an H.265/HEVC NAL unit.
///
/// HEVC uses a two-byte NAL header; the 6-bit `nal_unit_type` occupies bits
/// 1–6 of the first byte (`(byte >> 1) & 0x3F`). Returns `None` when the NAL
/// has no recognizable Annex B start code or is too short. Parameter-set
/// types: VPS = 32, SPS = 33, PPS = 34.
pub fn h265_nal_type(nal_data: &[u8]) -> Option<u8> {
    let offset = if nal_data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal_data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal_data.get(offset).map(|b| (b >> 1) & 0x3F)
}

/// Whether a NAL unit is a keyframe slice.
///
/// H.264: type 5 = IDR (Instantaneous Decoder Refresh).
/// H.265: types 16-21 = IRAP (Intra Random Access Point).
pub fn nal_is_keyframe(nal_data: &[u8], codec: VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => h264_nal_type(nal_data) == Some(5),
        VideoCodec::H265 => h265_nal_type(nal_data).is_some_and(|t| (16..=21).contains(&t)),
        VideoCodec::Unknown => false,
    }
}

/// Returns `true` if a VCL NAL unit begins a new primary coded picture.
///
/// Access-unit assembly groups every slice of one coded picture together, and
/// the picture's *first* slice is the only reliable boundary marker. This
/// inspects the first bit of the slice (segment) header, which both codecs
/// define as a picture-start marker:
///
/// * **H.264** — the slice header opens with `first_mb_in_slice`, an unsigned
///   Exp-Golomb (`ue(v)`) value. The first slice of a picture has
///   `first_mb_in_slice == 0`, which encodes as the single bit `1` — the MSB
///   of the first slice-header byte is set. Every continuation slice has
///   `first_mb_in_slice > 0`; an Exp-Golomb value greater than zero always
///   begins with a `0` bit, so its MSB is clear. This holds for the very
///   large macroblock indices a 4K picture's later slices carry (e.g. slice
///   24 of a 3840×2160 frame starts well past macroblock 8000). Emulation-
///   prevention bytes (`0x03`) are only ever inserted as the third byte of a
///   `00 00` run, so they can never displace the first slice-header byte.
///
/// * **H.265/HEVC** — the two-byte NAL header is followed by the slice segment
///   header, which opens with the one-bit `first_slice_segment_in_pic_flag`.
///   A set MSB on that byte marks the first slice of the picture.
///
/// Callers are expected to pass VCL NALs only. When the NAL has no
/// recognizable start code, or is too short to inspect, the answer is the
/// conservative `true`: starting a fresh access unit is safer than merging
/// two distinct pictures into one.
pub fn slice_starts_picture(nal_data: &[u8], codec: VideoCodec) -> bool {
    let start_code_len = if nal_data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal_data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return true;
    };
    // The slice (segment) header begins immediately after the NAL header:
    // one byte for H.264, two bytes for H.265.
    let nal_header_len = match codec {
        VideoCodec::H265 => 2,
        VideoCodec::H264 | VideoCodec::Unknown => 1,
    };
    match nal_data.get(start_code_len + nal_header_len) {
        Some(&first_slice_header_byte) => first_slice_header_byte & 0x80 != 0,
        None => true,
    }
}

/// Extract `profile-level-id` from an H.264 SPS NAL unit.
///
/// The three bytes immediately after the NAL header in an SPS are
/// `profile_idc`, `constraint_set_flags`, and `level_idc` — exactly the
/// value needed for the SDP `profile-level-id` parameter.
///
/// Returns a 6-character hex string (e.g. `"4d0033"` for Main Profile Level 5.1).
pub fn extract_profile_level_id(nal_data: &[u8]) -> Option<String> {
    let offset = if nal_data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal_data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };

    // Need NAL header + profile_idc + constraint_flags + level_idc
    if nal_data.len() < offset + 4 {
        return None;
    }

    let nal_type = nal_data[offset] & 0x1F;
    if nal_type != 7 {
        return None; // Not an SPS
    }

    let profile_idc = nal_data[offset + 1];
    let constraint_flags = nal_data[offset + 2];
    let level_idc = nal_data[offset + 3];

    Some(format!(
        "{:02x}{:02x}{:02x}",
        profile_idc, constraint_flags, level_idc
    ))
}

/// Find an Annex B start code in `buf` starting at position `from`.
/// Returns `(position, start_code_length)` where length is 3 or 4.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    if buf.len() < from + 3 {
        return None;
    }
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0x00 && buf[i + 1] == 0x00 {
            if buf[i + 2] == 0x01 {
                // Check for 4-byte start code (00 00 00 01)
                if i > 0 && buf[i - 1] == 0x00 {
                    // Prefer reporting the 4-byte version (backtrack one byte)
                    // but only if caller's `from` allows it
                    if i > from {
                        return Some((i - 1, 4));
                    }
                }
                return Some((i, 3));
            }
            // Check 00 00 00 01 starting at i
            if i + 3 < buf.len() && buf[i + 2] == 0x00 && buf[i + 3] == 0x01 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// Extract the next complete NAL unit from `buf`.
///
/// Scans for two consecutive Annex B start codes. Returns the bytes from
/// the first start code up to (but not including) the second. Bytes before
/// the first start code are discarded (handles joining mid-stream).
fn extract_next_nal(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    // Find the first start code
    let (first_pos, first_len) = find_start_code(buf, 0)?;

    // Discard any bytes before the first start code (mid-stream garbage)
    if first_pos > 0 {
        buf.drain(..first_pos);
        // Adjust: first start code is now at position 0
        return extract_next_nal(buf);
    }

    // Find the second start code (marks end of first NAL unit)
    let search_from = first_len;
    if let Some((second_pos, _)) = find_start_code(buf, search_from) {
        let nal = buf[..second_pos].to_vec();
        buf.drain(..second_pos);
        Some(nal)
    } else {
        // Only one start code found — need more data
        None
    }
}

/// Split a complete Annex B payload (one access unit) into its constituent
/// NAL units. The final NAL — which has no trailing start code — is also
/// returned. Each returned NAL keeps its leading start code.
pub fn split_annexb_nals(mut body: Vec<u8>) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    while let Some(nal) = extract_next_nal(&mut body) {
        nals.push(nal);
    }
    // `extract_next_nal` returns `None` only once a single start code remains,
    // having already drained any leading garbage — so whatever is left begins
    // at a start code and is the packet's last NAL.
    if find_start_code(&body, 0).is_some() {
        nals.push(body);
    }
    nals
}

/// Sampling frequencies addressed by the ADTS / AudioSpecificConfig
/// `sampling_frequency_index` (ISO 14496-3, Table 1.18).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// A parsed ADTS fixed header (ISO 13818-7 / 14496-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// MPEG-4 Audio Object Type (profile + 1; 2 = AAC-LC)
    pub audio_object_type: u8,
    /// Index into the sampling-frequency table
    pub sampling_frequency_index: u8,
    /// Decoded sample rate in Hz
    pub sample_rate: u32,
    /// Channel configuration (1 = mono, 2 = stereo)
    pub channel_configuration: u8,
    /// Header length: 7 bytes, or 9 when a CRC is present
    pub header_len: usize,
    /// Total frame length including the header
    pub frame_len: usize,
}

impl AdtsHeader {
    /// Parse an ADTS header at the start of `data`. Returns `None` when the
    /// bytes are not a plausible ADTS frame.
    pub fn parse(data: &[u8]) -> Option<AdtsHeader> {
        if data.len() < 7 {
            return None;
        }
        // Syncword (12 bits set), layer == 0.
        if data[0] != 0xFF || (data[1] & 0xF6) != 0xF0 {
            return None;
        }
        let protection_absent = data[1] & 0x01 == 1;
        let header_len = if protection_absent { 7 } else { 9 };

        let profile = (data[2] >> 6) & 0x03;
        let sampling_frequency_index = (data[2] >> 2) & 0x0F;
        let sample_rate = *AAC_SAMPLE_RATES.get(sampling_frequency_index as usize)?;
        let channel_configuration = ((data[2] & 0x01) << 2) | (data[3] >> 6);

        let frame_len = (((data[3] & 0x03) as usize) << 11)
            | ((data[4] as usize) << 3)
            | ((data[5] as usize) >> 5);
        if frame_len < header_len {
            return None;
        }

        Some(AdtsHeader {
            audio_object_type: profile + 1,
            sampling_frequency_index,
            sample_rate,
            channel_configuration,
            header_len,
            frame_len,
        })
    }

    /// Build the 2-byte AudioSpecificConfig (ISO 14496-3 §1.6.2.1) that fMP4
    /// `esds` boxes and AAC decoders need.
    pub fn audio_specific_config(&self) -> [u8; 2] {
        [
            (self.audio_object_type << 3) | (self.sampling_frequency_index >> 1),
            ((self.sampling_frequency_index & 0x01) << 7) | (self.channel_configuration << 3),
        ]
    }

    /// Duration of one AAC frame (1024 samples) in microseconds.
    pub fn frame_duration_us(&self) -> i64 {
        1_024_000_000 / self.sample_rate as i64
    }
}

/// Wraps raw AAC frames in ADTS headers, parameterized from a stream's
/// AudioSpecificConfig.
///
/// zmc's stream socket delivers AAC frames raw (as the RTSP depacketizer
/// produced them) with the ASC in the HELLO extradata. The rest of this
/// codebase — the fMP4 muxer and the AAC→Opus transcoder — consumes
/// self-describing ADTS frames, so the socket reader re-frames each packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsWrapper {
    audio_object_type: u8,
    sampling_frequency_index: u8,
    channel_configuration: u8,
}

impl AdtsWrapper {
    /// Parse an AudioSpecificConfig. Returns `None` for configurations ADTS
    /// cannot express: AOT outside 1..=4 (the 2-bit profile field), an
    /// explicit 24-bit sample rate (index 15), or a missing/PCE channel
    /// configuration.
    pub fn from_asc(asc: &[u8]) -> Option<Self> {
        if asc.len() < 2 {
            return None;
        }
        let audio_object_type = asc[0] >> 3;
        let sampling_frequency_index = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
        let channel_configuration = (asc[1] >> 3) & 0x0F;
        if !(1..=4).contains(&audio_object_type)
            || sampling_frequency_index as usize >= AAC_SAMPLE_RATES.len()
            || channel_configuration == 0
            || channel_configuration > 7
        {
            return None;
        }
        Some(Self {
            audio_object_type,
            sampling_frequency_index,
            channel_configuration,
        })
    }

    /// Frame `raw` with a 7-byte ADTS header (no CRC). Returns `None` when
    /// the frame exceeds the 13-bit ADTS length field (never for real AAC).
    pub fn wrap(&self, raw: &[u8]) -> Option<Vec<u8>> {
        let frame_len = raw.len() + 7;
        if frame_len >= 1 << 13 {
            return None;
        }
        let mut out = Vec::with_capacity(frame_len);
        out.push(0xFF);
        out.push(0xF1); // MPEG-4, layer 00, protection_absent = 1
        out.push(
            ((self.audio_object_type - 1) & 0x03) << 6
                | (self.sampling_frequency_index << 2)
                | ((self.channel_configuration >> 2) & 0x01),
        );
        out.push((self.channel_configuration & 0x03) << 6 | ((frame_len >> 11) as u8 & 0x03));
        out.push((frame_len >> 3) as u8);
        out.push(((frame_len as u8) & 0x07) << 5 | 0x1F); // buffer fullness = 0x7FF
        out.push(0xFC);
        out.extend_from_slice(raw);
        Some(out)
    }
}

/// Prefix a raw NAL with a 4-byte Annex B start code.
fn annexb(nal: &[u8]) -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01];
    v.extend_from_slice(nal);
    v
}

/// Convert a length-prefixed (AVCC) sample into Annex B NAL units.
pub(crate) fn avcc_to_annexb(data: &[u8], length_size: usize) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + length_size <= data.len() {
        let mut len = 0usize;
        for &b in &data[i..i + length_size] {
            len = (len << 8) | b as usize;
        }
        i += length_size;
        if len == 0 || i + len > data.len() {
            break;
        }
        nals.push(annexb(&data[i..i + len]));
        i += len;
    }
    nals
}

/// Parse SPS/PPS(/VPS) parameter sets and the NAL length size from avcC/hvcC.
/// Returns Annex B-framed parameter-set NALs and the AVCC length-prefix size.
pub(crate) fn parse_extradata(
    extradata: &[u8],
    codec: VideoCodec,
) -> Result<(Vec<Vec<u8>>, usize), String> {
    match codec {
        VideoCodec::H264 => parse_avcc(extradata),
        VideoCodec::H265 => parse_hvcc(extradata),
        VideoCodec::Unknown => Err("unknown codec".to_string()),
    }
}

/// Parse an AVCDecoderConfigurationRecord (avcC).
fn parse_avcc(d: &[u8]) -> Result<(Vec<Vec<u8>>, usize), String> {
    if d.len() < 7 {
        return Err("avcC too short".to_string());
    }
    let length_size = (d[4] & 0x03) as usize + 1;
    let mut nals = Vec::new();
    let mut i = 5;
    let num_sps = (d[i] & 0x1f) as usize;
    i += 1;
    for _ in 0..num_sps {
        if i + 2 > d.len() {
            return Err("avcC SPS truncated".to_string());
        }
        let len = ((d[i] as usize) << 8) | d[i + 1] as usize;
        i += 2;
        if i + len > d.len() {
            return Err("avcC SPS truncated".to_string());
        }
        nals.push(annexb(&d[i..i + len]));
        i += len;
    }
    if i >= d.len() {
        return Err("avcC PPS missing".to_string());
    }
    let num_pps = d[i] as usize;
    i += 1;
    for _ in 0..num_pps {
        if i + 2 > d.len() {
            return Err("avcC PPS truncated".to_string());
        }
        let len = ((d[i] as usize) << 8) | d[i + 1] as usize;
        i += 2;
        if i + len > d.len() {
            return Err("avcC PPS truncated".to_string());
        }
        nals.push(annexb(&d[i..i + len]));
        i += len;
    }
    Ok((nals, length_size))
}

/// Parse an HEVCDecoderConfigurationRecord (hvcC).
fn parse_hvcc(d: &[u8]) -> Result<(Vec<Vec<u8>>, usize), String> {
    if d.len() < 23 {
        return Err("hvcC too short".to_string());
    }
    let length_size = (d[21] & 0x03) as usize + 1;
    let num_arrays = d[22] as usize;
    let mut nals = Vec::new();
    let mut i = 23;
    for _ in 0..num_arrays {
        if i + 3 > d.len() {
            return Err("hvcC array header truncated".to_string());
        }
        // d[i]: array_completeness(1) | reserved(1) | NAL_unit_type(6)
        i += 1; // NAL_unit_type not needed: any of VPS/SPS/PPS goes to the stream
        let num_nalus = ((d[i] as usize) << 8) | d[i + 1] as usize;
        i += 2;
        for _ in 0..num_nalus {
            if i + 2 > d.len() {
                return Err("hvcC nalu length truncated".to_string());
            }
            let len = ((d[i] as usize) << 8) | d[i + 1] as usize;
            i += 2;
            if i + len > d.len() {
                return Err("hvcC nalu truncated".to_string());
            }
            nals.push(annexb(&d[i..i + len]));
            i += len;
        }
    }
    Ok((nals, length_size))
}

/// The codec a stream's extradata belongs to, read from its parameter sets.
///
/// zmc's HELLO names the codec by `AVCodecID`, whose numbering moves between
/// FFmpeg majors (HEVC is 173 in FFmpeg 7 and 172 in FFmpeg 9, #212). An id
/// this build does not recognise may still be H.264 or H.265 from a zmc linked
/// against another FFmpeg, and the parameter sets settle it: H.264 carries an
/// SPS and PPS (types 7 and 8), HEVC a VPS, SPS and PPS (32 to 34), and no
/// header byte is a parameter set in both numberings. SEI is allowed beside
/// them. Anything else, or a mix, is `Unknown`.
pub fn codec_from_extradata(extradata: &[u8]) -> VideoCodec {
    if extradata.starts_with(&[0, 0, 1]) || extradata.starts_with(&[0, 0, 0, 1]) {
        return codec_from_parameter_sets(&split_annexb_nals(extradata.to_vec()));
    }
    if extradata.first() == Some(&1) {
        // A configuration record. Either parser can accept the other's bytes
        // as garbage, so a record counts only if its NALs agree with it.
        if let Ok((nals, _)) = parse_hvcc(extradata) {
            if codec_from_parameter_sets(&nals) == VideoCodec::H265 {
                return VideoCodec::H265;
            }
        }
        if let Ok((nals, _)) = parse_avcc(extradata) {
            if codec_from_parameter_sets(&nals) == VideoCodec::H264 {
                return VideoCodec::H264;
            }
        }
    }
    VideoCodec::Unknown
}

fn codec_from_parameter_sets(nals: &[Vec<u8>]) -> VideoCodec {
    let all_h264 = nals.iter().all(|n| matches!(h264_nal_type(n), Some(6..=8)));
    if !nals.is_empty() && all_h264 && nals.iter().any(|n| h264_nal_type(n) == Some(7)) {
        return VideoCodec::H264;
    }
    let all_h265 = nals
        .iter()
        .all(|n| matches!(h265_nal_type(n), Some(32..=34 | 39 | 40)));
    if !nals.is_empty() && all_h265 && nals.iter().any(|n| h265_nal_type(n) == Some(33)) {
        return VideoCodec::H265;
    }
    VideoCodec::Unknown
}

/// Convert a stream's codec extradata into Annex B parameter-set NALs.
///
/// ffmpeg's `codecpar->extradata` (which zmc forwards verbatim in HELLO) is
/// Annex B for elementary-stream inputs (the common RTSP camera case) but an
/// avcC/hvcC configuration record for container-fed sources. Both forms are
/// accepted; anything unrecognizable yields an empty list.
pub fn extradata_to_annexb_nals(extradata: &[u8], codec: VideoCodec) -> Vec<Vec<u8>> {
    if extradata.is_empty() {
        return Vec::new();
    }
    if extradata.starts_with(&[0, 0, 1]) || extradata.starts_with(&[0, 0, 0, 1]) {
        return split_annexb_nals(extradata.to_vec());
    }
    if extradata[0] == 1 {
        match parse_extradata(extradata, codec) {
            Ok((nals, _)) => return nals,
            Err(e) => debug!("Unparseable {} extradata: {}", codec.as_str(), e),
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- NAL helpers ---

    #[test]
    fn test_nal_is_keyframe_h264() {
        // H.264 IDR frame (type 5) - keyframe
        let h264_idr = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];
        assert!(nal_is_keyframe(&h264_idr, VideoCodec::H264));

        // H.264 non-IDR frame (type 1) - not keyframe
        let h264_non_idr = vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x21, 0x58];
        assert!(!nal_is_keyframe(&h264_non_idr, VideoCodec::H264));
    }

    #[test]
    fn test_nal_is_keyframe_h265() {
        // H.265 IDR frame (type 19) - keyframe
        let h265_idr = vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x08];
        assert!(nal_is_keyframe(&h265_idr, VideoCodec::H265));

        // H.265 non-IRAP frame (type 1) - not keyframe
        let h265_non_irap = vec![0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xD0, 0x00];
        assert!(!nal_is_keyframe(&h265_non_irap, VideoCodec::H265));
    }

    #[test]
    fn test_h265_nal_type() {
        // The HEVC NAL type is bits 1–6 of the first header byte. VPS = 32
        // (0x40), SPS = 33 (0x42), PPS = 34 (0x44).
        assert_eq!(
            h265_nal_type(&[0x00, 0x00, 0x00, 0x01, 0x40, 0x01]),
            Some(32)
        );
        assert_eq!(
            h265_nal_type(&[0x00, 0x00, 0x00, 0x01, 0x42, 0x01]),
            Some(33)
        );
        assert_eq!(
            h265_nal_type(&[0x00, 0x00, 0x00, 0x01, 0x44, 0x01]),
            Some(34)
        );
        // IDR_W_RADL = 19 (0x26), with a 3-byte start code.
        assert_eq!(h265_nal_type(&[0x00, 0x00, 0x01, 0x26, 0x01]), Some(19));
        // No start code, and too-short input, both yield None.
        assert_eq!(h265_nal_type(&[0x40, 0x01]), None);
        assert_eq!(h265_nal_type(&[0x00, 0x00, 0x00, 0x01]), None);
    }

    #[test]
    fn test_h264_nal_type_4byte_start_code() {
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x67]), Some(7)); // SPS
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x68]), Some(8)); // PPS
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x65]), Some(5)); // IDR
        assert_eq!(h264_nal_type(&[0, 0, 0, 1, 0x41]), Some(1)); // non-IDR
    }

    #[test]
    fn test_h264_nal_type_3byte_start_code() {
        assert_eq!(h264_nal_type(&[0, 0, 1, 0x67]), Some(7));
    }

    #[test]
    fn test_h264_nal_type_no_start_code() {
        assert_eq!(h264_nal_type(&[0xFF, 0x67]), None);
        assert_eq!(h264_nal_type(&[]), None);
    }

    #[test]
    fn test_split_annexb_nals_multi() {
        // Three NALs back to back; the last one has no trailing start code.
        let body = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xBB, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0xCC, 0xDD, // IDR slice
        ];
        let nals = split_annexb_nals(body);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA]);
        assert_eq!(nals[1], vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xBB]);
        assert_eq!(nals[2], vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xCC, 0xDD]);
    }

    #[test]
    fn test_split_annexb_nals_single_and_empty() {
        let single = split_annexb_nals(vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x11]);
        assert_eq!(single, vec![vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x11]]);

        assert!(split_annexb_nals(vec![0x11, 0x22, 0x33]).is_empty());
        assert!(split_annexb_nals(Vec::new()).is_empty());
    }

    #[test]
    fn test_find_start_code_4byte() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42];
        let result = find_start_code(&buf, 0);
        assert_eq!(result, Some((0, 4)));
    }

    #[test]
    fn test_find_start_code_3byte() {
        let buf = [0xFF, 0x00, 0x00, 0x01, 0x67, 0x42];
        let result = find_start_code(&buf, 0);
        assert_eq!(result, Some((1, 3)));
    }

    #[test]
    fn test_find_start_code_offset() {
        // Start code at position 5
        let buf = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x00, 0x00, 0x00, 0x01, 0x67];
        let result = find_start_code(&buf, 3);
        assert_eq!(result, Some((5, 4)));
    }

    #[test]
    fn test_find_start_code_none() {
        let buf = [0xAA, 0xBB, 0xCC, 0xDD];
        assert_eq!(find_start_code(&buf, 0), None);
    }

    #[test]
    fn test_extract_next_nal_single() {
        // Two NAL units: SPS then PPS
        let mut buf = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F, // NAL 1 (SPS)
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // NAL 2 (PPS)
        ];
        let nal = extract_next_nal(&mut buf).unwrap();
        // First NAL: start code + SPS data
        assert_eq!(nal, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F]);
        // Buffer should now start with second NAL
        assert_eq!(buf[0..4], [0x00, 0x00, 0x00, 0x01]);
        assert_eq!(buf[4], 0x68);
    }

    #[test]
    fn test_extract_next_nal_discards_leading_garbage() {
        // Garbage bytes before first start code (joining mid-stream)
        let mut buf = vec![
            0xAA, 0xBB, 0xCC, // garbage
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // NAL 1
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, // NAL 2
        ];
        let nal = extract_next_nal(&mut buf).unwrap();
        assert_eq!(nal, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42]);
    }

    #[test]
    fn test_extract_next_nal_incomplete() {
        // Only one start code — need more data
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F];
        assert!(extract_next_nal(&mut buf).is_none());
        // Buffer should be preserved
        assert_eq!(buf.len(), 8);
    }

    #[test]
    fn test_extract_next_nal_3byte_start_codes() {
        let mut buf = vec![
            0x00, 0x00, 0x01, 0x67, 0x42, // NAL 1 (3-byte start code)
            0x00, 0x00, 0x01, 0x68, 0xCE, // NAL 2
        ];
        let nal = extract_next_nal(&mut buf).unwrap();
        assert_eq!(nal, vec![0x00, 0x00, 0x01, 0x67, 0x42]);
    }

    // --- slice_starts_picture ---

    /// Encode `first_mb_in_slice` as an H.264 `ue(v)` and return the byte that
    /// immediately follows the NAL header — the byte `slice_starts_picture`
    /// inspects.
    fn first_slice_header_byte(first_mb_in_slice: u32) -> u8 {
        let code = first_mb_in_slice as u64 + 1;
        let significant = 64 - code.leading_zeros();
        let leading_zeros = significant - 1;
        let mut byte = 0u8;
        let mut bit_pos = 0u32; // 0 = MSB
        for _ in 0..leading_zeros {
            bit_pos += 1; // a zero bit; nothing to set
            if bit_pos >= 8 {
                return byte;
            }
        }
        for i in (0..significant).rev() {
            if (code >> i) & 1 == 1 && bit_pos < 8 {
                byte |= 1 << (7 - bit_pos);
            }
            bit_pos += 1;
            if bit_pos >= 8 {
                break;
            }
        }
        byte
    }

    #[test]
    fn test_slice_starts_picture_h264_first_slice() {
        // first_mb_in_slice == 0 → ue(v) is the single bit `1` → MSB set.
        let first = first_slice_header_byte(0);
        assert_eq!(first, 0x80);
        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, first, 0x40];
        assert!(slice_starts_picture(&nal, VideoCodec::H264));
        // Also the 3-byte start-code form.
        let nal3 = vec![0x00, 0x00, 0x01, 0x41, first, 0x40];
        assert!(slice_starts_picture(&nal3, VideoCodec::H264));
    }

    #[test]
    fn test_slice_starts_picture_h264_continuation_slices() {
        // Every continuation slice of a multi-slice picture has
        // first_mb_in_slice > 0, so its first slice-header byte has the MSB
        // clear — including the large macroblock indices a 4K picture emits.
        for first_mb in [1u32, 5, 99, 240, 8160, 16200, 32000] {
            let byte = first_slice_header_byte(first_mb);
            assert_eq!(
                byte & 0x80,
                0,
                "first_mb_in_slice={first_mb} must encode with the MSB clear"
            );
            let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, byte, 0xAA, 0xBB];
            assert!(
                !slice_starts_picture(&nal, VideoCodec::H264),
                "first_mb_in_slice={first_mb} must read as a continuation slice"
            );
        }
    }

    #[test]
    fn test_slice_starts_picture_h264_emulation_prevention_unaffected() {
        // A continuation slice whose header begins `00 00 03 …` (an inserted
        // emulation-prevention byte). The detector inspects only the first
        // slice-header byte (0x00), so the 0x03 cannot shift its decision.
        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x00, 0x00, 0x03, 0x12];
        assert!(!slice_starts_picture(&nal, VideoCodec::H264));
    }

    #[test]
    fn test_slice_starts_picture_h265() {
        // H.265 has a 2-byte NAL header; the slice segment header opens with
        // `first_slice_segment_in_pic_flag`.
        // NAL header bytes 0x26,0x01 = IDR_W_RADL.
        let starts = vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x80, 0x40];
        assert!(slice_starts_picture(&starts, VideoCodec::H265));
        let continuation = vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x40, 0x40];
        assert!(!slice_starts_picture(&continuation, VideoCodec::H265));
    }

    #[test]
    fn test_slice_starts_picture_conservative_defaults() {
        // No start code → cannot locate the slice header → treat as a new AU.
        assert!(slice_starts_picture(&[0x65, 0x80], VideoCodec::H264));
        // Start code present but nothing after the NAL header → new AU.
        assert!(slice_starts_picture(
            &[0x00, 0x00, 0x00, 0x01, 0x65],
            VideoCodec::H264
        ));
    }

    // --- extract_profile_level_id ---

    #[test]
    fn test_extract_profile_level_id_valid_sps() {
        // Main Profile, Level 5.1
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x4D, 0x00, 0x33, 0xFF];
        assert_eq!(extract_profile_level_id(&sps), Some("4d0033".to_string()));
    }

    #[test]
    fn test_extract_profile_level_id_not_sps() {
        // PPS (type 8) — should return None
        let pps = vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80];
        assert_eq!(extract_profile_level_id(&pps), None);
    }

    #[test]
    fn test_extract_profile_level_id_too_short() {
        let short = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x4D];
        assert_eq!(extract_profile_level_id(&short), None);
    }

    // --- ADTS ---

    /// Build a minimal ADTS frame: AAC-LC, given sampling-frequency index and
    /// channel config, with `payload_len` bytes of body.
    pub(super) fn adts_frame(sf_index: u8, channels: u8, payload_len: usize) -> Vec<u8> {
        let frame_len = 7 + payload_len;
        let mut f = vec![0u8; frame_len];
        f[0] = 0xFF;
        f[1] = 0xF1; // MPEG-4, layer 0, no CRC
        f[2] = (1 << 6) | (sf_index << 2) | (channels >> 2); // profile 1 = AAC-LC
        f[3] = ((channels & 0x03) << 6) | ((frame_len >> 11) as u8 & 0x03);
        f[4] = (frame_len >> 3) as u8;
        f[5] = ((frame_len as u8 & 0x07) << 5) | 0x1F;
        f[6] = 0xFC;
        for (i, b) in f.iter_mut().enumerate().skip(7) {
            *b = i as u8;
        }
        f
    }

    #[test]
    fn test_adts_header_parse() {
        // 16 kHz (index 8) mono AAC-LC
        let frame = adts_frame(8, 1, 100);
        let h = AdtsHeader::parse(&frame).expect("valid ADTS header");
        assert_eq!(h.audio_object_type, 2); // AAC-LC
        assert_eq!(h.sampling_frequency_index, 8);
        assert_eq!(h.sample_rate, 16000);
        assert_eq!(h.channel_configuration, 1);
        assert_eq!(h.header_len, 7);
        assert_eq!(h.frame_len, 107);
        assert_eq!(h.frame_duration_us(), 64_000); // 1024 / 16000 s

        // 44.1 kHz (index 4) stereo
        let frame = adts_frame(4, 2, 50);
        let h = AdtsHeader::parse(&frame).expect("valid ADTS header");
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.channel_configuration, 2);
    }

    #[test]
    fn test_adts_header_rejects_invalid() {
        // No syncword
        assert!(AdtsHeader::parse(&[0x00; 16]).is_none());
        // Too short
        assert!(AdtsHeader::parse(&[0xFF, 0xF1, 0x4C]).is_none());
        // Syncword but invalid sampling frequency index (15)
        let mut bad = adts_frame(8, 1, 10);
        bad[2] = (1 << 6) | (15 << 2);
        assert!(AdtsHeader::parse(&bad).is_none());
        // Frame length smaller than the header
        let mut bad = adts_frame(8, 1, 10);
        bad[3] &= 0xC0;
        bad[4] = 0;
        bad[5] = 0x1F;
        assert!(AdtsHeader::parse(&bad).is_none());
    }

    #[test]
    fn test_audio_specific_config() {
        // AAC-LC 44.1 kHz stereo — canonical ASC is [0x12, 0x10]
        let frame = adts_frame(4, 2, 10);
        let h = AdtsHeader::parse(&frame).unwrap();
        assert_eq!(h.audio_specific_config(), [0x12, 0x10]);

        // AAC-LC 16 kHz mono — [0x14, 0x08]
        let frame = adts_frame(8, 1, 10);
        let h = AdtsHeader::parse(&frame).unwrap();
        assert_eq!(h.audio_specific_config(), [0x14, 0x08]);
    }

    #[test]
    fn test_adts_wrapper_roundtrips_through_parser() {
        // AAC-LC 44.1 kHz stereo ASC
        let wrapper = AdtsWrapper::from_asc(&[0x12, 0x10]).expect("valid ASC");
        let raw = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42];
        let framed = wrapper.wrap(&raw).expect("wrappable");

        let h = AdtsHeader::parse(&framed).expect("wrapped frame must parse");
        assert_eq!(h.audio_object_type, 2);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.channel_configuration, 2);
        assert_eq!(h.header_len, 7);
        assert_eq!(h.frame_len, framed.len());
        assert_eq!(&framed[7..], &raw[..]);
        // The wrapped frame's derived ASC matches the input ASC.
        assert_eq!(h.audio_specific_config(), [0x12, 0x10]);
    }

    #[test]
    fn test_adts_wrapper_16k_mono() {
        let wrapper = AdtsWrapper::from_asc(&[0x14, 0x08]).expect("valid ASC");
        let framed = wrapper.wrap(&[0xAB; 20]).expect("wrappable");
        let h = AdtsHeader::parse(&framed).expect("parse");
        assert_eq!(h.sample_rate, 16000);
        assert_eq!(h.channel_configuration, 1);
        assert_eq!(h.audio_specific_config(), [0x14, 0x08]);
    }

    #[test]
    fn test_adts_wrapper_rejects_inexpressible_asc() {
        // Too short
        assert!(AdtsWrapper::from_asc(&[0x12]).is_none());
        // AOT 5 (SBR) does not fit the 2-bit ADTS profile field
        assert!(AdtsWrapper::from_asc(&[0x2A, 0x10]).is_none());
        // Sampling frequency index 15 = explicit 24-bit rate
        assert!(AdtsWrapper::from_asc(&[0x17, 0x90]).is_none());
        // Channel configuration 0 (defined in-stream via PCE)
        assert!(AdtsWrapper::from_asc(&[0x12, 0x00]).is_none());
    }

    #[test]
    fn test_adts_wrapper_rejects_oversized_frame() {
        let wrapper = AdtsWrapper::from_asc(&[0x12, 0x10]).unwrap();
        assert!(wrapper.wrap(&vec![0u8; 8192]).is_none());
    }

    // --- avcC / hvcC ---

    #[test]
    fn avcc_splits_length_prefixed_nals() {
        // Two NALs: [00 00 00 03][AA BB CC] [00 00 00 02][DD EE]
        let data = [0, 0, 0, 3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 2, 0xDD, 0xEE];
        let nals = avcc_to_annexb(&data, 4);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0, 0, 0, 1, 0xAA, 0xBB, 0xCC]);
        assert_eq!(nals[1], vec![0, 0, 0, 1, 0xDD, 0xEE]);
    }

    #[test]
    fn avcc_stops_on_truncation() {
        // Declares length 9 but only 3 bytes follow.
        let data = [0, 0, 0, 9, 0xAA, 0xBB, 0xCC];
        assert!(avcc_to_annexb(&data, 4).is_empty());
    }

    #[test]
    fn parse_avcc_extracts_sps_pps() {
        // version, profile, compat, level, 0xFF (lenSize=4), 0xE1 (1 SPS),
        // SPS len=2 [67 42], 1 PPS, PPS len=2 [68 CE]
        let avcc = [
            1, 0x42, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0xce,
        ];
        let (nals, ls) = parse_avcc(&avcc).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0, 0, 0, 1, 0x67, 0x42]);
        assert_eq!(nals[1], vec![0, 0, 0, 1, 0x68, 0xce]);
    }

    #[test]
    fn parse_hvcc_extracts_vps_sps_pps() {
        // 21 bytes of config header, then byte 21 = lengthSizeMinusOne (0xFF ->
        // length_size 4), byte 22 = num_arrays (3). Each array: NAL-type byte,
        // num_nalus (u16), then [len(u16)][data] per nalu. Three arrays carry
        // one VPS (0x40 01), SPS (0x42 01) and PPS (0x44 01) respectively.
        let mut hvcc = vec![0u8; 21];
        hvcc.push(0xFF);
        hvcc.push(3);
        hvcc.extend_from_slice(&[0x20, 0x00, 0x01, 0x00, 0x02, 0x40, 0x01]); // VPS
        hvcc.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x02, 0x42, 0x01]); // SPS
        hvcc.extend_from_slice(&[0x22, 0x00, 0x01, 0x00, 0x02, 0x44, 0x01]); // PPS

        let (nals, ls) = parse_hvcc(&hvcc).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], vec![0, 0, 0, 1, 0x40, 0x01]);
        assert_eq!(nals[1], vec![0, 0, 0, 1, 0x42, 0x01]);
        assert_eq!(nals[2], vec![0, 0, 0, 1, 0x44, 0x01]);
    }

    #[test]
    fn parse_hvcc_rejects_short_record() {
        assert!(parse_hvcc(&[0u8; 10]).is_err());
    }

    // --- codec_from_extradata (#212) ---

    const H264_SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x33];
    const H264_PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];
    const HEVC_VPS: &[u8] = &[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
    const HEVC_SPS: &[u8] = &[0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01];
    const HEVC_PPS: &[u8] = &[0, 0, 0, 1, 0x44, 0x01, 0xC1, 0x72];

    #[test]
    fn annexb_h264_parameter_sets_are_h264() {
        let e = [H264_SPS, H264_PPS].concat();
        assert_eq!(codec_from_extradata(&e), VideoCodec::H264);
    }

    #[test]
    fn annexb_hevc_parameter_sets_are_h265() {
        let e = [HEVC_VPS, HEVC_SPS, HEVC_PPS].concat();
        assert_eq!(codec_from_extradata(&e), VideoCodec::H265);
        // Three-byte start codes, as some encoders write them.
        let short: Vec<u8> = [&HEVC_VPS[1..], &HEVC_SPS[1..], &HEVC_PPS[1..]].concat();
        assert_eq!(codec_from_extradata(&short), VideoCodec::H265);
    }

    #[test]
    fn sei_beside_the_parameter_sets_is_allowed() {
        let h264_sei: &[u8] = &[0, 0, 0, 1, 0x06, 0x05, 0x01];
        let hevc_sei: &[u8] = &[0, 0, 0, 1, 0x4E, 0x01, 0x05];
        assert_eq!(
            codec_from_extradata(&[H264_SPS, H264_PPS, h264_sei].concat()),
            VideoCodec::H264
        );
        assert_eq!(
            codec_from_extradata(&[HEVC_VPS, HEVC_SPS, HEVC_PPS, hevc_sei].concat()),
            VideoCodec::H265
        );
    }

    #[test]
    fn configuration_records_are_identified() {
        let avcc = [
            1, 0x42, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x4D, 0x00, 0x33, 1, 0, 2, 0x68, 0xce,
        ];
        assert_eq!(codec_from_extradata(&avcc), VideoCodec::H264);

        let mut hvcc = vec![1u8];
        hvcc.extend_from_slice(&[0u8; 20]);
        hvcc.push(0xFF);
        hvcc.push(3);
        hvcc.extend_from_slice(&[0x20, 0x00, 0x01, 0x00, 0x02, 0x40, 0x01]); // VPS
        hvcc.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x02, 0x42, 0x01]); // SPS
        hvcc.extend_from_slice(&[0x22, 0x00, 0x01, 0x00, 0x02, 0x44, 0x01]); // PPS
        assert_eq!(codec_from_extradata(&hvcc), VideoCodec::H265);
    }

    #[test]
    fn anything_else_stays_unknown() {
        assert_eq!(codec_from_extradata(&[]), VideoCodec::Unknown);
        // MJPEG and other codecs carry no H.26x parameter sets.
        assert_eq!(
            codec_from_extradata(&[0xFF, 0xD8, 0xFF, 0xE0]),
            VideoCodec::Unknown
        );
        // A slice is not a parameter set.
        assert_eq!(
            codec_from_extradata(&[0, 0, 0, 1, 0x65, 0x88, 0x84]),
            VideoCodec::Unknown
        );
        // Parameter sets without an SPS cannot be decoded either way.
        assert_eq!(codec_from_extradata(H264_PPS), VideoCodec::Unknown);
        assert_eq!(
            codec_from_extradata(&[HEVC_VPS, HEVC_PPS].concat()),
            VideoCodec::Unknown
        );
        // A mix of the two is corrupt, not either.
        assert_eq!(
            codec_from_extradata(&[H264_SPS, HEVC_SPS].concat()),
            VideoCodec::Unknown
        );
    }

    // --- extradata_to_annexb_nals ---

    #[test]
    fn extradata_annexb_passthrough_splits_nals() {
        let extradata = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x4D, 0x00, 0x33, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, // PPS
        ];
        let nals = extradata_to_annexb_nals(&extradata, VideoCodec::H264);
        assert_eq!(nals.len(), 2);
        assert_eq!(h264_nal_type(&nals[0]), Some(7));
        assert_eq!(h264_nal_type(&nals[1]), Some(8));
    }

    #[test]
    fn extradata_avcc_is_converted() {
        let avcc = [
            1, 0x42, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0xce,
        ];
        let nals = extradata_to_annexb_nals(&avcc, VideoCodec::H264);
        assert_eq!(nals.len(), 2);
        assert_eq!(h264_nal_type(&nals[0]), Some(7));
        assert_eq!(h264_nal_type(&nals[1]), Some(8));
    }

    #[test]
    fn extradata_garbage_yields_nothing() {
        assert!(extradata_to_annexb_nals(&[], VideoCodec::H264).is_empty());
        assert!(extradata_to_annexb_nals(&[0xAA, 0xBB], VideoCodec::H264).is_empty());
        // avcC-looking bytes but unknown codec
        assert!(extradata_to_annexb_nals(&[1, 2, 3, 4, 5, 6, 7], VideoCodec::Unknown).is_empty());
    }

    #[test]
    fn test_codec_as_str() {
        assert_eq!(VideoCodec::H264.as_str(), "H264");
        assert_eq!(VideoCodec::H265.as_str(), "H265");
        assert_eq!(VideoCodec::Unknown.as_str(), "Unknown");
    }

    #[test]
    fn test_audio_codec_as_str() {
        assert_eq!(AudioCodec::Aac.as_str(), "AAC");
        assert_eq!(AudioCodec::G711Alaw.as_str(), "G.711 A-law");
        assert_eq!(AudioCodec::G711Ulaw.as_str(), "G.711 u-law");
        assert_eq!(AudioCodec::Opus.as_str(), "Opus");
        assert_eq!(AudioCodec::Unknown.as_str(), "Unknown");
    }
}
