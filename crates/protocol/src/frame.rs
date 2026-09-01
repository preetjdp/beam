//! Binary video/audio frame header for WebSocket transport.
//!
//! Version 1 is the original 24-byte compatibility header. Version 2 adds a
//! 24-byte dependency/timing extension while keeping the first 24 bytes stable:
//! ```text
//! [0..4]   magic: 0x42454156 ("BEAV")
//! [4]      version: 1 or 2
//! [5]      flags: bit 0 = keyframe, bit 1 = audio
//! [6..8]   width (u16)
//! [8..10]  height (u16)
//! [10..12] v2 header size (48); reserved zero in v1
//! [12..20] capture timestamp_us (u64)
//! [20..24] payload_length (u32)
//! [24..28] stream generation (v2)
//! [28]     dependency class: key/reference/disposable (v2)
//! [29]     codec: H.264/HEVC (v2)
//! [30]     temporal id, 255 when unavailable (v2)
//! [31]     timing flags (v2)
//! [32..40] frame sequence (v2)
//! [40..44] encode-complete delta from capture, microseconds (v2, optional)
//! [44..48] agent-send delta from capture, microseconds (v2, optional)
//! [header_size..] payload
//! ```

use crate::{Codec, DependencyClass};

pub const FRAME_HEADER_SIZE: usize = 24;
pub const FRAME_V2_HEADER_SIZE: usize = 48;
pub const FRAME_MAGIC: u32 = 0x5641_4542; // "BEAV" in LE
pub const FRAME_VERSION: u8 = 1;
pub const FRAME_VERSION_EXTENDED: u8 = 2;

const TIMING_ENCODE_COMPLETE: u8 = 0x01;
const TIMING_AGENT_SEND: u8 = 0x02;

