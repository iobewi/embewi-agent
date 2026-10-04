use core::mem::MaybeUninit;

use embassy_usb::{
    Builder, Handler,
    control::{self, InResponse, OutResponse, Recipient, RequestType},
    driver::{Driver, Endpoint, EndpointError, EndpointIn, EndpointOut},
    types::InterfaceNumber,
};
use embassy_time::{Duration, Instant, Timer};
use usb_radio_core::{FileReadStatus, FileSource, SECTOR_SIZE, TOTAL_SECTORS, VirtualFat16};

const USB_CLASS_MASS_STORAGE: u8 = 0x08;
const MSC_SUBCLASS_SCSI_TRANSPARENT: u8 = 0x06;
const MSC_PROTOCOL_BULK_ONLY: u8 = 0x50;

const MSC_REQ_GET_MAX_LUN: u8 = 0xFE;
const MSC_REQ_BULK_ONLY_RESET: u8 = 0xFF;

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;

const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_START_STOP_UNIT: u8 = 0x1B;
const SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
const SCSI_READ_FORMAT_CAPACITIES: u8 = 0x23;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;

/// Longest wait for stream data inside one READ(10) sector (Windows gave up after ~20 s).
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

const SENSE_ILLEGAL_REQUEST: u8 = 0x05;
const ASC_INVALID_OPCODE: u8 = 0x20;
const ASC_LBA_OUT_OF_RANGE: u8 = 0x21;

pub struct State<'a> {
    control: MaybeUninit<Control>,
    _lifetime: core::marker::PhantomData<&'a ()>,
}

impl<'a> State<'a> {
    pub const fn new() -> Self {
        Self {
            control: MaybeUninit::uninit(),
            _lifetime: core::marker::PhantomData,
        }
    }
}

struct Control {
    interface: InterfaceNumber,
}

impl Handler for Control {
    fn control_out(&mut self, req: control::Request, _data: &[u8]) -> Option<OutResponse> {
        if (req.request_type, req.recipient, req.index)
            != (
                RequestType::Class,
                Recipient::Interface,
                self.interface.0 as u16,
            )
        {
            return None;
        }

        match req.request {
            MSC_REQ_BULK_ONLY_RESET if req.value == 0 && req.length == 0 => {
                esp_println::println!("msc: bulk-only reset");
                Some(OutResponse::Accepted)
            }
            _ => Some(OutResponse::Rejected),
        }
    }

    fn control_in<'a>(
        &'a mut self,
        req: control::Request,
        buf: &'a mut [u8],
    ) -> Option<InResponse<'a>> {
        if (req.request_type, req.recipient, req.index)
            != (
                RequestType::Class,
                Recipient::Interface,
                self.interface.0 as u16,
            )
        {
            return None;
        }

        match req.request {
            MSC_REQ_GET_MAX_LUN if req.value == 0 && req.length == 1 => {
                buf[0] = 0; // one LUN
                Some(InResponse::Accepted(&buf[..1]))
            }
            _ => Some(InResponse::Rejected),
        }
    }
}

#[derive(Clone, Copy)]
struct ReadStats {
    commands: u32,
    blocks: u64,
    min_lba: u32,
    max_lba: u32,
}

