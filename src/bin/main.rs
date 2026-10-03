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
// Composition roots are allowed to know the ESP TLS platform directly --
// only the portable applicative library (embewi_agent_esp's own lib.rs)
// must not re-export it.
use embewi_agent_esp::esp_tls as tls;
use iobewi_config_space::{ConfigManager, ConfigSpace};
use iobewi_esp_config_space::{NvsConfigBackend, NvsPartition};
use iobewi_esp_indicator::{EspStatusIndicator, led_task};
use iobewi_indicator::{Status, StatusTracker};
use embewi_agent_esp::wifi::{self, WifiManager};
use embewi_agent_esp::runtime_config;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// ESP implementation of `ota::BootReset`, this binary's own composition
/// boundary: boot-time reset is only ever needed here, before any runtime
/// service (`supervisor.rs`) exists, so there's no reason for it to live
/// alongside the other platform capabilities constructed there.
#[derive(Clone, Copy)]
struct EspBootReset;

impl embewi_agent_esp::ota::BootReset for EspBootReset {
    async fn reset_for_rollback(&self) -> ! {
        iobewi_esp_ota::service::reset_for_rollback().await
    }

    fn reset_now(&self) -> ! {
        iobewi_esp_ota::service::reset_now()
    }
}

/// Agent lifecycle -> status indicator. The Wi-Fi manager owns the connection
/// policy (connect, bounded backoff, reconnect: `WifiManager::maintain`); this
/// binary only reacts to the events it reports. `main` is the only caller of
/// `set()` in the agent image, so there is no arbitration to do. Policy:
///
/// * `Booting`    -- initial state of every image.
/// * `Connecting` -- the agent starts joining its saved network, and again
///   on every link-down event. An AP that is absent (at boot or later) just
///   keeps this state while the manager retries; it is not a failure.
/// * `Online`     -- the manager reports association *and* DHCP up; the
///   runtime services are started on the first such event. Not gated on a
///   heartbeat: transient heartbeat/DNS/TLS errors do not change it.
/// * `Failed`     -- terminal only: the saved credentials are absent or
///   invalid, so retrying cannot help.
///   Priority: `Failed` (terminal) > `Connecting` > `Online`.
fn indicate<I: iobewi_indicator::StatusIndicator>(tracker: &StatusTracker<'_, I>, next: Status) {
    if let Some(previous) = tracker.set(next) {
        log::info!("indicator: {previous:?} -> {next:?}");
    }
}

/// Executor timer for the manager's backoff.
struct EmbassySleep;

impl iobewi_wifi_manager::Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        embassy_time::Timer::after(embassy_time::Duration::from_millis(ms as u64)).await;
    }
}

/// Maps manager link events to the runtime services and the status LED.
struct LinkEvents<'a> {
    supervisor: &'a mut embewi_agent_esp::supervisor::ApplicationSupervisor,
    status: &'a StatusTracker<'a, EspStatusIndicator>,
}

impl iobewi_wifi_manager::LinkObserver<embassy_net::Stack<'static>> for LinkEvents<'_> {
    fn link_down(&mut self) {
        indicate(self.status, Status::Connecting);
    }

