#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]
#![deny(clippy::large_stack_frames)]

use config_space_manager::ConfigManager;
use espbewi_config_space::{NvsConfigBackend, NvsPartition};
use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::UsbSerialJtag;
use static_cell::StaticCell;

use embewi_agent_esp::{agent, hardware, lifecycle, ota, provisioning, tls, wifi};
use embewi_agent_esp::wifi::WifiManager;

esp_bootloader_esp_idf::esp_app_desc!();

fn parse_u32_ascii(value: &str) -> Option<u32> {
    let mut out = 0u32;
    if value.is_empty() {
        return None;
    }
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        out = out.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(out)
}

fn factory_agent() -> ota::PreloadedAgent {
    let size = option_env!("EMBEWI_FACTORY_AGENT_SIZE")
        .and_then(parse_u32_ascii)
        .expect("EMBEWI_FACTORY_AGENT_SIZE missing: use scripts/build-boot.sh");
    let digest = option_env!("EMBEWI_FACTORY_AGENT_DIGEST")
        .expect("EMBEWI_FACTORY_AGENT_DIGEST missing: use scripts/build-boot.sh");
    let deployment_id = option_env!("EMBEWI_FACTORY_AGENT_DEPLOYMENT")
        .unwrap_or("factory-agent");
    ota::PreloadedAgent {
        size,
        digest,
        deployment_id,
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    embewi_agent_esp::log_stream::install();

    let peripherals =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 132 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let flash = espbewi_flash::init(peripherals.FLASH);

    static CONFIG_BACKEND: StaticCell<NvsConfigBackend> = StaticCell::new();
    let config_backend = &*CONFIG_BACKEND.init(
        NvsConfigBackend::new(flash, NvsPartition::new(0x9000, 0x6000))
            .await
            .expect("NVS config backend unavailable"),
    );
    let mut config_manager = ConfigManager::new(*config_backend);

    let hardware_config = config_manager
        .claim("hardware", hardware::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for hardware config");
    static HARDWARE_CONFIG: StaticCell<hardware::HardwareConfigSpace> = StaticCell::new();
    let hardware_config = &*HARDWARE_CONFIG.init(hardware_config);

    let agent_config = config_manager
        .claim("agent", agent::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for agent config");
    static AGENT_CONFIG: StaticCell<agent::AgentConfigSpace> = StaticCell::new();
    let agent_config = &*AGENT_CONFIG.init(agent_config);

    let lifecycle_config = config_manager
        .claim("lifecycle", lifecycle::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for lifecycle state");
    static LIFECYCLE_CONFIG: StaticCell<lifecycle::LifecycleConfigSpace> = StaticCell::new();
    let lifecycle_config = &*LIFECYCLE_CONFIG.init(lifecycle_config);

    let ota_config = config_manager
        .claim("ota", ota::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for OTA metadata");
    static OTA_CONFIG: StaticCell<ota::OtaConfigSpace> = StaticCell::new();
    let ota_config = &*OTA_CONFIG.init(ota_config);

    // TLS is claimed and established before Wi-Fi is even instantiated.
    // No provisioning network endpoint can exist before this succeeds.
    let tls_config = config_manager
        .claim("tls", tls::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for TLS config");
    static TLS_CONFIG: StaticCell<tls::TlsConfigSpace> = StaticCell::new();
    let tls_config = &*TLS_CONFIG.init(tls_config);

    let wifi_config = config_manager
        .claim("wifi", wifi::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for Wi-Fi config");

    match lifecycle::state(lifecycle_config)
        .await
        .expect("invalid Embewi lifecycle")
    {
        lifecycle::LifecycleState::Factory => {
            lifecycle::begin_provisioning(lifecycle_config)
                .await
                .expect("couldn't enter Provisioning lifecycle");
        }
        lifecycle::LifecycleState::Provisioning
        | lifecycle::LifecycleState::ReadyForAgent => {
            // A rollback from the first candidate legitimately returns here
            // with ReadyForAgent. The init image remains recoverable until the
            // agent reaches Production.
        }
        lifecycle::LifecycleState::Production => {
            panic!("embewi-init must never run once the device is in Production");
        }
    }

    // Before RF/Wi-Fi exists, plain Rng output is not guaranteed to be true
    // random on ESP32-C3. Keep the ADC-backed entropy source alive across
    // key/certificate generation so the global hardware RNG used by MbedTLS
    // is cryptographically seeded without bringing up any network capability.
    let trng_source =
        esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
    // The global MbedTLS instance must exist before any PSA call: PSA draws its
    // randomness from `mbedtls_psa_external_get_random`, which reads the RNG
    // slot that only `Tls::new` fills, and reports an entropy failure while no
    // `Tls` is active. Creating it draws no randomness itself, so the ADC-backed
    // source above still covers every byte the key generation consumes.
    let tls_handle = tls::init();
    let identity_name = agent::node_id(agent_config).await;
    tls::ensure_server_identity(tls_config, &identity_name)
        .await
        .expect("bootstrap TLS identity unavailable");
    core::mem::drop(trng_source);

    // Only after the durable identity has been re-read and validated do we
    // initialize the networking/provisioning machinery.
    let mut supervisor = embewi_agent_esp::supervisor::ProvisioningSupervisor::new(
        spawner,
        peripherals.LPWR,
        tls_handle,
        flash,
        agent_config,
        hardware_config,
        tls_config,
        lifecycle_config,
        ota_config,
        factory_agent(),
    );

    let mut wifi = WifiManager::new(peripherals.WIFI, spawner, wifi_config);
    if wifi.reconnect_saved().await {
        if let Some(stack) = wifi.ip_stack() {
            supervisor.on_ip_ready(stack);
        }
    }

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    provisioning::run(rx, tx, wifi, supervisor).await
}
