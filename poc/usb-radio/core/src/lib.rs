#![cfg_attr(not(test), no_std)]

pub mod config_store;

pub const SECTOR_SIZE: usize = 512;

// P3 geometry: FAT16 with 32 KiB clusters and a 1 GiB virtual RADIO.MP3.
// The medium is virtual; only the rolling stream window exists in RAM.
pub const SECTORS_PER_CLUSTER: u32 = 64;
pub const CLUSTER_SIZE: u32 = SECTORS_PER_CLUSTER * SECTOR_SIZE as u32;
pub const FILE_CLUSTER_COUNT: u32 = 32_768;
pub const FILE_START_CLUSTER: u16 = 2;
pub const FILE_LAST_CLUSTER: u16 =
    FILE_START_CLUSTER + FILE_CLUSTER_COUNT as u16 - 1;
pub const FILE_SECTORS: u32 = FILE_CLUSTER_COUNT * SECTORS_PER_CLUSTER;
pub const FILE_SIZE: u32 = FILE_CLUSTER_COUNT * CLUSTER_SIZE; // 1 GiB

pub const RESERVED_SECTORS: u32 = 1;
pub const FAT_COUNT: u32 = 2;
pub const FAT_SECTORS: u32 =
    (((FILE_CLUSTER_COUNT + 2) * 2) + SECTOR_SIZE as u32 - 1) / SECTOR_SIZE as u32;
pub const ROOT_ENTRIES: u32 = 32;
pub const ROOT_SECTORS: u32 =
    (ROOT_ENTRIES * 32 + SECTOR_SIZE as u32 - 1) / SECTOR_SIZE as u32;
pub const DATA_START_LBA: u32 =
    RESERVED_SECTORS + FAT_COUNT * FAT_SECTORS + ROOT_SECTORS;
pub const TOTAL_SECTORS: u32 = DATA_START_LBA + FILE_SECTORS;

const MEDIA_DESCRIPTOR: u8 = 0xF8;
const VOLUME_LABEL: &[u8; 11] = b"RADIOUSB   ";
const FILE_NAME: &[u8; 11] = b"RADIO   MP3";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileReadStatus {
    Ready,
    Pending,
    Expired,
}

/// A read starting more than this many bytes past the live edge of the stream is answered at
/// once with zeroes instead of waiting. The Metronic reads just behind the live edge; hosts such
/// as Windows probe arbitrary offsets (e.g. the tail of the 1 GiB file, ~18 h in the future),
/// which would otherwise block the whole MSC until the stream got there.
pub const FAR_AHEAD_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamWindow {
    /// Fully inside the retained ring window.
    Ready,
    /// Slightly ahead of the live edge: the data will arrive soon, wait for it.
    Pending,
    /// Far ahead of the live edge: answer with zeroes immediately.
    FarAhead,
    /// Older than the retained window.
    Expired,
}

/// Classifies a read of absolute stream bytes `[start_abs, end_abs)` given the live edge
/// `write_abs` and the ring capacity.
pub fn classify_stream_read(
    start_abs: u64,
    end_abs: u64,
    write_abs: u64,
    ring_capacity: u64,
) -> StreamWindow {
    if start_abs > write_abs.saturating_add(FAR_AHEAD_BYTES) {
        StreamWindow::FarAhead
    } else if end_abs > write_abs {
        StreamWindow::Pending
    } else if start_abs < write_abs.saturating_sub(ring_capacity) {
        StreamWindow::Expired
    } else {
        StreamWindow::Ready
    }
}

/// Supplies sectors from the virtual RADIO.MP3.
///
/// A source may use session callbacks to remap file offset 0 to a new position in an
/// infinite backing stream whenever the USB host reconnects.
pub trait FileSource {
    fn begin_session(&mut self) {}
    fn end_session(&mut self) {}

    fn read_file_sector(
        &mut self,
        index: u32,
        out: &mut [u8; SECTOR_SIZE],
    ) -> FileReadStatus;
}

pub struct VirtualFat16<S> {
    source: S,
}

impl<S: FileSource> VirtualFat16<S> {
    pub const fn new(source: S) -> Self {
        Self { source }
    }

    pub const fn last_lba(&self) -> u32 {
        TOTAL_SECTORS - 1
    }

    pub fn begin_session(&mut self) {
        self.source.begin_session();
    }

    pub fn end_session(&mut self) {
        self.source.end_session();
    }