    fn ready(&mut self, network: embassy_net::Stack<'static>) {
        // Idempotent: services are started once, the stack handle is stable.
        self.supervisor.on_ip_ready(network);
        indicate(self.status, Status::Online);
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // First thing, before anything else touches the stack any deeper than
    // this: paints it for the runtime diagnostics capability's high-water-mark
    // measurement (contrat §5's `task_hwm_min`) -- the earlier this runs,
    // the more of the stack it captures as "unused" before real usage
    // grows past it. Doesn't capture the runtime's own pre-`main` prologue
    // (riscv-rt's own stack usage before jumping here), but that's a fixed,
    // small, one-time cost, not something that grows with this firmware's
    // own code.
    let diagnostics = iobewi_esp_runtime::EspRuntimeDiagnostics::initialize();

    // The portable log service replaces `esp_println::logger::init_logger_from_env()`:
    // it still prints locally at the same filter level (`.cargo/config.toml`'s
    // ESP_LOG, configured by the shared service), but also captures lines for the outbound
    // WebSocket log stream (contrat §5). `init_logger` (not `_from_env`, the
    // reason the old call used the latter) applies one flat level to every
    // crate, ignoring ESP_LOG's per-module syntax entirely -- that meant
    // smoltcp, embassy-net and esp-radio were all logging at "info" on the
    // same USB wire Improv uses, real bytes possibly queued behind that
    // chatter, a suspected contributor to an earlier Improv bug.
    iobewi_log::install(iobewi_esp_console::console_print, "embewi_agent_esp");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Sizes recommended by esp-radio's docs for Wi-Fi. This second pool is
    // carved out of the same DRAM region the linker otherwise reserves
    // entirely for the stack (see `iobewi-esp-runtime`) -- grown from 36 KiB
    // once `task_hwm_min` (contrat §5's real stack high-water-mark,
    // exposed in the heartbeat) confirmed real usage was nowhere close to
    // the ~189 KiB the linker set aside by default. Deliberately not
    // claiming all of the headroom `task_hwm_min` showed free: that
    // number only reflects code paths actually exercised so far (a real
    // OTA cycle, concurrent admin+heartbeat+logs TLS, haven't all been
    // observed together yet), so this keeps a wide safety margin rather
    // than assuming the untested paths won't dig deeper.
    // With native Workloads, half of the reclaimed area is the Workload region (fixed address).
    #[cfg(feature = "workload-native")]
    let workload_region_ok = embewi_agent_esp::workload::native_runtime::init_reclaimed_ram();
    #[cfg(not(feature = "workload-native"))]
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
    // device instead of bricking it on a `pending_verify` image. Arming
    // itself is a platform mechanism; `BOOT_WATCHDOG_DEADLINE_MS` (how
    // long) is Embewi policy -- see `ota.rs`.
    iobewi_esp_ota::service::arm_watchdog_ms(embewi_agent_esp::ota::BOOT_WATCHDOG_DEADLINE_MS);

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

    // Constructed once, reused for every boot-time OTA decision below --
    // `on_boot`/`confirm_pending`/`reject_pending` only ever run here,
    // before any runtime service exists.
    let boot_runtime = iobewi_esp_ota::service::EspBootRuntime { flash, nvs: config_backend };
    let boot_reset = EspBootReset;

    // Workload OTA (S17): capability from the partition table (by name), OTM2 recovery,
    // and the HTTP service. Selection/staging only; nothing is loaded or executed and
    // activation is refused (no supervisor exists).
    let (workload, workload_control) = embewi_agent_esp::workload::init(
        flash,
        spawner,
        #[cfg(feature = "workload-native")]
        esp_hal::system::CpuControl::new(peripherals.CPU_CTRL),
        #[cfg(feature = "workload-native")]
        workload_region_ok,
    )
    .await;

    let hardware_config = config_manager
        .claim("hardware", hardware::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for hardware config");
    static HARDWARE_CONFIG: StaticCell<hardware::HardwareConfigSpace<NvsConfigBackend>> = StaticCell::new();
    let hardware_config = &*HARDWARE_CONFIG.init(hardware_config);

    let app_config = config_manager
        .claim("app", app_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for app config");
    static APP_CONFIG: StaticCell<ConfigSpace<NvsConfigBackend>> = StaticCell::new();
    let app_config = &*APP_CONFIG.init(app_config);

    let agent_config = config_manager
        .claim("agent", agent::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for agent config");
    static AGENT_CONFIG: StaticCell<agent::AgentConfigSpace<NvsConfigBackend>> = StaticCell::new();
    let agent_config = &*AGENT_CONFIG.init(agent_config);

    let lifecycle_config = config_manager
        .claim("lifecycle", embewi_agent_esp::ota::BOOTSTRAP_CONFIG_BUDGET)
        .expect("NVS capacity insufficient for lifecycle state");
    static LIFECYCLE_CONFIG: StaticCell<embewi_agent_esp::ota::BootstrapConfigSpace<NvsConfigBackend>> =
        StaticCell::new();
    let lifecycle_config = &*LIFECYCLE_CONFIG.init(lifecycle_config);

    let ota_config = config_manager
        .claim("ota", embewi_agent_esp::ota::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for OTA metadata");
    static OTA_CONFIG: StaticCell<embewi_agent_esp::ota::OtaConfigSpace<NvsConfigBackend>> = StaticCell::new();
    let ota_config = &*OTA_CONFIG.init(ota_config);

    let runtime_space = config_manager
        .claim("runtime", runtime_config::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for runtime config");
    static RUNTIME_CONFIG: StaticCell<runtime_config::RuntimeConfig<NvsConfigBackend>> = StaticCell::new();
    let runtime_config = &*RUNTIME_CONFIG.init(
        runtime_config::RuntimeConfig::new(runtime_space)
            .await
            .expect("runtime config unavailable"),
    );

    let tls_config = config_manager
        .claim("tls", tls::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for TLS config");
    static TLS_CONFIG: StaticCell<tls::TlsConfigSpace<NvsConfigBackend>> = StaticCell::new();
    let tls_config = &*TLS_CONFIG.init(tls_config);

    let wifi_config = config_manager
        .claim("wifi", wifi::CONFIG_BUDGET)
        .expect("NVS capacity insufficient for Wi-Fi config");

    // Which GPIO (if any) drives the status LED is board-specific and now
    // belongs to the hardware ConfigSpace rather than application-owned NVS.
    let led_gpio = hardware::led_gpio(hardware_config).await;
    match led_gpio {
        Some(gpio) => log::info!("hardware: status LED configured on GPIO{gpio}"),
        None => log::info!("hardware: no status LED configured"),
    }
    if let Some(gpio) = led_gpio {
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
            38 => peripherals.GPIO38.degrade(),
            39 => peripherals.GPIO39.degrade(),
            40 => peripherals.GPIO40.degrade(),
            41 => peripherals.GPIO41.degrade(),
            42 => peripherals.GPIO42.degrade(),
            43 => peripherals.GPIO43.degrade(),
            44 => peripherals.GPIO44.degrade(),
            45 => peripherals.GPIO45.degrade(),
            46 => peripherals.GPIO46.degrade(),
            47 => peripherals.GPIO47.degrade(),
            48 => peripherals.GPIO48.degrade(),
            other => panic!("saved status LED GPIO {other} is out of range for this chip"),
        };
        spawner.spawn(led_task(peripherals.RMT, led_pin).unwrap());
    }

    // IOBEWI OTA only reports the boot disposition here. Application policy
    // below decides whether this image is fit to be confirmed.
    let boot = embewi_agent_esp::ota::on_boot(
        &boot_runtime,
        &boot_reset,
        ota_config,
    )
    .await;

    let lifecycle = embewi_agent_esp::ota::bootstrap_state(lifecycle_config)
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
            embewi_agent_esp::ota::reject_pending(&boot_runtime, &boot_reset).await;
        }
        panic!("embewi-agent prerequisites are missing or invalid");
    }

    // Dual-OTA guard (S14/S18): a new Agent that cannot satisfy the Workload that is
    // active must not be confirmed; it is rejected and the previous Agent comes back.
    if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify
        && !embewi_agent_esp::workload::agent_can_run_active_workload(workload).await
    {
        log::error!(
            "workload: this Agent (runtime API {:?}) cannot run the active Workload: rejecting the Agent update",
            embewi_agent_esp::workload::RUNTIME_API
        );
        agent::set_state(agent::State::Failed);
        embewi_agent_esp::ota::reject_pending(&boot_runtime, &boot_reset).await;
    }

    match lifecycle {
        embewi_agent_esp::ota::BootstrapState::ReadyForAgent => {
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::confirm_pending(
                    &boot_runtime,
                    &boot_reset,
                    ota_config,
                )
                .await;
            }
            // If power failed after IOBEWI OTA confirmation but before this small
            // application bookkeeping write, the next boot reaches this same
            // Stable + ReadyForAgent path and completes it idempotently.
            embewi_agent_esp::ota::production(lifecycle_config)
                .await
                .expect("couldn't enter Production lifecycle");
        }
        embewi_agent_esp::ota::BootstrapState::Production => {
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::confirm_pending(
                    &boot_runtime,
                    &boot_reset,
                    ota_config,
                )
                .await;
            }
        }
        embewi_agent_esp::ota::BootstrapState::Factory
        | embewi_agent_esp::ota::BootstrapState::Provisioning => {
            agent::set_state(agent::State::Failed);
            if boot == embewi_agent_esp::ota::BootDisposition::PendingVerify {
                embewi_agent_esp::ota::reject_pending(&boot_runtime, &boot_reset).await;
            }
            panic!("embewi-agent must not bootstrap an unprovisioned device");
        }
    }

    // Boot reconciliation of the Workload (offline: no Wi-Fi, no Core needed). Only
    // reached once the Agent itself is confirmed/staying.
    #[cfg(any(feature = "test-probe-runtime", feature = "workload-native"))]
    spawner.spawn(embewi_agent_esp::workload::reconcile_task(workload_control).unwrap());
    #[cfg(feature = "workload-native")]
    spawner.spawn(embewi_agent_esp::workload::native_runtime::support_task(workload_control).unwrap());

    // From here on the runtime is allowed to expose its administrative
    // surface. The HTTP module itself has no port-80 fallback.
    let tls = tls::init(embewi_agent_esp::time::now);

    // S16 hardware gate only (feature `workload-selftest`, never in production images).
    #[cfg(feature = "workload-selftest")]
    if workload.storage().is_ok() {
        spawner.spawn(embewi_agent_esp::workload::selftest::run(workload, agent_config).unwrap());
    }

    let reboot = embewi_agent_esp::esp_reboot::EspReboot::new(peripherals.LPWR, spawner);
    let mut supervisor = embewi_agent_esp::supervisor::ApplicationSupervisor::new(
        spawner,
        reboot,
        tls,
        agent_config,
        app_config,
        tls_config,
        runtime_config,
        ota_config,
        flash,
        workload,
        workload_control,
        config_backend,
        diagnostics,
    );

    let status_led = EspStatusIndicator;
    let status = StatusTracker::new(&status_led);

    let transport = iobewi_esp_wifi::WifiManager::new(peripherals.WIFI, spawner, wifi::network_resources());
    let mut wifi = WifiManager::new(transport, wifi_config);
    indicate(&status, Status::Connecting);
    let mut events = LinkEvents { supervisor: &mut supervisor, status: &status };
    let ended = wifi.maintain(&EmbassySleep, &mut events).await;
    log::error!("wifi: connection maintenance ended: {ended:?}");
    indicate(&status, Status::Failed);

    // Runtime has no Improv/bootstrap service and never opens a provisioning
    // fallback. Reaching this point means the saved credentials are unusable.
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3600)).await;
    }
}
