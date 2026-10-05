//! Product UCF1 configuration records in the golden absolute NVS sectors.
//! Physical ownership/serialization comes from IOBEWI; the persisted format,
//! slot choice and provisioning policy remain unchanged product responsibilities.

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use iobewi_config_space::{Budget, ConfigBackend, Snapshot};
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_partitions::{PartitionError, TABLE_BUFFER_SIZE};
use usb_radio_core::config_store::{
    self, FLASH_SECTOR_SIZE, MAX_RECORD_LEN, SLOT_A_OFFSET, SLOT_B_OFFSET, Slot,
};

#[derive(Debug)]
pub enum FlashConfigError {
    Flash,
    TooLarge,
    VerifyFailed,
    Partition(PartitionError),
    LayoutUnsupported,
}

impl core::fmt::Display for FlashConfigError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Partition(error) => {
                write!(formatter, "NVS partition discovery failed: {error:?}")
            }
            error => write!(formatter, "{error:?}"),
        }
    }
}

#[derive(Clone, Copy)]
pub struct FlashConfigBackend {
    flash: &'static SharedFlash,
}

impl FlashConfigBackend {
    pub async fn new(flash: &'static SharedFlash) -> Result<Self, FlashConfigError> {
        {
            let mut guard = flash.lock().await;
            let mut table_buffer = [0u8; TABLE_BUFFER_SIZE];
            let partition = iobewi_esp_partitions::find_by_label(
                guard.storage(),
                &mut table_buffer,
                "nvs",
                1,
                2,
            )
            .map_err(FlashConfigError::Partition)?;
            if !config_store::supports_layout(partition.offset, partition.size, guard.capacity()) {
                return Err(FlashConfigError::LayoutUnsupported);
            }
        }
        Ok(Self { flash })
    }

    fn offset(slot: Slot) -> u32 {
        match slot {
            Slot::A => SLOT_A_OFFSET,
            Slot::B => SLOT_B_OFFSET,
        }
    }

    /// Reads and validates one slot; returns (generation, record bytes) if valid.
    async fn read_slot(&self, slot: Slot, buf: &mut [u8; MAX_RECORD_LEN]) -> Option<u64> {
        let ok = self
            .flash
            .lock()
            .await
            .read(Self::offset(slot), buf)
            .is_ok();
        if !ok {
            return None;
        }
        config_store::decode(buf).map(|d| d.generation)
    }

    async fn latest_generations(&self) -> (Option<u64>, Option<u64>) {
        let mut buf = [0u8; MAX_RECORD_LEN];
        let a = self.read_slot(Slot::A, &mut buf).await;
        let b = self.read_slot(Slot::B, &mut buf).await;
        (a, b)
    }

    async fn write_record(
        &self,
        space: &str,
        data: &[u8],
        cleared: bool,
    ) -> Result<u64, FlashConfigError> {
        let (a, b) = self.latest_generations().await;
        let generation = a.max(b).unwrap_or(0) + 1;
        let target = config_store::write_target(a, b);
        let mut record = [0xFFu8; MAX_RECORD_LEN];
        let len = config_store::encode(&mut record, generation, space, data, cleared)
            .ok_or(FlashConfigError::TooLarge)?;
        let at = Self::offset(target);

        {
            let mut flash = self.flash.lock().await;
            flash
                .erase(at, at + FLASH_SECTOR_SIZE)
                .map_err(|_| FlashConfigError::Flash)?;
            flash
                .write(at, &record[..len])
                .map_err(|_| FlashConfigError::Flash)?;
        } // Release the shared owner before verification reacquires it.

        let mut check = [0u8; MAX_RECORD_LEN];
        match self.read_slot(target, &mut check).await {
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
        let (a, b) = self.latest_generations().await;
        let Some(slot) = config_store::newest(a, b) else {
            return Ok(None);
        };
        let mut buf = [0u8; MAX_RECORD_LEN];
        self.read_slot(slot, &mut buf).await;
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
        self.write_record(space, data, false).await
    }

    async fn clear(&self, space: &str) -> Result<u64, Self::Error> {
        self.write_record(space, &[], true).await
    }
}