    pub fn read_sector(
        &mut self,
        lba: u32,
        out: &mut [u8; SECTOR_SIZE],
    ) -> FileReadStatus {
        out.fill(0);

        if lba >= TOTAL_SECTORS {
            return FileReadStatus::Expired;
        }

        if lba == 0 {
            write_boot_sector(out);
            return FileReadStatus::Ready;
        }

        let fat1_start = RESERVED_SECTORS;
        let fat2_start = fat1_start + FAT_SECTORS;
        let root_start = fat2_start + FAT_SECTORS;

        if (fat1_start..fat1_start + FAT_SECTORS).contains(&lba) {
            write_fat_sector(lba - fat1_start, out);
            return FileReadStatus::Ready;
        }

        if (fat2_start..fat2_start + FAT_SECTORS).contains(&lba) {
            write_fat_sector(lba - fat2_start, out);
            return FileReadStatus::Ready;
        }

        if (root_start..root_start + ROOT_SECTORS).contains(&lba) {
            write_root_sector(lba - root_start, out);
            return FileReadStatus::Ready;
        }

        if (DATA_START_LBA..DATA_START_LBA + FILE_SECTORS).contains(&lba) {
            return self.source.read_file_sector(lba - DATA_START_LBA, out);
        }

        FileReadStatus::Ready
    }
}

#[derive(Default)]
pub struct DiagnosticSource;

impl FileSource for DiagnosticSource {
    fn read_file_sector(
        &mut self,
        index: u32,
        out: &mut [u8; SECTOR_SIZE],
    ) -> FileReadStatus {
        out.fill((index & 0xff) as u8);

        if index == 0 {
            out[..10].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
            out[16..32].copy_from_slice(b"IOBEWI-USB-RADIO");
        }

        out[SECTOR_SIZE - 4..].copy_from_slice(&index.to_le_bytes());
        FileReadStatus::Ready
    }
}