impl ReadStats {
    const fn new() -> Self {
        Self {
            commands: 0,
            blocks: 0,
            min_lba: u32::MAX,
            max_lba: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn record(&mut self, lba: u32, blocks: u32) -> bool {
        self.commands = self.commands.saturating_add(1);
        self.blocks = self.blocks.saturating_add(blocks as u64);
        self.min_lba = core::cmp::min(self.min_lba, lba);
        self.max_lba = core::cmp::max(
            self.max_lba,
            lba.saturating_add(blocks.saturating_sub(1)),
        );
        self.commands % 256 == 0
    }

    fn log(&self, prefix: &str) {
        if self.commands == 0 {
            esp_println::println!("msc: {} reads=0", prefix);
        } else {
            esp_println::println!(
                "msc: {} reads={} blocks={} bytes={} lba={}..{}",
                prefix,
                self.commands,
                self.blocks,
                self.blocks * SECTOR_SIZE as u64,
                self.min_lba,
                self.max_lba
            );
        }
    }
}

pub struct MscClass<'d, D: Driver<'d>> {
    read_ep: D::EndpointOut,
    write_ep: D::EndpointIn,
    stats: ReadStats,
}

impl<'d, D: Driver<'d>> MscClass<'d, D> {
    pub fn new(builder: &mut Builder<'d, D>, state: &'d mut State<'d>) -> Self {
        let mut function = builder.function(
            USB_CLASS_MASS_STORAGE,
            MSC_SUBCLASS_SCSI_TRANSPARENT,
            MSC_PROTOCOL_BULK_ONLY,
        );
        let mut interface = function.interface();
        let interface_number = interface.interface_number();
        let mut alt = interface.alt_setting(
            USB_CLASS_MASS_STORAGE,
            MSC_SUBCLASS_SCSI_TRANSPARENT,
            MSC_PROTOCOL_BULK_ONLY,
            None,
        );

        let read_ep = alt.endpoint_bulk_out(None, 64);
        let write_ep = alt.endpoint_bulk_in(None, 64);

        drop(function);

        let control = state.control.write(Control {
            interface: interface_number,
        });
        builder.handler(control);

        Self {
            read_ep,
            write_ep,
            stats: ReadStats::new(),
        }
    }

    pub async fn run<S: FileSource>(
        &mut self,
        disk: &mut VirtualFat16<S>,
    ) -> Result<(), EndpointError> {
        loop {
            self.read_ep.wait_enabled().await;
            self.stats.reset();
            disk.begin_session();
            esp_println::println!("msc: connected");

            match self.run_connected(disk).await {
                Ok(()) => {
                    self.stats.log("session end");
                    disk.end_session();
                }
                Err(EndpointError::Disabled) => {
                    self.stats.log("session end");
                    disk.end_session();
                    esp_println::println!("msc: disconnected");
                }
                Err(e) => {
                    self.stats.log("session error");
                    disk.end_session();
                    return Err(e);
                }
            }
        }
    }

    async fn run_connected<S: FileSource>(
        &mut self,
        disk: &mut VirtualFat16<S>,
    ) -> Result<(), EndpointError> {
        let mut packet = [0u8; 64];
        // (sense key, ASC) of the last failed command, reported once by REQUEST SENSE.
        let mut sense = (0u8, 0u8);

        loop {
            let n = self.read_ep.read(&mut packet).await?;
            if n != 31 {
                esp_println::println!("msc: ignoring non-CBW packet len={}", n);
                continue;
            }

            let Some(cbw) = Cbw::parse(&packet[..31]) else {
                esp_println::println!("msc: invalid CBW");
                continue;
            };

            let mut residue = cbw.transfer_len;
            let mut status = 0u8;

            match cbw.command[0] {
                SCSI_TEST_UNIT_READY => {}

                SCSI_INQUIRY => {
                    let data = inquiry();
                    let sent = self.send_limited(&data, cbw.transfer_len).await?;
                    residue = residue.saturating_sub(sent as u32);
                }

                SCSI_REQUEST_SENSE => {
                    let data = request_sense(sense.0, sense.1);
                    sense = (0, 0);
                    let sent = self.send_limited(&data, cbw.transfer_len).await?;
                    residue = residue.saturating_sub(sent as u32);
                }

                SCSI_MODE_SENSE_6 => {
                    // Write-protected direct-access removable block device.
                    let data = [3u8, 0, 0x80, 0];
                    let sent = self.send_limited(&data, cbw.transfer_len).await?;
                    residue = residue.saturating_sub(sent as u32);
                }

                SCSI_READ_CAPACITY_10 => {
                    let mut data = [0u8; 8];
                    data[..4].copy_from_slice(&(TOTAL_SECTORS - 1).to_be_bytes());
                    data[4..].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
                    let sent = self.send_limited(&data, cbw.transfer_len).await?;
                    residue = residue.saturating_sub(sent as u32);
                }

                SCSI_READ_FORMAT_CAPACITIES => {
                    let mut data = [0u8; 12];
                    data[3] = 8; // one capacity descriptor
                    data[4..8].copy_from_slice(&TOTAL_SECTORS.to_be_bytes());
                    data[8] = 0x02; // formatted media
                    let block_len = SECTOR_SIZE as u32;
                    data[9] = ((block_len >> 16) & 0xff) as u8;
                    data[10] = ((block_len >> 8) & 0xff) as u8;
                    data[11] = (block_len & 0xff) as u8;
                    let sent = self.send_limited(&data, cbw.transfer_len).await?;
                    residue = residue.saturating_sub(sent as u32);
                }

                SCSI_START_STOP_UNIT | SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL => {}

                SCSI_READ_10 => {
                    let lba = u32::from_be_bytes([
                        cbw.command[2],
                        cbw.command[3],
                        cbw.command[4],
                        cbw.command[5],
                    ]);
                    let blocks = u16::from_be_bytes([cbw.command[7], cbw.command[8]]) as u32;

                    if self.stats.record(lba, blocks) {
                        self.stats.log("progress");
                    }

                    if lba >= TOTAL_SECTORS || blocks > TOTAL_SECTORS - lba {
                        sense = (SENSE_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE);
                        status = 1;
                    } else {
                        let mut sector = [0u8; SECTOR_SIZE];
                        let mut sent_total = 0u32;

                        for block in 0..blocks {
                            let block_lba = lba + block;
                            let mut pending_logged = false;
                            let waiting_since = Instant::now();

                            loop {
                                match disk.read_sector(block_lba, &mut sector) {
                                    FileReadStatus::Ready => break,
                                    FileReadStatus::Pending => {
                                        if !pending_logged {
                                            esp_println::println!(
                                                "msc: waiting for stream lba={}",
                                                block_lba
                                            );
                                            pending_logged = true;
                                        }
                                        if waiting_since.elapsed() >= PENDING_TIMEOUT {
                                            // Never wait forever: a stalled stream must not wedge
                                            // the MSC, and a host-side reset must be able to
                                            // recover it (the write below fails if the bus was
                                            // reset meanwhile). The sector is already zero-filled.
                                            esp_println::println!(
                                                "msc: stream wait timed out lba={}",
                                                block_lba
                                            );
                                            break;
                                        }
                                        Timer::after(Duration::from_millis(5)).await;
                                    }
                                    FileReadStatus::Expired => {
                                        esp_println::println!(
                                            "msc: stream data expired lba={}",
                                            block_lba
                                        );
                                        // Keep BOT/SCSI state healthy for the POC. An expired
                                        // sector is filled with zeroes by the source; this should
                                        // never happen in the normal forward-only Metronic path.
                                        break;
                                    }
                                }
                            }

                            self.write_bytes(&sector).await?;
                            sent_total += SECTOR_SIZE as u32;
                        }

                        residue = residue.saturating_sub(sent_total);
                    }
                }

                other => {
                    esp_println::println!(
                        "msc: unsupported SCSI opcode=0x{:02x} xfer={}",
                        other,
                        cbw.transfer_len
                    );
                    sense = (SENSE_ILLEGAL_REQUEST, ASC_INVALID_OPCODE);
                    status = 1;
                }
            }

            self.send_csw(cbw.tag, residue, status).await?;
        }
    }

    async fn send_limited(
        &mut self,
        data: &[u8],
        transfer_len: u32,
    ) -> Result<usize, EndpointError> {
        let len = core::cmp::min(data.len(), transfer_len as usize);
        self.write_bytes(&data[..len]).await?;
        Ok(len)
    }

    async fn write_bytes(&mut self, mut data: &[u8]) -> Result<(), EndpointError> {
        while !data.is_empty() {
            let n = core::cmp::min(64, data.len());
            self.write_ep.write(&data[..n]).await?;
            data = &data[n..];
        }
        Ok(())
    }

    async fn send_csw(
        &mut self,
        tag: u32,
        residue: u32,
        status: u8,
    ) -> Result<(), EndpointError> {
        let mut csw = [0u8; 13];
        csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        csw[4..8].copy_from_slice(&tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = status;
        self.write_ep.write(&csw).await
    }
}

struct Cbw {
    tag: u32,
    transfer_len: u32,
    command: [u8; 16],
}

impl Cbw {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() != 31 {
            return None;
        }

        if u32::from_le_bytes(data[0..4].try_into().ok()?) != CBW_SIGNATURE {
            return None;
        }

        if data[13] > 15 || data[14] == 0 || data[14] > 16 {
            return None;
        }

        let mut command = [0u8; 16];
        command.copy_from_slice(&data[15..31]);

        Some(Self {
            tag: u32::from_le_bytes(data[4..8].try_into().ok()?),
            transfer_len: u32::from_le_bytes(data[8..12].try_into().ok()?),
            command,
        })
    }
}

fn inquiry() -> [u8; 36] {
    let mut data = [0u8; 36];
    data[0] = 0x00; // direct-access block device
    data[1] = 0x80; // removable
    data[2] = 0x04; // SPC-2-ish
    data[3] = 0x02;
    data[4] = 31;
    data[8..16].copy_from_slice(b"IOBEWI  ");
    data[16..32].copy_from_slice(b"USB RADIO POC   ");
    data[32..36].copy_from_slice(b"0001");
    data
}

fn request_sense(key: u8, asc: u8) -> [u8; 18] {
    let mut data = [0u8; 18];
    data[0] = 0x70;
    data[2] = key;
    data[7] = 10;
    data[12] = asc;
    data
}
