//! Power-cut-safe record format for the two-slot flash config store.
//!
//! Pure logic (no flash access): the firmware reads/writes the slots, this module frames
//! records and decides which slot is newest. A record is only valid if magic and CRC match,
//! so a write interrupted by a power cut leaves the older slot as the winner.
//!
//! ```text
//! "UCF1" | generation u64 LE | flags u8 | space_len u8 | data_len u16 LE | space | data | crc32 LE
//! ```
//! The record is padded with 0xFF to a multiple of 4 bytes (flash write size); the CRC covers
//! everything before it, not the padding.

pub const MAGIC: [u8; 4] = *b"UCF1";
pub const HEADER_LEN: usize = 4 + 8 + 1 + 1 + 2;
pub const CRC_LEN: usize = 4;
/// Upper bound the firmware reads per slot; the Wi-Fi record is far smaller.
pub const MAX_RECORD_LEN: usize = 512;
const FLAG_CLEARED: u8 = 1;

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Length of the encoded record including 0xFF padding to 4 bytes.
pub fn encoded_len(space: &str, data: &[u8]) -> usize {
    (HEADER_LEN + space.len() + data.len() + CRC_LEN).next_multiple_of(4)
}

/// Encode into `out`; returns the padded length, or `None` if it does not fit
/// or a field is out of range.
pub fn encode(
    out: &mut [u8],
    generation: u64,
    space: &str,
    data: &[u8],
    cleared: bool,
) -> Option<usize> {
    let space_len = u8::try_from(space.len()).ok()?;
    let data_len = u16::try_from(data.len()).ok()?;
    let total = encoded_len(space, data);
    if total > MAX_RECORD_LEN || total > out.len() {
        return None;
    }
    out[0..4].copy_from_slice(&MAGIC);
    out[4..12].copy_from_slice(&generation.to_le_bytes());
    out[12] = if cleared { FLAG_CLEARED } else { 0 };
    out[13] = space_len;
    out[14..16].copy_from_slice(&data_len.to_le_bytes());
    let mut at = HEADER_LEN;
    out[at..at + space.len()].copy_from_slice(space.as_bytes());
    at += space.len();
    out[at..at + data.len()].copy_from_slice(data);
    at += data.len();
    let crc = crc32(&out[..at]);
    out[at..at + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
    at += CRC_LEN;
    out[at..total].fill(0xFF);
    Some(total)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decoded<'a> {
    pub generation: u64,
    pub space: &'a [u8],
    pub data: &'a [u8],
    pub cleared: bool,
}

/// Total record length (unpadded) announced by a header, if the header is plausible.
pub fn announced_len(header: &[u8]) -> Option<usize> {
    if header.len() < HEADER_LEN || header[0..4] != MAGIC {
        return None;
    }
    let space_len = header[13] as usize;
    let data_len = u16::from_le_bytes([header[14], header[15]]) as usize;
    let total = HEADER_LEN + space_len + data_len + CRC_LEN;
    (total <= MAX_RECORD_LEN).then_some(total)
}

/// Decode and verify a record; `None` for blank (0xFF), torn or corrupt slots.
pub fn decode(raw: &[u8]) -> Option<Decoded<'_>> {
    let total = announced_len(raw)?;
    if raw.len() < total {
        return None;
    }
    let crc_at = total - CRC_LEN;
    let stored = u32::from_le_bytes(raw[crc_at..total].try_into().ok()?);
    if crc32(&raw[..crc_at]) != stored {
        return None;
    }
    let space_len = raw[13] as usize;
    let data_len = u16::from_le_bytes([raw[14], raw[15]]) as usize;
    let space_at = HEADER_LEN;
    let data_at = space_at + space_len;
    Some(Decoded {
        generation: u64::from_le_bytes(raw[4..12].try_into().ok()?),
        space: &raw[space_at..data_at],
        data: &raw[data_at..data_at + data_len],
        cleared: raw[12] & FLAG_CLEARED != 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

/// Slot holding the newest valid record, given each slot's generation (`None` = invalid).
pub fn newest(a: Option<u64>, b: Option<u64>) -> Option<Slot> {
    match (a, b) {
        (None, None) => None,
        (Some(_), None) => Some(Slot::A),
        (None, Some(_)) => Some(Slot::B),
        (Some(x), Some(y)) => Some(if y > x { Slot::B } else { Slot::A }),
    }
}

/// Slot to overwrite for the next commit: never the one holding the newest record.
pub fn write_target(a: Option<u64>, b: Option<u64>) -> Slot {
    match newest(a, b) {
        Some(Slot::A) => Slot::B,
        _ => Slot::A,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_padding() {
        let mut buf = [0u8; MAX_RECORD_LEN];
        let n = encode(&mut buf, 7, "wifi", b"WFC1abc", false).unwrap();
        assert_eq!(n % 4, 0);
        let d = decode(&buf[..n]).unwrap();
        assert_eq!((d.generation, d.space, d.data, d.cleared), (7, &b"wifi"[..], &b"WFC1abc"[..], false));
    }

    #[test]
    fn blank_flash_and_torn_writes_are_invalid() {
        assert!(decode(&[0xFF; 64]).is_none());
        let mut buf = [0u8; MAX_RECORD_LEN];
        let n = encode(&mut buf, 1, "wifi", b"secret", false).unwrap();
        // Torn write: tail of the record never reached the flash.
        buf[n - 8..n].fill(0xFF);
        assert!(decode(&buf[..n]).is_none());
        // Bit flip in the payload.
        let n = encode(&mut buf, 1, "wifi", b"secret", false).unwrap();
        buf[HEADER_LEN + 4] ^= 1;
        assert!(decode(&buf[..n]).is_none());
    }

    #[test]
    fn cleared_record_decodes_as_cleared() {
        let mut buf = [0u8; MAX_RECORD_LEN];
        let n = encode(&mut buf, 3, "wifi", &[], true).unwrap();
        assert!(decode(&buf[..n]).unwrap().cleared);
    }

    #[test]
    fn rejects_oversized_input() {
        let mut buf = [0u8; MAX_RECORD_LEN];
        assert!(encode(&mut buf, 1, "wifi", &[0u8; MAX_RECORD_LEN], false).is_none());
    }

    #[test]
    fn slot_selection_never_overwrites_the_newest() {
        assert_eq!(newest(None, None), None);
        assert_eq!(write_target(None, None), Slot::A);
        assert_eq!(newest(Some(4), Some(5)), Some(Slot::B));
        assert_eq!(write_target(Some(4), Some(5)), Slot::A);
        assert_eq!(newest(Some(6), Some(5)), Some(Slot::A));
        assert_eq!(write_target(Some(6), Some(5)), Slot::B);
        // Only one valid slot (e.g. the other write was torn): keep it, write the other.
        assert_eq!(write_target(Some(9), None), Slot::B);
        assert_eq!(write_target(None, Some(9)), Slot::A);
    }
}