fn write_boot_sector(out: &mut [u8; SECTOR_SIZE]) {
    out[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    out[3..11].copy_from_slice(b"IOBEWI  ");

    put_u16(out, 11, SECTOR_SIZE as u16);
    out[13] = SECTORS_PER_CLUSTER as u8;
    put_u16(out, 14, RESERVED_SECTORS as u16);
    out[16] = FAT_COUNT as u8;
    put_u16(out, 17, ROOT_ENTRIES as u16);
    put_u16(out, 19, 0); // volume is too large for BPB_TotSec16
    out[21] = MEDIA_DESCRIPTOR;
    put_u16(out, 22, FAT_SECTORS as u16);
    put_u16(out, 24, 63);
    put_u16(out, 26, 255);
    put_u32(out, 28, 0);
    put_u32(out, 32, TOTAL_SECTORS);

    out[36] = 0x80;
    out[38] = 0x29;
    put_u32(out, 39, 0x5241_4449); // "RADI"
    out[43..54].copy_from_slice(VOLUME_LABEL);
    out[54..62].copy_from_slice(b"FAT16   ");

    out[510] = 0x55;
    out[511] = 0xAA;
}

fn write_fat_sector(fat_sector: u32, out: &mut [u8; SECTOR_SIZE]) {
    let first_entry = fat_sector * (SECTOR_SIZE as u32 / 2);

    for slot in 0..(SECTOR_SIZE / 2) {
        let entry = first_entry + slot as u32;
        let value = if entry <= u16::MAX as u32 {
            fat_value(entry as u16)
        } else {
            0
        };
        let offset = slot * 2;
        out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
}

fn fat_value(cluster: u16) -> u16 {
    match cluster {
        0 => 0xFFF8,
        1 => 0xFFFF,
        c if c >= FILE_START_CLUSTER && c < FILE_LAST_CLUSTER => c + 1,
        c if c == FILE_LAST_CLUSTER => 0xFFFF,
        _ => 0x0000,
    }
}

fn write_root_sector(index: u32, out: &mut [u8; SECTOR_SIZE]) {
    if index != 0 {
        return;
    }

    out[0..11].copy_from_slice(VOLUME_LABEL);
    out[11] = 0x08;

    let e = 32;
    out[e..e + 11].copy_from_slice(FILE_NAME);
    out[e + 11] = 0x21; // read-only + archive
    put_u16(out, e + 26, FILE_START_CLUSTER);
    put_u32(out, e + 28, FILE_SIZE);
}

fn put_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p3_geometry_is_valid_fat16() {
        let data_sectors = TOTAL_SECTORS - DATA_START_LBA;
        let cluster_count = data_sectors / SECTORS_PER_CLUSTER;

        assert_eq!(SECTORS_PER_CLUSTER, 64);
        assert_eq!(CLUSTER_SIZE, 32 * 1024);
        assert_eq!(FILE_SIZE, 1024 * 1024 * 1024);
        assert_eq!(cluster_count, FILE_CLUSTER_COUNT);
        assert!((4085..65525).contains(&cluster_count));
        assert!(FILE_LAST_CLUSTER < 0xFFF0);
    }

    #[test]
    fn boot_sector_is_fat16_and_uses_32bit_total_sectors() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];
        assert_eq!(disk.read_sector(0, &mut sector), FileReadStatus::Ready);

        assert_eq!(&sector[3..11], b"IOBEWI  ");
        assert_eq!(u16::from_le_bytes([sector[11], sector[12]]), 512);
        assert_eq!(sector[13] as u32, SECTORS_PER_CLUSTER);
        assert_eq!(u16::from_le_bytes([sector[19], sector[20]]), 0);
        assert_eq!(
            u32::from_le_bytes([sector[32], sector[33], sector[34], sector[35]]),
            TOTAL_SECTORS
        );
        assert_eq!(&sector[54..62], b"FAT16   ");
        assert_eq!(&sector[510..512], &[0x55, 0xAA]);
    }

    #[test]
    fn root_contains_one_gib_radio_mp3() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];
        let root_lba = RESERVED_SECTORS + FAT_COUNT * FAT_SECTORS;
        disk.read_sector(root_lba, &mut sector);

        assert_eq!(&sector[32..43], b"RADIO   MP3");
        assert_eq!(u16::from_le_bytes([sector[58], sector[59]]), FILE_START_CLUSTER);
        assert_eq!(
            u32::from_le_bytes([sector[60], sector[61], sector[62], sector[63]]),
            FILE_SIZE
        );
    }

    #[test]
    fn file_cluster_chain_is_contiguous_and_terminated() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];

        disk.read_sector(RESERVED_SECTORS, &mut sector);
        assert_eq!(u16::from_le_bytes([sector[4], sector[5]]), 3);

        let fat_offset = FILE_LAST_CLUSTER as u32 * 2;
        let fat_sector = fat_offset / SECTOR_SIZE as u32;
        let in_sector = (fat_offset % SECTOR_SIZE as u32) as usize;
        disk.read_sector(RESERVED_SECTORS + fat_sector, &mut sector);
        assert_eq!(
            u16::from_le_bytes([sector[in_sector], sector[in_sector + 1]]),
            0xFFFF
        );
    }

    #[test]
    fn file_sector_mapping_is_still_sector_granular() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];

        disk.read_sector(DATA_START_LBA + 7, &mut sector);
        assert_eq!(sector[0], 7);
        assert_eq!(&sector[SECTOR_SIZE - 4..], &7u32.to_le_bytes());
    }

    #[test]
    fn stream_read_classification() {
        const RING: u64 = 96 * 1024;
        let live = 2 * 1024 * 1024;
        // Inside the window.
        assert_eq!(classify_stream_read(live - 4096, live - 3584, live, RING), StreamWindow::Ready);
        // Just past the live edge (the Metronic pacing case): wait.
        assert_eq!(classify_stream_read(live, live + 512, live, RING), StreamWindow::Pending);
        assert_eq!(classify_stream_read(live - 256, live + 256, live, RING), StreamWindow::Pending);
        assert_eq!(
            classify_stream_read(live + FAR_AHEAD_BYTES, live + FAR_AHEAD_BYTES + 512, live, RING),
            StreamWindow::Pending
        );
        // Windows tail probe of the 1 GiB file: answered at once.
        let tail = FILE_SIZE as u64 - 512;
        assert_eq!(classify_stream_read(tail, tail + 512, live, RING), StreamWindow::FarAhead);
        assert_eq!(
            classify_stream_read(live + FAR_AHEAD_BYTES + 1, live + FAR_AHEAD_BYTES + 513, live, RING),
            StreamWindow::FarAhead
        );
        // Older than the retained window.
        assert_eq!(classify_stream_read(0, 512, live, RING), StreamWindow::Expired);
    }
}
