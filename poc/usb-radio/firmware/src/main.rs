#![no_std]
#![no_main]

extern crate alloc;

mod flash_config;
mod msc;
mod stream;
mod wifi;

use embassy_executor::Spawner;

use embassy_futures::join::join;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use embassy_usb::Builder;
#[cfg(feature = "usb-debug")]
use embassy_usb::Handler;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart, UartRx, UartTx},
    usb::otg::{
        Usb,
        embassy_usb_device::{Config as OtgConfig, Driver},
    },
};
use flash_config::FlashConfigBackend;
use iobewi_config_space::ConfigManager;
use iobewi_wifi_manager::{CONFIG_BUDGET, WifiManager};
use msc::{MscClass, State as MscState};
use stream::{STREAM, SharedStreamSource};
use usb_radio_core::VirtualFat16;

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

/// Logs USB bus lifecycle events to show how far a host's enumeration gets (`usb-debug`).
#[cfg(feature = "usb-debug")]
struct BusLog;

#[cfg(feature = "usb-debug")]
impl Handler for BusLog {
    fn enabled(&mut self, enabled: bool) {
        esp_println::println!("usb: enabled={}", enabled);
    }
    fn reset(&mut self) {
        esp_println::println!("usb: bus reset");
    }
    fn addressed(&mut self, addr: u8) {
        esp_println::println!("usb: addressed={}", addr);
    }
    fn configured(&mut self, configured: bool) {
        esp_println::println!("usb: configured={}", configured);
    }
    fn suspended(&mut self, suspended: bool) {
        esp_println::println!("usb: suspended={}", suspended);
    }
}

#[esp_hal::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    esp_println::println!("usb-radio POC: P3 continuous HTTP MP3 -> USB MSC");
    esp_println::println!("usb-radio POC: DP=GPIO20 DM=GPIO19");
    esp_println::println!("stream: {}", stream::STREAM_URL);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Wi-Fi's vendor driver needs a C-compatible heap. Keep the MP3 ring itself static
    // so its memory use is deterministic and independent from this allocator.
    esp_alloc::heap_allocator!(size: 96 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Wi-Fi credentials come from Improv Serial (ESP Web Tools) and live in flash.
    let flash = iobewi_esp_flash::init(peripherals.FLASH);
    let backend = FlashConfigBackend::new(flash)
        .await
        .unwrap_or_else(|error| panic!("golden UCF1 NVS layout: {error}"));
    let mut config_manager = ConfigManager::new(backend);
    let wifi_space = config_manager
        .claim("wifi", CONFIG_BUDGET)
        .expect("wifi config space");

    let transport = wifi::EspWifiTransport::new(iobewi_esp_wifi::WifiManager::new(
        peripherals.WIFI,
        spawner,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
    ));

    // Improv Serial shares UART0 (the USB-UART port) with the log output.
    let uart = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_rx(peripherals.GPIO44)
        .with_tx(peripherals.GPIO43)
        .into_async();
    let (uart_rx, uart_tx) = uart.split();
    let manager = WifiManager::new(transport, wifi_space);
    spawner.spawn(improv_task(uart_rx).unwrap());
    // Stack handles stay on this executor; they are not Send across cores.
    let network_ready = mk_static!(wifi::NetworkReady, wifi::NetworkReady::new());
    spawner.spawn(wifi_task(manager, uart_tx, network_ready).unwrap());

    let usb = Usb::new_fs(peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);

    let mut ep_out_buffer = [0u8; 1024];
    let driver = Driver::new(usb, &mut ep_out_buffer, OtgConfig::default());

    let mut usb_config = embassy_usb::Config::new(0x303A, 0x4001);
    usb_config.manufacturer = Some("IOBEWI");
    usb_config.product = Some("USB Radio POC");
    usb_config.serial_number = Some("RADIO-POC-0002");
    // Bus-powered by the player: Wi-Fi bursts draw far more than the former 100 mA declared.
    // 500 mA is the USB 2.0 maximum for a bus-powered device.
    usb_config.max_power = 500;
    usb_config.composite_with_iads = false;
    usb_config.device_class = 0x00;
    usb_config.device_sub_class = 0x00;
    usb_config.device_protocol = 0x00;

    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut control_buf = [0u8; 64];

    let mut msc_state = MscState::new();
    #[cfg(feature = "usb-debug")]
    let mut bus_log = BusLog;

    let mut builder = Builder::new(
        driver,
        usb_config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );

    #[cfg(feature = "usb-debug")]
    builder.handler(&mut bus_log);

    let mut msc = MscClass::new(&mut builder, &mut msc_state);
    let mut usb_device = builder.build();
    let mut disk = VirtualFat16::new(SharedStreamSource::new(&STREAM));

    let stream_fut = async {
        let stack = network_ready.wait().await;
        stream::run(stack, &STREAM).await
    };

    let usb_fut = async {
        // The USB disk is only presented once the stream has delivered the prebuffer: a visible
        // RADIO.MP3 with no audio behind it (Wi-Fi not provisioned/connected, stream down)
        // would look like an empty file to the host. Without data there is no USB device.
        while !STREAM.is_ready() {
            let (written, consumed) = STREAM.progress();
            esp_println::println!(
                "stream: prebuffer written={} consumed={}",
                written,
                consumed
            );
            Timer::after(Duration::from_millis(500)).await;
        }

        let (written, _) = STREAM.progress();
        esp_println::println!("stream: prebuffer bytes={}", written);
        esp_println::println!("usb: enabling MSC for Metronic");

        let device_fut = usb_device.run();
        let msc_fut = async {
            if let Err(err) = msc.run(&mut disk).await {
                esp_println::println!("msc: fatal endpoint error: {:?}", err);
            }
        };

        join(device_fut, msc_fut).await;
    };

    join(stream_fut, usb_fut).await;
}

#[embassy_executor::task]
async fn improv_task(rx: UartRx<'static, esp_hal::Async>) {
    wifi::improv_reader(rx).await
}

#[embassy_executor::task]
async fn wifi_task(
    manager: wifi::Manager,
    tx: UartTx<'static, esp_hal::Async>,
    network_ready: &'static wifi::NetworkReady,
) {
    wifi::wifi_task(manager, tx, network_ready).await
}
