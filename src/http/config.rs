//! One-shot HTTPS provisioning UI owned by embewi-init.
//!
//! This router is not linked into the normal runtime path. It writes the
//! durable application configuration, verifies the preloaded first agent,
//! moves the Embewi lifecycle to ReadyForAgent, asks IOBEWI OTA to activate the
//! staged image, and reboots. The transport beneath it is always TLS.

use alloc::format;
use alloc::string::String;
use core::fmt::Write as _;

use picoserve::extract::Form;
use picoserve::response::{File, Response, StatusCode};
use iobewi_http::routing::{get, get_service};
use iobewi_http::HttpRouter;
use iobewi_ota::http::RebootPort;

use crate::agent;
use crate::ota::BootstrapState;

use super::{STYLE_CSS, html_escape};

/// Narrow port for the factory-provisioning form's OTA needs: verify/stage
/// the preloaded first agent image and activate it. Deliberately opaque to
/// the physical storage and partition layout behind it -- this module only
/// ever needs to know whether each step succeeded.
#[allow(async_fn_in_trait)]
pub trait FactoryOta {
    async fn stage_preloaded(&self, image: crate::ota::PreloadedAgent) -> Result<(), ()>;
    async fn activate(&self, deployment_id: &str) -> Result<(), ()>;
}

const INDEX_TEMPLATE: &str = include_str!("index.html");
const CONFIRM_TEMPLATE: &str = include_str!("confirm.html");
const LOCKED_PAGE: &str = include_str!("locked.html");

/// Highest usable GPIO number on this chip. Update when this firmware
/// targets a chip other than ESP32-C3.
const MAX_GPIO: u8 = 21;
/// Sent by the form in place of a real pin number to mean "no status LED".
const LED_DISABLED: u8 = 255;

#[derive(serde::Deserialize)]
struct ConfigForm {
    led_gpio: u8,
    node_id: String,
    ctrl_url: String,
}

/// Wraps `text` in the same `<p class="message[ error]">` markup used
/// inline in `page()` -- factored out so the handler building its own
/// (success/error) message reuses the same class names.
fn message_html(text: &str, is_error: bool) -> String {
    let class = if is_error { "message error" } else { "message" };
    format!("<p class=\"{class}\">{text}</p>")
}

fn page(led_gpio: Option<u8>, node_id: &str, ctrl_url: &str, message: Option<&str>) -> String {
    let mut options = String::new();
    let _ = write!(
        options,
        "<option value=\"{LED_DISABLED}\"{}>D\u{e9}sactiv\u{e9}e</option>",
        if led_gpio.is_none() { " selected" } else { "" }
    );
    for gpio in 0..=MAX_GPIO {
        let _ = write!(
            options,
            "<option value=\"{gpio}\"{}>{gpio}</option>",
            if led_gpio == Some(gpio) { " selected" } else { "" }
        );
    }

    INDEX_TEMPLATE
        .replace("{{OPTIONS}}", &options)
        .replace("{{MESSAGE}}", message.unwrap_or_default())
        .replace("{{NODE_ID}}", &html_escape(node_id))
        .replace("{{CTRL_URL}}", &html_escape(ctrl_url))
}