pub const FLAG_KEYFRAME: u8 = 0x01;
pub const FLAG_AUDIO: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameExtension {
    pub stream_generation: u32,
    pub frame_sequence: u64,
    pub dependency: DependencyClass,
    pub codec: Codec,
    pub temporal_id: Option<u8>,
    pub encode_complete_delta_us: Option<u32>,
    pub agent_send_delta_us: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameHeader {
    pub flags: u8,
    pub width: u16,
    pub height: u16,
    pub timestamp_us: u64,
    pub payload_length: u32,
    /// Present only for negotiated version-2 frames.
    pub extension: Option<FrameExtension>,
}

impl VideoFrameHeader {
    /// Create a new video frame header.
    pub fn video(
        width: u16,
        height: u16,
        timestamp_us: u64,
        payload_length: u32,
        keyframe: bool,
    ) -> Self {
        Self {
            flags: if keyframe { FLAG_KEYFRAME } else { 0 },
            width,
            height,
            timestamp_us,
            payload_length,
            extension: None,
        }
    }

    /// Create a new audio frame header.
    pub fn audio(timestamp_us: u64, payload_length: u32) -> Self {
        Self {
            flags: FLAG_AUDIO,
            width: 0,
            height: 0,
            timestamp_us,
            payload_length,
            extension: None,
        }
    }

    /// Attach the negotiated version-2 identity/dependency extension.
    pub fn with_extension(mut self, extension: FrameExtension) -> Self {
        self.extension = Some(extension);
        self
    }

    pub fn version(&self) -> u8 {
        if self.extension.is_some() {
            FRAME_VERSION_EXTENDED
        } else {
            FRAME_VERSION
        }
    }

    pub fn header_size(&self) -> usize {
        if self.extension.is_some() {
            FRAME_V2_HEADER_SIZE
        } else {
            FRAME_HEADER_SIZE
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn is_audio(&self) -> bool {
        self.flags & FLAG_AUDIO != 0
    }

    /// Serialize header to 24-byte little-endian buffer.
    pub fn serialize(&self, buf: &mut [u8; FRAME_HEADER_SIZE]) {
        buf[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf[4] = FRAME_VERSION;
        buf[5] = self.flags;
        buf[6..8].copy_from_slice(&self.width.to_le_bytes());
        buf[8..10].copy_from_slice(&self.height.to_le_bytes());
        buf[10..12].copy_from_slice(&0u16.to_le_bytes()); // reserved
        buf[12..20].copy_from_slice(&self.timestamp_us.to_le_bytes());
        buf[20..24].copy_from_slice(&self.payload_length.to_le_bytes());
    }

    /// Serialize header + payload into a single Vec. Version 1 remains byte-for-
    /// byte compatible; attaching an extension selects version 2.
    pub fn serialize_with_payload(&self, payload: &[u8]) -> Vec<u8> {
        let header_size = self.header_size();
        let mut buf = vec![0u8; header_size + payload.len()];
        let mut base = [0u8; FRAME_HEADER_SIZE];
        self.serialize(&mut base);
        if let Some(ext) = &self.extension {
            base[4] = FRAME_VERSION_EXTENDED;
            base[10..12].copy_from_slice(&(FRAME_V2_HEADER_SIZE as u16).to_le_bytes());
            buf[24..28].copy_from_slice(&ext.stream_generation.to_le_bytes());
            buf[28] = match ext.dependency {
                DependencyClass::Key => 0,
                DependencyClass::Reference => 1,
                DependencyClass::Disposable => 2,
            };
            buf[29] = match ext.codec {
                Codec::H264 => 0,
                Codec::Hevc => 1,
            };
            buf[30] = ext.temporal_id.unwrap_or(u8::MAX);
            let mut timing_flags = 0u8;
            if ext.encode_complete_delta_us.is_some() {
                timing_flags |= TIMING_ENCODE_COMPLETE;
            }
            if ext.agent_send_delta_us.is_some() {
                timing_flags |= TIMING_AGENT_SEND;
            }
            buf[31] = timing_flags;
            buf[32..40].copy_from_slice(&ext.frame_sequence.to_le_bytes());
            buf[40..44].copy_from_slice(&ext.encode_complete_delta_us.unwrap_or(0).to_le_bytes());
            buf[44..48].copy_from_slice(&ext.agent_send_delta_us.unwrap_or(0).to_le_bytes());
        }
        buf[..FRAME_HEADER_SIZE].copy_from_slice(&base);
        buf[header_size..].copy_from_slice(payload);
        buf
    }

    /// Deserialize header from a byte slice (must be at least 24 bytes).
    pub fn deserialize(buf: &[u8]) -> Result<Self, FrameError> {
        if buf.len() < FRAME_HEADER_SIZE {
            return Err(FrameError::TooShort(buf.len()));
        }

        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != FRAME_MAGIC {
            return Err(FrameError::BadMagic(magic));
        }

        let version = buf[4];
        if version != FRAME_VERSION && version != FRAME_VERSION_EXTENDED {
            return Err(FrameError::UnsupportedVersion(version));
        }

        let extension = if version == FRAME_VERSION_EXTENDED {
            let declared = u16::from_le_bytes([buf[10], buf[11]]) as usize;
            if declared < FRAME_V2_HEADER_SIZE || buf.len() < declared {
                return Err(FrameError::MalformedExtension {
                    declared,
                    actual: buf.len(),
                });
            }
            let dependency = match buf[28] {
                0 => DependencyClass::Key,
                1 => DependencyClass::Reference,
                2 => DependencyClass::Disposable,
                value => return Err(FrameError::BadDependency(value)),
            };
            let codec = match buf[29] {
                0 => Codec::H264,
                1 => Codec::Hevc,
                value => return Err(FrameError::BadCodec(value)),
            };
            let timing_flags = buf[31];
            Some(FrameExtension {
                stream_generation: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
                frame_sequence: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
                dependency,
                codec,
                temporal_id: (buf[30] != u8::MAX).then_some(buf[30]),
                encode_complete_delta_us: (timing_flags & TIMING_ENCODE_COMPLETE != 0)
                    .then(|| u32::from_le_bytes(buf[40..44].try_into().unwrap())),
                agent_send_delta_us: (timing_flags & TIMING_AGENT_SEND != 0)
                    .then(|| u32::from_le_bytes(buf[44..48].try_into().unwrap())),
            })
        } else {
            None
        };

        Ok(Self {
            flags: buf[5],
            width: u16::from_le_bytes([buf[6], buf[7]]),
            height: u16::from_le_bytes([buf[8], buf[9]]),
            timestamp_us: u64::from_le_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ]),
            payload_length: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
            extension,
        })
    }

    /// Validate that the buffer contains a complete frame (header + payload).
    pub fn validate_complete(buf: &[u8]) -> Result<(), FrameError> {
        let header = Self::deserialize(buf)?;
        let header_size = header.header_size();
        let expected = header_size + header.payload_length as usize;
        if buf.len() < expected {
            return Err(FrameError::IncompletePayload {
                expected: header.payload_length as usize,
                actual: buf.len().saturating_sub(header_size),
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("buffer too short: {0} bytes (need at least {FRAME_HEADER_SIZE})")]
    TooShort(usize),
    #[error("bad magic: 0x{0:08x} (expected 0x{FRAME_MAGIC:08x})")]
    BadMagic(u32),
    #[error("unsupported version: {0} (expected 1 or 2)")]
    UnsupportedVersion(u8),
    #[error("malformed frame extension: declared header {declared} bytes, buffer has {actual}")]
    MalformedExtension { declared: usize, actual: usize },
    #[error("invalid dependency class: {0}")]
    BadDependency(u8),
    #[error("invalid codec id: {0}")]
    BadCodec(u8),
    #[error("incomplete payload: expected {expected} bytes, got {actual}")]
    IncompletePayload { expected: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_roundtrip() {
        let header = VideoFrameHeader::video(1920, 1080, 123456, 65536, true);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
        assert!(parsed.is_keyframe());
        assert!(!parsed.is_audio());
    }

    #[test]
    fn extended_header_roundtrip() {
        let payload = [1, 2, 3, 4];
        let header = VideoFrameHeader::video(1920, 1080, 123_456, payload.len() as u32, false)
            .with_extension(FrameExtension {
                stream_generation: 7,
                frame_sequence: 99,
                dependency: DependencyClass::Reference,
                codec: Codec::H264,
                temporal_id: Some(1),
                encode_complete_delta_us: Some(2_000),
                agent_send_delta_us: Some(2_500),
            });
        let bytes = header.serialize_with_payload(&payload);
        assert_eq!(bytes.len(), FRAME_V2_HEADER_SIZE + payload.len());
        let parsed = VideoFrameHeader::deserialize(&bytes).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(&bytes[parsed.header_size()..], &payload);
        assert!(VideoFrameHeader::validate_complete(&bytes).is_ok());
    }

    #[test]
    fn malformed_extended_header_is_rejected() {
        let mut bytes = [0u8; FRAME_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        bytes[4] = FRAME_VERSION_EXTENDED;
        bytes[10..12].copy_from_slice(&(FRAME_V2_HEADER_SIZE as u16).to_le_bytes());
        assert!(matches!(
            VideoFrameHeader::deserialize(&bytes),
            Err(FrameError::MalformedExtension { .. })
        ));
    }

    #[test]
    fn audio_header_roundtrip() {
        let header = VideoFrameHeader::audio(999999, 480);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
        assert!(!parsed.is_keyframe());
        assert!(parsed.is_audio());
    }

    #[test]
    fn p_frame_no_keyframe_flag() {
        let header = VideoFrameHeader::video(1920, 1080, 0, 1024, false);
        assert!(!header.is_keyframe());
        assert!(!header.is_audio());
        assert_eq!(header.flags, 0);
    }

    #[test]
    fn serialize_with_payload() {
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let header = VideoFrameHeader::video(640, 480, 42, 4, true);
        let buf = header.serialize_with_payload(&payload);
        assert_eq!(buf.len(), FRAME_HEADER_SIZE + 4);
        // Verify header
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(parsed.width, 640);
        assert_eq!(parsed.height, 480);
        assert_eq!(parsed.timestamp_us, 42);
        assert_eq!(parsed.payload_length, 4);
        // Verify payload
        assert_eq!(&buf[FRAME_HEADER_SIZE..], &payload);
    }

    #[test]
    fn deserialize_too_short() {
        let buf = [0u8; 10];
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::TooShort(10)) => {}
            other => panic!("expected TooShort(10), got {:?}", other),
        }
    }

    #[test]
    fn deserialize_bad_magic() {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::BadMagic(0xDEADBEEF)) => {}
            other => panic!("expected BadMagic, got {:?}", other),
        }
    }

    #[test]
    fn deserialize_bad_version() {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf[4] = 99;
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {:?}", other),
        }
    }

    #[test]
    fn validate_complete_ok() {
        let payload = vec![0u8; 100];
        let header = VideoFrameHeader::video(1920, 1080, 0, 100, false);
        let buf = header.serialize_with_payload(&payload);
        assert!(VideoFrameHeader::validate_complete(&buf).is_ok());
    }

    #[test]
    fn validate_complete_incomplete_payload() {
        let payload = vec![0u8; 50];
        let header = VideoFrameHeader::video(1920, 1080, 0, 100, false);
        let buf = header.serialize_with_payload(&payload);
        // Truncate — header says 100 bytes but only 50 present
        match VideoFrameHeader::validate_complete(&buf) {
            Err(FrameError::IncompletePayload {
                expected: 100,
                actual: 50,
            }) => {}
            other => panic!("expected IncompletePayload, got {:?}", other),
        }
    }

    #[test]
    fn magic_bytes_spell_beav() {
        let bytes = FRAME_MAGIC.to_le_bytes();
        assert_eq!(&bytes, b"BEAV");
    }

    #[test]
    fn header_size_is_24() {
        assert_eq!(FRAME_HEADER_SIZE, 24);
    }

    #[test]
    fn roundtrip_max_values() {
        let header = VideoFrameHeader::video(u16::MAX, u16::MAX, u64::MAX, u32::MAX, true);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
    }

    #[test]
    fn roundtrip_zero_values() {
        let header = VideoFrameHeader::video(0, 0, 0, 0, false);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
    }

    #[test]
    fn reserved_bytes_ignored_on_deserialize() {
        // Ensure arbitrary values in the reserved field [10..12] do not cause failure.
        // This is important for forward compatibility if future protocol versions use
        // those bytes.
        let header = VideoFrameHeader::video(1920, 1080, 42, 100, true);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);

        // Mutate the reserved bytes to non-zero
        buf[10] = 0xFF;
        buf[11] = 0xAB;

        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(parsed.width, 1920);
        assert_eq!(parsed.height, 1080);
        assert_eq!(parsed.timestamp_us, 42);
        assert_eq!(parsed.payload_length, 100);
    }
}
