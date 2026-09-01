//! Bounded WebTransport datagram packetization and lightweight XOR FEC.
//! Transport I/O is deliberately outside this module so malformed packets can
//! be rejected before allocation in server and browser implementations.

use std::collections::HashMap;

use crate::DependencyClass;

pub const DATAGRAM_MAGIC: u32 = 0x4744_4d42; // "BMDG" little-endian
pub const DATAGRAM_VERSION: u8 = 1;
pub const DATAGRAM_HEADER_SIZE: usize = 40;
pub const DEFAULT_MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAGMENTS: u16 = 4096;
pub const MAX_ENCODED_FRAME: u32 = 16 * 1024 * 1024;

const FLAG_KEY: u8 = 0x01;
const FLAG_PARITY: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatagramHeader {
    pub connection_id: u32,
    pub stream_generation: u32,
    pub frame_sequence: u64,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub total_frame_length: u32,
    pub dependency: DependencyClass,
    pub temporal_id: Option<u8>,
    pub keyframe: bool,
    pub fec_group: u16,
    pub fec_index: u8,
    pub fec_data_count: u8,
    pub parity: bool,
    pub payload_length: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DatagramError {
    #[error("datagram too short")]
    TooShort,
    #[error("invalid datagram magic")]
    BadMagic,
    #[error("unsupported datagram version {0}")]
    Version(u8),
    #[error("invalid dependency {0}")]
    Dependency(u8),
    #[error("invalid fragment bounds")]
    FragmentBounds,
    #[error("encoded frame exceeds configured maximum")]
    FrameTooLarge,
    #[error("declared payload is truncated")]
    Truncated,
}

impl DatagramHeader {
    pub fn serialize(&self, payload: &[u8]) -> Result<Vec<u8>, DatagramError> {
        if self.fragment_count == 0
            || self.fragment_count > MAX_FRAGMENTS
            || self.fragment_index >= self.fragment_count
        {
            return Err(DatagramError::FragmentBounds);
        }
        if self.total_frame_length > MAX_ENCODED_FRAME {
            return Err(DatagramError::FrameTooLarge);
        }
        if payload.len() > u16::MAX as usize {
            return Err(DatagramError::Truncated);
        }
        let mut out = vec![0u8; DATAGRAM_HEADER_SIZE + payload.len()];
        out[0..4].copy_from_slice(&DATAGRAM_MAGIC.to_le_bytes());
        out[4] = DATAGRAM_VERSION;
        out[5] = (if self.keyframe { FLAG_KEY } else { 0 })
            | (if self.parity { FLAG_PARITY } else { 0 });
        out[6] = match self.dependency {
            DependencyClass::Key => 0,
            DependencyClass::Reference => 1,
            DependencyClass::Disposable => 2,
        };
        out[7] = self.temporal_id.unwrap_or(u8::MAX);
        out[8..12].copy_from_slice(&self.connection_id.to_le_bytes());
        out[12..16].copy_from_slice(&self.stream_generation.to_le_bytes());
        out[16..24].copy_from_slice(&self.frame_sequence.to_le_bytes());
        out[24..26].copy_from_slice(&self.fragment_index.to_le_bytes());
        out[26..28].copy_from_slice(&self.fragment_count.to_le_bytes());
        out[28..32].copy_from_slice(&self.total_frame_length.to_le_bytes());
        out[32..34].copy_from_slice(&self.fec_group.to_le_bytes());
        out[34] = self.fec_index;
        out[35] = self.fec_data_count;
        out[36..38].copy_from_slice(&(payload.len() as u16).to_le_bytes());
        out[DATAGRAM_HEADER_SIZE..].copy_from_slice(payload);
        Ok(out)
    }

    pub fn parse(data: &[u8]) -> Result<(Self, &[u8]), DatagramError> {
        if data.len() < DATAGRAM_HEADER_SIZE {
            return Err(DatagramError::TooShort);
        }
        if u32::from_le_bytes(data[0..4].try_into().unwrap()) != DATAGRAM_MAGIC {
            return Err(DatagramError::BadMagic);
        }
        if data[4] != DATAGRAM_VERSION {
            return Err(DatagramError::Version(data[4]));
        }
        let dependency = match data[6] {
            0 => DependencyClass::Key,
            1 => DependencyClass::Reference,
            2 => DependencyClass::Disposable,
            value => return Err(DatagramError::Dependency(value)),
        };
        let fragment_index = u16::from_le_bytes(data[24..26].try_into().unwrap());
        let fragment_count = u16::from_le_bytes(data[26..28].try_into().unwrap());
        let total_frame_length = u32::from_le_bytes(data[28..32].try_into().unwrap());
        let payload_length = u16::from_le_bytes(data[36..38].try_into().unwrap());
        if fragment_count == 0 || fragment_count > MAX_FRAGMENTS || fragment_index >= fragment_count
        {
            return Err(DatagramError::FragmentBounds);
        }
        if total_frame_length > MAX_ENCODED_FRAME {
            return Err(DatagramError::FrameTooLarge);
        }
        if data.len() < DATAGRAM_HEADER_SIZE + payload_length as usize {
            return Err(DatagramError::Truncated);
        }
        let flags = data[5];
        Ok((
            Self {
                connection_id: u32::from_le_bytes(data[8..12].try_into().unwrap()),
                stream_generation: u32::from_le_bytes(data[12..16].try_into().unwrap()),
                frame_sequence: u64::from_le_bytes(data[16..24].try_into().unwrap()),
                fragment_index,
                fragment_count,
                total_frame_length,
                dependency,
                temporal_id: (data[7] != u8::MAX).then_some(data[7]),
                keyframe: flags & FLAG_KEY != 0,
                fec_group: u16::from_le_bytes(data[32..34].try_into().unwrap()),
                fec_index: data[34],
                fec_data_count: data[35],
                parity: flags & FLAG_PARITY != 0,
                payload_length,
            },
            &data[DATAGRAM_HEADER_SIZE..DATAGRAM_HEADER_SIZE + payload_length as usize],
        ))
    }
}

pub fn fragment_frame(
    mut base: DatagramHeader,
    payload: &[u8],
    max_datagram: usize,
) -> Result<Vec<Vec<u8>>, DatagramError> {
    let chunk = max_datagram
        .checked_sub(DATAGRAM_HEADER_SIZE)
        .filter(|v| *v > 0)
        .ok_or(DatagramError::TooShort)?;
    if payload.len() > MAX_ENCODED_FRAME as usize {
        return Err(DatagramError::FrameTooLarge);
    }
    let count = payload.len().div_ceil(chunk).max(1);
    if count > MAX_FRAGMENTS as usize {
        return Err(DatagramError::FragmentBounds);
    }
    base.fragment_count = count as u16;
    base.total_frame_length = payload.len() as u32;
    (0..count)
        .map(|index| {
            let start = index * chunk;
            let end = (start + chunk).min(payload.len());
            base.fragment_index = index as u16;
            base.payload_length = (end - start) as u16;
            base.serialize(&payload[start..end])
        })
        .collect()
}

#[derive(Debug)]
struct PartialFrame {
    deadline_ms: u64,
    header: DatagramHeader,
    fragments: Vec<Option<Vec<u8>>>,
    bytes: usize,
}

/// Reassembly with hard frame/byte/deadline bounds. Old generations and stale
/// frames are discarded rather than retransmitted.
pub struct Reassembler {
    generation: u32,
    max_frames: usize,
    max_bytes: usize,
    bytes: usize,
    frames: HashMap<u64, PartialFrame>,
}

impl Reassembler {
    pub fn new(generation: u32, max_frames: usize, max_bytes: usize) -> Self {
        Self {
            generation,
            max_frames: max_frames.max(1),
            max_bytes,
            bytes: 0,
            frames: HashMap::new(),
        }
    }

    pub fn reset(&mut self, generation: u32) {
        self.frames.clear();
        self.bytes = 0;
        self.generation = generation;
    }

    pub fn expire(&mut self, now_ms: u64) -> usize {
        let expired: Vec<u64> = self
            .frames
            .iter()
            .filter_map(|(seq, frame)| (frame.deadline_ms <= now_ms).then_some(*seq))
            .collect();
        for seq in &expired {
            if let Some(frame) = self.frames.remove(seq) {
                self.bytes = self.bytes.saturating_sub(frame.bytes);
            }
        }
        expired.len()
    }

    pub fn push(
        &mut self,
        header: DatagramHeader,
        payload: &[u8],
        now_ms: u64,
        deadline_ms: u64,
    ) -> Option<(DatagramHeader, Vec<u8>)> {
        self.expire(now_ms);
        if header.stream_generation != self.generation || header.parity {
            return None;
        }
        if payload.len() + self.bytes > self.max_bytes {
            return None;
        }
        if !self.frames.contains_key(&header.frame_sequence) && self.frames.len() >= self.max_frames
        {
            return None;
        }
        let entry = self
            .frames
            .entry(header.frame_sequence)
            .or_insert_with(|| PartialFrame {
                deadline_ms,
                fragments: vec![None; header.fragment_count as usize],
                header: header.clone(),
                bytes: 0,
            });
        if entry.header.fragment_count != header.fragment_count
            || entry.header.total_frame_length != header.total_frame_length
        {
            return None;
        }
        let slot = &mut entry.fragments[header.fragment_index as usize];
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            entry.bytes += payload.len();
            self.bytes += payload.len();
        }
        if entry.fragments.iter().any(Option::is_none) {
            return None;
        }
        let frame = self.frames.remove(&header.frame_sequence)?;
        self.bytes = self.bytes.saturating_sub(frame.bytes);
        let mut assembled = Vec::with_capacity(frame.header.total_frame_length as usize);
        for fragment in frame.fragments {
            assembled.extend(fragment?);
        }
        if assembled.len() != frame.header.total_frame_length as usize {
            return None;
        }
        Some((frame.header, assembled))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XorParity {
    pub lengths: Vec<u16>,
    pub bytes: Vec<u8>,
}

pub fn xor_parity(data: &[&[u8]]) -> Option<XorParity> {
    if data.is_empty() || data.len() > 16 || data.iter().any(|p| p.len() > u16::MAX as usize) {
        return None;
    }
    let max = data.iter().map(|p| p.len()).max().unwrap_or(0);
    let mut bytes = vec![0u8; max];
    for packet in data {
        for (index, byte) in packet.iter().enumerate() {
            bytes[index] ^= byte;
        }
    }
    Some(XorParity {
        lengths: data.iter().map(|p| p.len() as u16).collect(),
        bytes,
    })
}

pub fn xor_recover(packets: &[Option<&[u8]>], parity: &XorParity) -> Option<(usize, Vec<u8>)> {
    if packets.len() != parity.lengths.len() {
        return None;
    }
    let missing: Vec<usize> = packets
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.is_none().then_some(i))
        .collect();
    if missing.len() != 1 {
        return None;
    }
    let missing_index = missing[0];
    let mut out = parity.bytes.clone();
    for packet in packets.iter().flatten() {
        for (index, byte) in packet.iter().enumerate() {
            out[index] ^= byte;
        }
    }
    out.truncate(parity.lengths[missing_index] as usize);
    Some((missing_index, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header() -> DatagramHeader {
        DatagramHeader {
            connection_id: 7,
            stream_generation: 2,
            frame_sequence: 9,
            fragment_index: 0,
            fragment_count: 1,
            total_frame_length: 0,
            dependency: DependencyClass::Reference,
            temporal_id: None,
            keyframe: false,
            fec_group: 0,
            fec_index: 0,
            fec_data_count: 0,
            parity: false,
            payload_length: 0,
        }
    }

    #[test]
    fn packet_roundtrip() {
        let mut h = header();
        h.total_frame_length = 3;
        let bytes = h.serialize(b"abc").unwrap();
        let (parsed, payload) = DatagramHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.connection_id, 7);
        assert_eq!(payload, b"abc");
    }

    #[test]
    fn fragment_and_reassemble_out_of_order() {
        let packets = fragment_frame(header(), &vec![42; 3000], 1200).unwrap();
        let mut reassembler = Reassembler::new(2, 4, 10_000);
        let mut complete = None;
        for packet in packets.iter().rev() {
            let (h, p) = DatagramHeader::parse(packet).unwrap();
            complete = reassembler.push(h, p, 1, 100).or(complete);
        }
        assert_eq!(complete.unwrap().1, vec![42; 3000]);
    }

    #[test]
    fn expiry_releases_partial_frame() {
        let packets = fragment_frame(header(), &vec![1; 2000], 1200).unwrap();
        let (h, p) = DatagramHeader::parse(&packets[0]).unwrap();
        let mut r = Reassembler::new(2, 1, 3000);
        assert!(r.push(h, p, 0, 5).is_none());
        assert_eq!(r.expire(5), 1);
    }

    #[test]
    fn xor_recovers_every_single_position_and_variable_lengths() {
        let data: Vec<&[u8]> = vec![b"alpha", b"b", b"charlie"];
        let parity = xor_parity(&data).unwrap();
        for missing in 0..data.len() {
            let packets: Vec<Option<&[u8]>> = data
                .iter()
                .enumerate()
                .map(|(i, p)| (i != missing).then_some(*p))
                .collect();
            assert_eq!(
                xor_recover(&packets, &parity),
                Some((missing, data[missing].to_vec()))
            );
        }
    }

    #[test]
    fn xor_rejects_two_losses() {
        let parity = xor_parity(&[b"a", b"b", b"c"]).unwrap();
        assert!(xor_recover(&[None, None, Some(b"c")], &parity).is_none());
    }
}
