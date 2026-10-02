//! Workload storage (S16): does this device have it, and where does the OTM2
//! state stand? Selection state only -- nothing here loads, runs or supervises a
//! Workload, and no HTTP surface exists for it yet.
//!
//! The capability comes from the on-flash partition table (partitions
//! `wl_meta`, `workload_a`, `workload_b`, found by name). A device flashed with an
//! older table, or a layout without Workload space, is `Unsupported`: the Agent
//! never invents space for it.

use iobewi_esp_flash::SharedFlash;
use iobewi_esp_workload::{Capability, EspWorkloadStorage, probe};

/// Probes the partition table and logs the capability and the OTM2 recovery
/// state. `None` = Workload OTA unsupported on this device.
pub async fn init(flash: &'static SharedFlash) -> Option<EspWorkloadStorage> {
    match probe(flash).await {
        Capability::Supported(layout) => {
            let storage = EspWorkloadStorage::new(layout);
            log::info!(
                "workload: storage supported, meta@{:#x} slots {} B each (A@{:#x} B@{:#x})",
                layout.meta().offset,
                layout.max_artifact_size(),
                layout.slot(iobewi_esp_workload::model::Side::A).offset,
                layout.slot(iobewi_esp_workload::model::Side::B).offset,
            );
            match storage.recover(flash).await {
                Ok(state) => log::info!("workload: otm2 recovery: {state:?}"),
                Err(e) => log::warn!("workload: otm2 metadata unreadable: {e:?}"),
            }
            Some(storage)
        }
        Capability::Unsupported(why) => {
            log::info!("workload: storage unsupported ({why:?}): Workload OTA unavailable");
            None
        }
        Capability::TableUnreadable => {
            log::warn!("workload: partition table unreadable: Workload OTA unavailable");
            None
        }
    }
}

#[cfg(feature = "workload-selftest")]
pub mod selftest;
