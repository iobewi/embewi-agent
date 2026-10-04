#![cfg_attr(not(test), no_std)]

pub const SECTOR_SIZE: usize = 512;
pub const TOTAL_SECTORS: u32 = 8192; // 4 MiB
pub const RESERVED_SECTORS: u32 = 1;
pub const FAT_COUNT: u32 = 2;
pub const FAT_SECTORS: u32 = 32;
pub const ROOT_ENTRIES: u32 = 32;
pub const ROOT_SECTORS: u32 = (ROOT_ENTRIES * 32 + SECTOR_SIZE as u32 - 1) / SECTOR_SIZE as u32;
pub const DATA_START_LBA: u32 = RESERVED_SECTORS + FAT_COUNT * FAT_SECTORS + ROOT_SECTORS;

pub const FILE_START_CLUSTER: u16 = 2;
pub const FILE_SECTORS: u32 = 4096; // 2 MiB at one sector/cluster
pub const FILE_SIZE: u32 = FILE_SECTORS * SECTOR_SIZE as u32;
pub const FILE_LAST_CLUSTER: u16 = FILE_START_CLUSTER + FILE_SECTORS as u16 - 1;

const MEDIA_DESCRIPTOR: u8 = 0xF8;
const VOLUME_LABEL: &[u8; 11] = b"RADIOUSB   ";
const FILE_NAME: &[u8; 11] = b"RADIO   MP3";

/// Supplies one 512-byte sector of the virtual RADIO.MP3 contents.
///
/// P0 uses a diagnostic pattern. P2 can use a static MP3. P3 can supply sectors from a
/// rolling network-backed window without changing the FAT or MSC layers.
pub trait FileSource {
    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]);
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

    pub fn read_sector(&mut self, lba: u32, out: &mut [u8; SECTOR_SIZE]) {
        out.fill(0);

        if lba >= TOTAL_SECTORS {
            return;
        }

        if lba == 0 {
            write_boot_sector(out);
            return;
        }

        let fat1_start = RESERVED_SECTORS;
        let fat2_start = fat1_start + FAT_SECTORS;
        let root_start = fat2_start + FAT_SECTORS;

        if (fat1_start..fat1_start + FAT_SECTORS).contains(&lba) {
            write_fat_sector(lba - fat1_start, out);
            return;
        }

        if (fat2_start..fat2_start + FAT_SECTORS).contains(&lba) {
            write_fat_sector(lba - fat2_start, out);
            return;
        }

        if (root_start..root_start + ROOT_SECTORS).contains(&lba) {
            write_root_sector(lba - root_start, out);
            return;
        }

        if (DATA_START_LBA..DATA_START_LBA + FILE_SECTORS).contains(&lba) {
            self.source.read_file_sector(lba - DATA_START_LBA, out);
        }
    }
}

/// Diagnostic P0 source. It is intentionally not valid audio.
///
/// Sector 0 starts with a small ID3-shaped marker so a hex dump immediately identifies
/// the virtual file; the rest is a deterministic sector/index pattern.
#[derive(Default)]
pub struct DiagnosticSource;

impl FileSource for DiagnosticSource {
    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]) {
        out.fill((index & 0xff) as u8);

        if index == 0 {
            out[..10].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
            out[16..32].copy_from_slice(b"IOBEWI-USB-RADIO");
        }

        out[SECTOR_SIZE - 4..].copy_from_slice(&index.to_le_bytes());
    }
}

fn write_boot_sector(out: &mut [u8; SECTOR_SIZE]) {
    out[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    out[3..11].copy_from_slice(b"IOBEWI  ");

    put_u16(out, 11, SECTOR_SIZE as u16);
    out[13] = 1; // sectors per cluster
    put_u16(out, 14, RESERVED_SECTORS as u16);
    out[16] = FAT_COUNT as u8;
    put_u16(out, 17, ROOT_ENTRIES as u16);
    put_u16(out, 19, TOTAL_SECTORS as u16);
    out[21] = MEDIA_DESCRIPTOR;
    put_u16(out, 22, FAT_SECTORS as u16);
    put_u16(out, 24, 63); // sectors/track, conventional removable-media value
    put_u16(out, 26, 255); // heads
    put_u32(out, 28, 0); // hidden sectors
    put_u32(out, 32, 0); // total sectors fits in BPB_TotSec16

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
        let value = fat_value(entry as u16);
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

    // Volume label.
    out[0..11].copy_from_slice(VOLUME_LABEL);
    out[11] = 0x08;

    // RADIO.MP3.
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
    fn boot_sector_is_fat16_and_geometry_is_consistent() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];
        disk.read_sector(0, &mut sector);

        assert_eq!(&sector[3..11], b"IOBEWI  ");
        assert_eq!(u16::from_le_bytes([sector[11], sector[12]]), 512);
        assert_eq!(sector[13], 1);
        assert_eq!(u16::from_le_bytes([sector[19], sector[20]]) as u32, TOTAL_SECTORS);
        assert_eq!(&sector[54..62], b"FAT16   ");
        assert_eq!(&sector[510..512], &[0x55, 0xAA]);

        let cluster_count = TOTAL_SECTORS - DATA_START_LBA;
        assert!((4085..65525).contains(&cluster_count), "must classify as FAT16");
    }

    #[test]
    fn root_contains_radio_mp3() {
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

        // First FAT sector includes clusters 0..255.
        disk.read_sector(RESERVED_SECTORS, &mut sector);
        let c2 = u16::from_le_bytes([sector[4], sector[5]]);
        assert_eq!(c2, 3);

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
    fn first_file_sector_contains_diagnostic_marker_without_panicking() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];
        disk.read_sector(DATA_START_LBA, &mut sector);

        assert_eq!(&sector[..3], b"ID3");
        assert_eq!(&sector[16..32], b"IOBEWI-USB-RADIO");
    }

    #[test]
    fn file_data_maps_one_sector_per_cluster() {
        let mut disk = VirtualFat16::new(DiagnosticSource);
        let mut sector = [0u8; SECTOR_SIZE];
        disk.read_sector(DATA_START_LBA + 7, &mut sector);

        assert_eq!(sector[0], 7);
        assert_eq!(&sector[SECTOR_SIZE - 4..], &7u32.to_le_bytes());
    }
}
