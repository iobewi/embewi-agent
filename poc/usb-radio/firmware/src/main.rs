#![no_std]
#![no_main]

mod msc;

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_usb::Builder;
use esp_backtrace as _;
use esp_hal::{
    timer::timg::TimerGroup,
    usb::otg::{
        Usb,
        embassy_usb_device::{Config as OtgConfig, Driver},
    },
};
use msc::{MscClass, State as MscState};
use usb_radio_core::{DiagnosticSource, VirtualFat16};

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
async fn main(_spawner: Spawner) {
    esp_println::println!("usb-radio POC: P1 static virtual FAT16 MSC");
    esp_println::println!("usb-radio POC: DP=GPIO20 DM=GPIO19");

    let peripherals = esp_hal::init(esp_hal::Config::default());

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let usb = Usb::new_fs(
        peripherals.USB_FS,
        peripherals.GPIO20,
        peripherals.GPIO19,
    );

    let mut ep_out_buffer = [0u8; 1024];
    let driver = Driver::new(usb, &mut ep_out_buffer, OtgConfig::default());

    let mut usb_config = embassy_usb::Config::new(0x303A, 0x4001);
    usb_config.manufacturer = Some("IOBEWI");
    usb_config.product = Some("USB Radio POC");
    usb_config.serial_number = Some("RADIO-POC-0001");
    usb_config.max_power = 100;

    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut control_buf = [0u8; 64];

    let mut msc_state = MscState::new();

    let mut builder = Builder::new(
        driver,
        usb_config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );

    let mut msc = MscClass::new(&mut builder, &mut msc_state);
    let mut usb_device = builder.build();

    let mut disk = VirtualFat16::new(DiagnosticSource);

    let usb_fut = usb_device.run();
    let msc_fut = async {
        if let Err(err) = msc.run(&mut disk).await {
            esp_println::println!("msc: fatal endpoint error: {:?}", err);
        }
    };

    join(usb_fut, msc_fut).await;
}
