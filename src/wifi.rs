//! Application socket budget and the Wi-Fi capability assembled at boot.
//! The portable service owns credentials; the ESP adapter owns the radio.

use embassy_net::StackResources;
use static_cell::StaticCell;

pub use iobewi_wifi::{CONFIG_BUDGET, Network, WifiManager, WifiProvisioning, WifiTransport, is_provisioned};

// Socket count is application policy: DHCP, HTTPS, SNTP and outbound clients.
const SOCKETS: usize = 8;
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

pub fn network_resources() -> &'static mut StackResources<SOCKETS> {
    RESOURCES.init(StackResources::new())
}