/// Plain `async fn`, not `#[embassy_executor::task]`: called from inside
/// `http::run`'s own `if is_locked() {...} else {...}` (see that module's
/// doc comment) rather than spawned as an independent task, so its
/// `Future`'s storage shares space with [`super::api::serve`]'s instead of
/// both being reserved simultaneously and permanently.
pub async fn serve<L, R, FO, I, E, AB, HB, LB>(
    listener: &mut L,
    agent_config: &'static agent::AgentConfigSpace<AB>,
    hardware_config: &'static crate::hardware::HardwareConfigSpace<HB>,
    lifecycle_config: &'static crate::ota::BootstrapConfigSpace<LB>,
    factory_ota: FO,
    factory_agent: crate::ota::PreloadedAgent,
    reboot: R,
    identity: &'static I,
    entropy: &'static E,
) -> !
where
    L: iobewi_https::TlsListener,
    R: RebootPort + Clone + 'static,
    FO: FactoryOta + Clone + 'static,
    I: agent::DeviceIdentity,
    E: agent::TokenEntropy,
    AB: iobewi_config_space::ConfigBackend,
    HB: iobewi_config_space::ConfigBackend,
    LB: iobewi_config_space::ConfigBackend,
{
    let router = HttpRouter::new()
        .route("/style.css", get_service(File::css(STYLE_CSS)))
        .route(
            "/",
            get(move || async move {
                let lifecycle = crate::ota::bootstrap_state(lifecycle_config).await;
                let led_gpio = crate::hardware::led_gpio(hardware_config).await;
                if !matches!(
                    lifecycle,
                    Ok(BootstrapState::Provisioning | BootstrapState::ReadyForAgent)
                ) {
                    return Response::new(StatusCode::LOCKED, String::from(LOCKED_PAGE))
                        .with_content_type("text/html; charset=utf-8");
                }
                let node_id = agent::node_id(agent_config, identity).await;
                let ctrl_url = agent::ctrl_url(agent_config).await;
                Response::ok(page(led_gpio, &node_id, &ctrl_url, None))
                    .with_content_type("text/html; charset=utf-8")
            })
            // Single, one-shot save: on success this always locks and
            // reboots (the confirm() dialog in index.html warns about
            // that) -- a validation error re-serves the editable form
            // instead, so a typo doesn't lock the device out over nothing.
            .post(move |Form(form): Form<ConfigForm>| {
                // Cloned per call (this closure must stay `Fn`, invoked once
                // per request) -- `reboot` is a one-shot capability
                // internally, so cloning it here is safe even though this
                // handler only ever expects to actually trigger it once.
                let reboot = reboot.clone();
                let factory_ota = factory_ota.clone();
                async move {
                let lifecycle = crate::ota::bootstrap_state(lifecycle_config).await;
                if !matches!(
                    lifecycle,
                    Ok(BootstrapState::Provisioning | BootstrapState::ReadyForAgent)
                ) {
                    return Response::new(StatusCode::LOCKED, String::from(LOCKED_PAGE))
                        .with_content_type("text/html; charset=utf-8");
                }
                let gpio = (form.led_gpio != LED_DISABLED).then_some(form.led_gpio);
                if gpio.is_some_and(|gpio| gpio > MAX_GPIO) {
                    return Response::new(
                        StatusCode::BAD_REQUEST,
                        page(
                            None,
                            &form.node_id,
                            &form.ctrl_url,
                            Some(&message_html("Broche hors plage pour cette puce.", true)),
                        ),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }

                // Nothing below may report success (nor reboot, nor lock the
                // page for good) unless every write actually reached NVS.
                let saved = if crate::hardware::save_led_gpio(hardware_config, gpio).await.is_err() {
                    false
                } else {
                    agent::save_identity(agent_config, &form.node_id, &form.ctrl_url, "", entropy)
                        .await
                        .is_ok()
                };
                if !saved {
                    return Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        page(
                            gpio,
                            &form.node_id,
                            &form.ctrl_url,
                            Some(&message_html("\u{c9}chec de l'\u{e9}criture en m\u{e9}moire flash, r\u{e9}essayez.", true)),
                        ),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }
                let token = agent::token(agent_config).await;

                if factory_ota.stage_preloaded(factory_agent)
                    .await
                    .is_err()
                {
                    return Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        page(
                            gpio,
                            &form.node_id,
                            &form.ctrl_url,
                            Some(&message_html("L'image agent préchargée est absente ou invalide.", true)),
                        ),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }

                if crate::ota::ready_for_agent(lifecycle_config).await.is_err() {
                    return Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        page(
                            gpio,
                            &form.node_id,
                            &form.ctrl_url,
                            Some(&message_html("Échec du passage à ReadyForAgent.", true)),
                        ),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }

                if factory_ota.activate(factory_agent.deployment_id)
                    .await
                    .is_err()
                {
                    return Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        page(
                            gpio,
                            &form.node_id,
                            &form.ctrl_url,
                            Some(&message_html("Échec de l'activation du premier agent.", true)),
                        ),
                    )
                    .with_content_type("text/html; charset=utf-8");
                }

                // The platform's reboot capability is its own one-shot
                // guard -- calling it here is safe even under a second
                // concurrent hit. Its own delay gives this response time to
                // actually reach the client first.
                reboot.schedule_reboot().await;

                Response::ok(CONFIRM_TEMPLATE.replace("{{TOKEN}}", &html_escape(&token)))
                    .with_content_type("text/html; charset=utf-8")
                }
            }),
        );

    super::serve(listener, &router).await
}
