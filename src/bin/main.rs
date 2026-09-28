#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::timer::timg::TimerGroup;
use static_cell::StaticCell;

use embewi_agent_esp::agent;
use embewi_agent_esp::app_config;
use embewi_agent_esp::hardware;
use embewi_agent_esp::status;
use embewi_agent_esp::tls;
use iobewi_config_space::ConfigManager;
use iobewi_esp_config_space::{NvsConfigBackend, NvsPartition};
use embewi_agent_esp::wifi::{self, WifiManager};
use embewi_agent_esp::runtime_config;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // First thing, before anything else touches the stack any deeper than
    // this: paints it for `stack_usage::free_bytes()`'s high-water-mark
    // measurement (contrat §5's `task_hwm_min`) -- the earlier this runs,
    // the more of the stack it captures as "unused" before real usage
    // grows past it. Doesn't capture the runtime's own pre-`main` prologue
    // (riscv-rt's own stack usage before jumping here), but that's a fixed,
    // small, one-time cost, not something that grows with this firmware's
    // own code.
    embewi_agent_esp::stack_usage::paint();

    // The portable log service replaces `esp_println::logger::init_logger_from_env()`:
    // it still prints locally at the same filter level (`.cargo/config.toml`'s
    // ESP_LOG, configured by the shared service), but also captures lines for the outbound
    // WebSocket log stream (contrat §5). `init_logger` (not `_from_env`, the
    // reason the old call used the latter) applies one flat level to every
    // crate, ignoring ESP_LOG's per-module syntax entirely -- that meant
    // smoltcp, embassy-net and esp-radio were all logging at "info" on the
    // same USB wire Improv uses, real bytes possibly queued behind that
    // chatter, a suspected contributor to an earlier Improv bug.
    iobewi_log_stream::install(iobewi_esp_log_stream::console_print, "embewi_agent_esp");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Sizes recommended by esp-radio's docs for Wi-Fi. This second pool is
    // carved out of the same DRAM region the linker otherwise reserves
    // entirely for the stack (see `stack_usage.rs`) -- grown from 36 KiB
    // once `task_hwm_min` (contrat §5's real stack high-water-mark,
    // exposed in the heartbeat) confirmed real usage was nowhere close to
    // the ~189 KiB the linker set aside by default. Deliberately not
    // claiming all of the headroom `task_hwm_min` showed free: that
    // number only reflects code paths actually exercised so far (a real
    // OTA cycle, concurrent admin+heartbeat+logs TLS, haven't all been
    // observed together yet), so this keeps a wide safety margin rather
    // than assuming the untested paths won't dig deeper.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 132 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // `esp_hal::init` above unconditionally disabled every watchdog on the
    // chip (no `Config` option to keep one running), and `TimerGroup::new`
    // just now reset the whole TIMG0 block anyway (its first use resets the
    // peripheral -- arming the watchdog any earlier than this would just
    // have that reset wipe it straight back out). From here on, re-armed:
    // a freeze anywhere through `ota::on_boot`'s decision still resets the
    // device instead of bricking it on a `pending_verify` image; see
    // `ota.rs`'s "anti-freeze watchdog" section for the rest of it.
    embewi_agent_esp::ota::arm_boot_watchdog();

    // The physical flash has one process-wide owner. ConfigSpace/NVS and
    // IOBEWI OTA share only this serialized hardware capability.
    let flash = iobewi_esp_flash::init(peripherals.FLASH);

    // Components claim isolated persistent configuration capabilities at
    // boot. The manager knows capacities/ownership only; each component owns
    // the schema inside its opaque space. Claims are completed in a fixed
    // order before any application service is spawned.
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

    let app_config = config_manager
        .claim("app", app_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for app config");
    static APP_CONFIG: StaticCell<app_config::AppConfigSpace> = StaticCell::new();
    let app_config = &*APP_CONFIG.init(app_config);

    let agent_config = config_manager
        .claim("agent", agent::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for agent config");
    static AGENT_CONFIG: StaticCell<agent::AgentConfigSpace> = StaticCell::new();
    let agent_config = &*AGENT_CONFIG.init(agent_config);

    let lifecycle_config = config_manager
        .claim("lifecycle", embewi_agent_esp::lifecycle::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for lifecycle state");
    static LIFECYCLE_CONFIG: StaticCell<embewi_agent_esp::lifecycle::LifecycleConfigSpace> =
        StaticCell::new();
    let lifecycle_config = &*LIFECYCLE_CONFIG.init(lifecycle_config);

    let ota_config = config_manager
        .claim("ota", embewi_agent_esp::ota::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for OTA metadata");
    static OTA_CONFIG: StaticCell<embewi_agent_esp::ota::OtaConfigSpace> = StaticCell::new();
    let ota_config = &*OTA_CONFIG.init(ota_config);

    let runtime_space = config_manager
        .claim("runtime", runtime_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for runtime config");
    static RUNTIME_CONFIG: StaticCell<runtime_config::RuntimeConfig> = StaticCell::new();
    let runtime_config = &*RUNTIME_CONFIG.init(
        runtime_config::RuntimeConfig::new(runtime_space)
            .await
            .expect("runtime config unavailable"),
    );

    let tls_config = config_manager
        .claim("tls", tls::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for TLS config");
    static TLS_CONFIG: StaticCell<tls::TlsConfigSpace> = StaticCell::new();
    let tls_config = &*TLS_CONFIG.init(tls_config);

    let wifi_config = config_manager
        .claim("wifi", wifi::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for Wi-Fi config");

    // Which GPIO (if any) drives the status LED is board-specific and now
    // belongs to the hardware ConfigSpace rather than application-owned NVS.
    if let Some(gpio) = hardware::led_gpio(hardware_config).await {
        let led_pin = match gpio {
            0 => peripherals.GPIO0.degrade(),
            1 => peripherals.GPIO1.degrade(),
            2 => peripherals.GPIO2.degrade(),
            3 => peripherals.GPIO3.degrade(),
            4 => peripherals.GPIO4.degrade(),
            5 => peripherals.GPIO5.degrade(),
            6 => peripherals.GPIO6.degrade(),
            7 => peripherals.GPIO7.degrade(),
            8 => peripherals.GPIO8.degrade(),
            9 => peripherals.GPIO9.degrade(),
            10 => peripherals.GPIO10.degrade(),
            11 => peripherals.GPIO11.degrade(),
            12 => peripherals.GPIO12.degrade(),
            13 => peripherals.GPIO13.degrade(),
            14 => peripherals.GPIO14.degrade(),
            15 => peripherals.GPIO15.degrade(),
            16 => peripherals.GPIO16.degrade(),
            17 => peripherals.GPIO17.degrade(),
            18 => peripherals.GPIO18.degrade(),
            19 => peripherals.GPIO19.degrade(),
            20 => peripherals.GPIO20.degrade(),
            21 => peripherals.GPIO21.degrade(),
            other => panic!("saved status LED GPIO {other} is out of range for this chip"),
        };
        spawner.spawn(status::led_task(peripherals.RMT, led_pin).unwrap());
    }

    // IOBEWI OTA only reports the boot disposition here. Application policy
    // below decides whether this image is fit to be confirmed.
    let boot = embewi_agent_esp::ota::on_boot(
        flash,
        config_backend,
        ota_config,
    )
    .await;

    let lifecycle = embewi_agent_esp::lifecycle::state(lifecycle_config)
        .await
        .expect("invalid Embewi lifecycle");

    // Runtime prerequisites are local/durable properties. Network reachability
    // is deliberately not one of them: a temporarily unavailable AP must not
    // cause an otherwise-good firmware to roll back.
    let prerequisites_ok =
        wifi::is_provisioned(&wifi_config).await
        && tls::server_identity_valid(tls_config).await
        && hardware::is_configured(hardware_config).await
        && agent::is_provisioned(agent_config).await;

    if !prerequisites_ok {
        agent::set_state(agent::State::Failed);
        if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
            embewi_agent_esp::ota::reject_pending(flash).await;
        }
        panic!("embewi-agent prerequisites are missing or invalid");
    }

    match lifecycle {
        embewi_agent_esp::lifecycle::LifecycleState::ReadyForAgent => {
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::confirm_pending(
                    flash,
                    config_backend,
                    ota_config,
                )
                .await;
            }
            // If power failed after IOBEWI OTA confirmation but before this small
            // application bookkeeping write, the next boot reaches this same
            // Stable + ReadyForAgent path and completes it idempotently.
            embewi_agent_esp::lifecycle::production(lifecycle_config)
                .await
                .expect("couldn't enter Production lifecycle");
        }
        embewi_agent_esp::lifecycle::LifecycleState::Production => {
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::confirm_pending(
                    flash,
                    config_backend,
                    ota_config,
                )
                .await;
            }
        }
        embewi_agent_esp::lifecycle::LifecycleState::Factory
        | embewi_agent_esp::lifecycle::LifecycleState::Provisioning => {
            agent::set_state(agent::State::Failed);
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::reject_pending(flash).await;
            }
            panic!("embewi-agent must not bootstrap an unprovisioned device");
        }
    }

    // From here on the runtime is allowed to expose its administrative
    // surface. The HTTP module itself has no port-80 fallback.
    let tls = embewi_agent_esp::tls::init(embewi_agent_esp::time::now);

    let mut supervisor = embewi_agent_esp::supervisor::ApplicationSupervisor::new(
        spawner,
        peripherals.LPWR,
        tls,
        agent_config,
        app_config,
        tls_config,
        runtime_config,
        ota_config,
        flash,
        config_backend,
    );

    let transport = iobewi_esp_wifi::WifiManager::new(peripherals.WIFI, spawner, wifi::network_resources());
    let mut wifi = WifiManager::new(transport, wifi_config);
    if wifi.reconnect_saved().await {
        if let Some(stack) = wifi.ip_stack() {
            supervisor.on_ip_ready(stack);
        }
    }

    // Runtime has no Improv/bootstrap service. If the saved network is
    // unavailable it remains offline and retries only according to the normal
    // connector policy; it never opens a provisioning fallback.
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3600)).await;
    }
}
