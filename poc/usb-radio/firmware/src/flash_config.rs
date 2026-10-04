//! `iobewi_config_space::ConfigBackend` over two flash sectors of the default NVS partition
//! (0x9000..0xF000). IOBEWI's own ESP backend needs esp-hal 1.1, so this POC carries a small
//! adapter on esp-storage; the record format and slot choice live in `usb_radio_core`.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use esp_storage::FlashStorage;
use iobewi_config_space::{Budget, ConfigBackend, Snapshot};
use usb_radio_core::config_store::{self, MAX_RECORD_LEN, Slot};

const SLOT_A: u32 = 0x9000;
const SLOT_B: u32 = 0xA000;
const SECTOR: u32 = 4096;

pub type SharedFlash = Mutex<CriticalSectionRawMutex, RefCell<FlashStorage<'static>>>;

#[derive(Debug)]
pub enum FlashConfigError {
    Flash,
    TooLarge,
    VerifyFailed,
}

#[derive(Clone, Copy)]
pub struct FlashConfigBackend {
    flash: &'static SharedFlash,
}

impl FlashConfigBackend {
    pub fn new(flash: &'static SharedFlash) -> Self {
        Self { flash }
    }

    fn offset(slot: Slot) -> u32 {
        match slot {
            Slot::A => SLOT_A,
            Slot::B => SLOT_B,
        }
    }

    /// Reads and validates one slot; returns (generation, record bytes) if valid.
    fn read_slot(&self, slot: Slot, buf: &mut [u8; MAX_RECORD_LEN]) -> Option<u64> {
        let ok = self.flash.lock(|f| f.borrow_mut().read(Self::offset(slot), buf).is_ok());
        if !ok {
            return None;
        }
        config_store::decode(buf).map(|d| d.generation)
    }

    fn latest_generations(&self) -> (Option<u64>, Option<u64>) {
        let mut buf = [0u8; MAX_RECORD_LEN];
        let a = self.read_slot(Slot::A, &mut buf);
        let b = self.read_slot(Slot::B, &mut buf);
        (a, b)
    }

    fn write_record(&self, space: &str, data: &[u8], cleared: bool) -> Result<u64, FlashConfigError> {
        let (a, b) = self.latest_generations();
        let generation = a.max(b).unwrap_or(0) + 1;
        let target = config_store::write_target(a, b);
        let mut record = [0xFFu8; MAX_RECORD_LEN];
        let len = config_store::encode(&mut record, generation, space, data, cleared)
            .ok_or(FlashConfigError::TooLarge)?;
        let at = Self::offset(target);

        self.flash.lock(|f| {
            let mut f = f.borrow_mut();
            f.erase(at, at + SECTOR).map_err(|_| FlashConfigError::Flash)?;
            f.write(at, &record[..len]).map_err(|_| FlashConfigError::Flash)
        })?;

        let mut check = [0u8; MAX_RECORD_LEN];
        match self.read_slot(target, &mut check) {
            Some(g) if g == generation => Ok(generation),
            _ => Err(FlashConfigError::VerifyFailed),
        }
    }
}

impl ConfigBackend for FlashConfigBackend {
    type Error = FlashConfigError;

    fn capacity_units(&self) -> usize {
        MAX_RECORD_LEN
    }

    fn reservation_units(&self, _space: &str, budget: Budget) -> Option<usize> {
        // One space only: payload + header/CRC overhead must fit the per-slot record limit.
        let units = budget.max_bytes() + config_store::HEADER_LEN + config_store::CRC_LEN + 16;
        (units <= MAX_RECORD_LEN).then_some(budget.max_bytes())
    }

    async fn load(&self, space: &str) -> Result<Option<Snapshot>, Self::Error> {
        let (a, b) = self.latest_generations();
        let Some(slot) = config_store::newest(a, b) else {
            return Ok(None);
        };
        let mut buf = [0u8; MAX_RECORD_LEN];
        self.read_slot(slot, &mut buf);
        let Some(record) = config_store::decode(&buf) else {
            return Ok(None);
        };
        if record.cleared || record.space != space.as_bytes() {
            return Ok(None);
        }
        Ok(Some(Snapshot {
            generation: record.generation,
            data: record.data.to_vec(),
        }))
    }

    async fn commit(&self, space: &str, data: &[u8]) -> Result<u64, Self::Error> {
        self.write_record(space, data, false)
    }

    async fn clear(&self, space: &str) -> Result<u64, Self::Error> {
        self.write_record(space, &[], true)
    }
}
