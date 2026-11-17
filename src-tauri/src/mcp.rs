//! MCP server, so agents (Claude Code, Claude Desktop, ...) can drive the bench through the
//! running app.
//!
//! It lives inside the GUI rather than as a separate program because only one process can
//! own the serial port: agent and GUI share the same [`Link`], so every agent action goes
//! through the same range checks, shows up on screen as it happens, and is announced in
//! the footer (see [`Notify`]).
//!
//! Setup for agents: `src-tauri/MCP.md`.
//!
//! Transport: Streamable HTTP at `http://127.0.0.1:<port>/mcp` ([`DEFAULT_PORT`], override
//! or disable with [`PORT_ENV`]). Bound to loopback only; the `Host` header must be a
//! loopback name (DNS-rebinding guard) and any request carrying a browser `Origin` is
//! refused, so web pages can't reach it. Agent CLIs don't send `Origin`.
//!
//! Choosing which supply to connect to stays with the person at the bench: tools work on
//! the connected supply and report a pending choice, but can't answer it. Likewise the
//! "Agentic power configuration" switch in the app ([`AgentControl`]): while it is off,
//! agents can read status but every setpoint and output change is refused.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use psu_core::psu::{Limits, Psu, Status};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::link::{KnownDevice, Link, LinkState};

pub const DEFAULT_PORT: u16 = 47310;
/// Port override; `off` disables the server.
pub const PORT_ENV: &str = "STEADYSTATE_MCP_PORT";

/// Announces an agent action to the person at the bench (the app shows it in the footer).
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

/// The app's "Agentic power configuration" switch: whether agents may change setpoints and
/// the output. Starts off each launch, so opening the app never hands an agent the bench.
#[derive(Clone, Default)]
pub struct AgentControl(Arc<AtomicBool>);

impl AgentControl {
    pub fn enabled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

/// Whether and where the server listens, for the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum McpStatus {
    Disabled,
    Listening { url: String },
    Failed { error: String },
}

/// Reads [`PORT_ENV`]: `None` = disabled, `Some(Err)` = unusable value.
pub fn port_from_env() -> Option<Result<u16, String>> {
    match std::env::var(PORT_ENV) {
        Err(_) => Some(Ok(DEFAULT_PORT)),
        Ok(v) if v.trim().eq_ignore_ascii_case("off") => None,
        Ok(v) => Some(
            v.trim()
                .parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .ok_or(format!("{PORT_ENV}={v:?} is not a port number or \"off\"")),
        ),
    }
}

/// Binds the loopback listener. Done synchronously at startup so the app knows right away
/// whether the server is up (e.g. a second instance finds the port taken).
pub fn bind(port: u16) -> io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub fn url(addr: SocketAddr) -> String {
    format!("http://{addr}/mcp")
}

/// Serves MCP on `listener` until the process exits. Must run inside a Tokio runtime.
pub async fn serve(
    listener: std::net::TcpListener,
    link: Link,
    notify: Notify,
    control: AgentControl,
) -> io::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let service = StreamableHttpService::new(
        move || Ok(Bench::new(link.clone(), notify.clone(), control.clone())),
        LocalSessionManager::default().into(),
        // Default allowed hosts are the loopback names; additionally refuse every request
        // that carries a browser Origin.
        StreamableHttpServerConfig::default().enforce_origin_validation(),
    );
    axum::serve(listener, axum::Router::new().nest_service("/mcp", service)).await
}

const INSTRUCTIONS: &str = "\
Controls the bench power supply connected to the SteadyState app on this computer. \
A person is usually at the bench watching the app, and every action you take is shown to them.

Start with get_status: it reports whether a supply is connected, its limits (max_volts, \
max_amps) and live readings. Values outside the limits are rejected without touching the \
supply. Setpoints are the voltage target and the current limit; the supply regulates to \
whichever it hits first.

Changing setpoints or the output needs the user's permission: the \"Agentic power \
configuration\" switch in the app. While it is off (agent_control_enabled is false in \
get_status), those tools are refused; ask the user to switch it on rather than retrying. \
Even when it is on, only turn the output on when the user has asked for it, and set voltage and current limit \
before enabling it (apply sets both at once). Readings take a moment to settle after a change; \
call get_status again to see them.

If connection is \"choosing\", the user has to pick a supply in the app window; tell them. \
If it is \"disconnected\", rescan looks for supplies again.";

/// One MCP session's handler. Cheap to clone; all sessions share the app's [`Link`].
#[derive(Clone)]
pub struct Bench {
    link: Link,
    notify: Notify,
    control: AgentControl,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Report {
    /// "connected", "searching" (looking for a supply), "choosing" (waiting for the user to
    /// pick a supply in the app window) or "disconnected".
    pub connection: String,
    /// What the connection state means right now, and what to do about it.
    pub detail: String,
    /// The connected supply.
    pub device: Option<Device>,
    /// Largest setpoints the connected supply accepts.
    pub limits: Option<DeviceLimits>,
    /// Live readings, read from the supply for this call.
    pub readings: Option<Readings>,
    /// The app's "Agentic power configuration" switch. While false, set_voltage,
    /// set_current, apply and set_output are refused; only the user can switch it on.
    pub agent_control_enabled: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Device {
    pub model: String,
    pub port: String,
    pub driver: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeviceLimits {
    pub max_volts: f64,
    pub max_amps: f64,
    pub max_watts: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Readings {
    pub output_on: bool,
    pub measured_volts: f64,
    pub measured_amps: f64,
    pub measured_watts: f64,
    /// Voltage setpoint.
    pub set_volts: f64,
    /// Current limit.
    pub set_amps: f64,
    /// Device-side over-voltage trip point, if the supply reports one. Informational: not
    /// enforced on every model.
    pub ovp_volts: Option<f64>,
    /// Device-side over-current trip point, if the supply reports one. Informational.
    pub ocp_amps: Option<f64>,
}

impl From<Status> for Readings {
    fn from(s: Status) -> Self {
        Self {
            output_on: s.output_on,
            measured_volts: s.v_meas,
            measured_amps: s.i_meas,
            measured_watts: s.p_meas,
            set_volts: s.v_set,
            set_amps: s.i_set,
            ovp_volts: s.ovp,
            ocp_amps: s.ocp,
        }
    }
}

impl From<Limits> for DeviceLimits {
    fn from(l: Limits) -> Self {
        Self { max_volts: l.voltage, max_amps: l.current, max_watts: l.power }
    }
}

impl From<&KnownDevice> for Device {
    fn from(d: &KnownDevice) -> Self {
        Self { model: d.model.clone(), port: d.port.clone(), driver: d.driver.clone() }
    }
}

impl Report {
    /// Everything but the live readings.
    fn from_link(link: &LinkState) -> Self {
        let (connection, detail, device, limits) = match link {
            LinkState::Connected { device, limits } => (
                "connected",
                format!("{} on {}", device.model, device.port),
                Some(device.into()),
                Some((*limits).into()),
            ),
            LinkState::Searching { last } => (
                "searching",
                match last {
                    Some(d) => format!("connecting to {} on {}; check again shortly", d.model, d.port),
                    None => "looking for a supply; check again shortly".into(),
                },
                None,
                None,
            ),
            LinkState::Choosing { found, .. } => (
                "choosing",
                format!(
                    "found {}; the user must choose one in the SteadyState window before it \
                     can be controlled",
                    found
                        .iter()
                        .map(|d| format!("{} on {}", d.info.model, d.port))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None,
                None,
            ),
            LinkState::Disconnected { reason, .. } => (
                "disconnected",
                format!("{reason}; call rescan to look for supplies again"),
                None,
                None,
            ),
        };
        Self {
            connection: connection.into(),
            detail,
            device,
            limits,
            readings: None,
            agent_control_enabled: false,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VoltageArgs {
    /// Voltage setpoint in volts, from 0 up to max_volts (see get_status).
    pub volts: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CurrentArgs {
    /// Current limit in amps, from 0 up to max_amps (see get_status).
    pub amps: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutputArgs {
    /// true energizes the output terminals, false switches them off.
    pub on: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyArgs {
    /// Voltage setpoint in volts, from 0 up to max_volts.
    pub volts: f64,
    /// Current limit in amps, from 0 up to max_amps.
    pub amps: f64,
}

type ToolResult = Result<Json<Report>, String>;

#[tool_router]
impl Bench {
    pub fn new(link: Link, notify: Notify, control: AgentControl) -> Self {
        Self { link, notify, control, tool_router: Self::tool_router() }
    }

    /// Connection state plus, when connected, live readings straight from the supply.
    async fn report(&self) -> Report {
        let link = self.link.snapshot().link;
        let mut report = Report::from_link(&link);
        report.agent_control_enabled = self.control.enabled();
        if matches!(link, LinkState::Connected { .. }) {
            match self.link.with_psu(|psu| psu.status()).await {
                Ok(status) => report.readings = Some(status.into()),
                Err(e) => report.detail = format!("connected, but reading the supply failed: {e}"),
            }
        }
        report
    }

    /// Runs a device command, tells the person at the bench, and reports the result.
    async fn act<F>(&self, what: String, f: F) -> ToolResult
    where
        F: FnOnce(&mut dyn Psu) -> psu_core::psu::Result<()> + Send + 'static,
    {
        if !self.control.enabled() {
            (self.notify)(format!("Agent blocked: {what}"));
            return Err(format!(
                "{what} refused: \"Agentic power configuration\" is switched off in the \
                 SteadyState app. Ask the user to switch it on; don't retry until they have."
            ));
        }
        match self.link.with_psu(f).await {
            Ok(()) => {
                (self.notify)(format!("Agent: {what}"));
                Ok(Json(self.report().await))
            }
            Err(e) => {
                (self.notify)(format!("Agent: {what} refused: {e}"));
                Err(format!("{what} failed: {e}"))
            }
        }
    }

    #[tool(
        description = "Report the connection state, the connected supply and its limits, and live \
                       readings (output on/off, measured V/A/W, setpoints). Call this first.",
        annotations(title = "Get supply status", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_status(&self) -> Json<Report> {
        Json(self.report().await)
    }

    #[tool(
        description = "Set the voltage setpoint in volts. Takes effect on the output immediately \
                       if it is on. Returns the updated status.",
        annotations(
            title = "Set voltage",
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_voltage(&self, Parameters(VoltageArgs { volts }): Parameters<VoltageArgs>) -> ToolResult {
        self.act(format!("set voltage to {volts} V"), move |psu| psu.set_voltage(volts)).await
    }

    #[tool(
        description = "Set the current limit in amps. Returns the updated status.",
        annotations(
            title = "Set current limit",
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_current(&self, Parameters(CurrentArgs { amps }): Parameters<CurrentArgs>) -> ToolResult {
        self.act(format!("set current limit to {amps} A"), move |psu| psu.set_current(amps)).await
    }

    #[tool(
        description = "Set voltage and current limit together. Both are checked before either is \
                       sent, and the current limit is set first, so a rejected pair leaves the \
                       supply untouched. Prefer this over two separate calls. Returns the updated \
                       status.",
        annotations(
            title = "Set voltage and current limit",
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn apply(&self, Parameters(ApplyArgs { volts, amps }): Parameters<ApplyArgs>) -> ToolResult {
        self.act(format!("set {volts} V, {amps} A"), move |psu| psu.apply(volts, amps)).await
    }

    #[tool(
        description = "Switch the output terminals on or off. Only switch on when the user asked \
                       for it, after setting voltage and current limit. Returns the updated status.",
        annotations(
            title = "Switch output",
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_output(&self, Parameters(OutputArgs { on }): Parameters<OutputArgs>) -> ToolResult {
        let what = if on { "output on" } else { "output off" };
        self.act(what.to_string(), move |psu| psu.set_output(on)).await
    }

    #[tool(
        description = "Look for supplies again when disconnected (e.g. after plugging one in). \
                       Probing is read-only. If an unfamiliar supply turns up, the user is asked \
                       in the app to confirm it. Returns the status right after starting the scan.",
        annotations(title = "Rescan for supplies", read_only_hint = true, open_world_hint = false)
    )]
    async fn rescan(&self) -> ToolResult {
        if matches!(self.link.snapshot().link, LinkState::Connected { .. }) {
            return Err("already connected; rescan only applies while disconnected".into());
        }
        self.link.rescan();
        Ok(Json(self.report().await))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Bench {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("steadystate", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use psu_core::psu::{DeviceInfo, Error, check_range};
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use rmcp::service::RunningService;
    use rmcp::transport::StreamableHttpClientTransport;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const LIMITS: Limits = Limits { voltage: 30.0, current: 5.0, power: 150.0 };

    /// In-memory supply with the same range checks as the real drivers.
    struct FakePsu {
        info: DeviceInfo,
        status: Status,
    }

    impl FakePsu {
        fn boxed() -> Box<dyn Psu> {
            Box::new(Self {
                info: DeviceInfo { driver: "fake", model: "Fake 30 V / 5 A".into(), limits: LIMITS },
                status: Status {
                    output_on: false,
                    v_meas: 0.0,
                    i_meas: 0.0,
                    p_meas: 0.0,
                    v_set: 5.0,
                    i_set: 1.0,
                    ovp: None,
                    ocp: None,
                },
            })
        }
    }

    impl Psu for FakePsu {
        fn info(&self) -> &DeviceInfo {
            &self.info
        }
        fn status(&mut self) -> psu_core::psu::Result<Status> {
            let s = &mut self.status;
            s.v_meas = if s.output_on { s.v_set } else { 0.0 };
            s.i_meas = if s.output_on { 0.1 } else { 0.0 };
            s.p_meas = s.v_meas * s.i_meas;
            Ok(s.clone())
        }
        fn set_output(&mut self, on: bool) -> psu_core::psu::Result<()> {
            self.status.output_on = on;
            Ok(())
        }
        fn set_voltage(&mut self, volts: f64) -> psu_core::psu::Result<()> {
            check_range("voltage", volts, LIMITS.voltage)?;
            self.status.v_set = volts;
            Ok(())
        }
        fn set_current(&mut self, amps: f64) -> psu_core::psu::Result<()> {
            check_range("current", amps, LIMITS.current)?;
            self.status.i_set = amps;
            Ok(())
        }
        fn set_ovp(&mut self, _volts: f64) -> psu_core::psu::Result<()> {
            Err(Error::Unsupported("OVP"))
        }
    }

    fn connected() -> LinkState {
        LinkState::Connected {
            device: KnownDevice {
                port: "COM7".into(),
                driver: "fake".into(),
                model: "Fake 30 V / 5 A".into(),
            },
            limits: LIMITS,
        }
    }

    struct Harness {
        url: String,
        notes: Arc<Mutex<Vec<String>>>,
        control: AgentControl,
        client: RunningService<rmcp::RoleClient, ()>,
    }

    /// Serves `link` with agent control switched on.
    async fn start(link: Link) -> Harness {
        let h = start_locked(link).await;
        h.control.set(true);
        h
    }

    /// Serves `link` with agent control in its startup state (off).
    async fn start_locked(link: Link) -> Harness {
        let listener = bind(0).unwrap();
        let url = url(listener.local_addr().unwrap());
        let notes = Arc::new(Mutex::new(Vec::new()));
        let sink = notes.clone();
        let notify: Notify = Arc::new(move |n| sink.lock().unwrap().push(n));
        let control = AgentControl::default();
        tokio::spawn(serve(listener, link, notify, control.clone()));
        let client = ().serve(StreamableHttpClientTransport::from_uri(url.clone())).await.unwrap();
        Harness { url, notes, control, client }
    }

    async fn call(h: &Harness, tool: &'static str, args: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(tool);
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        h.client.call_tool(params).await.unwrap()
    }

    fn structured(r: &CallToolResult) -> &Value {
        assert_ne!(r.is_error, Some(true), "tool failed: {r:?}");
        r.structured_content.as_ref().expect("structured content")
    }

    fn error_text(r: &CallToolResult) -> String {
        assert_eq!(r.is_error, Some(true), "expected failure: {r:?}");
        serde_json::to_string(&r.content).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lists_tools_with_annotations() {
        let h = start(Link::detached(Some(FakePsu::boxed()), connected())).await;
        let info = h.client.peer_info().unwrap();
        assert_eq!(info.server_info.as_ref().unwrap().name, "steadystate");
        assert!(info.instructions.as_deref().unwrap().contains("get_status"));

        let tools = h.client.list_all_tools().await.unwrap();
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(names, ["apply", "get_status", "rescan", "set_current", "set_output", "set_voltage"]);
        let get_status = tools.iter().find(|t| t.name == "get_status").unwrap();
        assert_eq!(get_status.annotations.as_ref().unwrap().read_only_hint, Some(true));
        assert!(get_status.output_schema.is_some());
        let set_output = tools.iter().find(|t| t.name == "set_output").unwrap();
        assert_eq!(set_output.annotations.as_ref().unwrap().destructive_hint, Some(true));
        assert!(set_output.input_schema.get("properties").unwrap().get("on").is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_and_control_round_trip() {
        let h = start(Link::detached(Some(FakePsu::boxed()), connected())).await;

        let s = call(&h, "get_status", json!({})).await;
        let s = structured(&s);
        assert_eq!(s["connection"], "connected");
        assert_eq!(s["device"]["port"], "COM7");
        assert_eq!(s["limits"]["max_volts"], 30.0);
        assert_eq!(s["readings"]["set_volts"], 5.0);
        assert_eq!(s["readings"]["output_on"], false);

        let r = call(&h, "apply", json!({ "volts": 12.0, "amps": 0.5 })).await;
        assert_eq!(structured(&r)["readings"]["set_volts"], 12.0);
        assert_eq!(structured(&r)["readings"]["set_amps"], 0.5);

        let r = call(&h, "set_output", json!({ "on": true })).await;
        assert_eq!(structured(&r)["readings"]["output_on"], true);
        assert_eq!(structured(&r)["readings"]["measured_volts"], 12.0);

        let r = call(&h, "set_voltage", json!({ "volts": 3.3 })).await;
        assert_eq!(structured(&r)["readings"]["set_volts"], 3.3);
        let r = call(&h, "set_current", json!({ "amps": 2.0 })).await;
        assert_eq!(structured(&r)["readings"]["set_amps"], 2.0);

        assert_eq!(
            *h.notes.lock().unwrap(),
            [
                "Agent: set 12 V, 0.5 A",
                "Agent: output on",
                "Agent: set voltage to 3.3 V",
                "Agent: set current limit to 2 A"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn out_of_range_is_a_tool_error_and_changes_nothing() {
        let h = start(Link::detached(Some(FakePsu::boxed()), connected())).await;
        let r = call(&h, "apply", json!({ "volts": 40.0, "amps": 1.0 })).await;
        assert!(error_text(&r).contains("outside 0..=30"));
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["readings"]["set_volts"], 5.0);
        assert!(h.notes.lock().unwrap()[0].starts_with("Agent: set 40 V, 1 A refused"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disconnected_and_choosing_explain_themselves() {
        let link = LinkState::Disconnected { last: None, reason: "no supply found".into() };
        let h = start(Link::detached(None, link)).await;
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["connection"], "disconnected");
        assert!(structured(&s)["detail"].as_str().unwrap().contains("rescan"));
        assert_eq!(structured(&s)["readings"], Value::Null);
        let r = call(&h, "set_output", json!({ "on": true })).await;
        assert!(error_text(&r).contains("no supply connected"));
        assert_eq!(structured(&call(&h, "rescan", json!({})).await)["connection"], "disconnected");

        let found = vec![psu_core::drivers::Detected {
            port: "COM9".into(),
            info: DeviceInfo { driver: "fake", model: "Fake 30 V / 5 A".into(), limits: LIMITS },
        }];
        let link = LinkState::Choosing { found, suggested: 0, last: None };
        let h = start(Link::detached(None, link)).await;
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["connection"], "choosing");
        let detail = structured(&s)["detail"].as_str().unwrap().to_string();
        assert!(detail.contains("Fake 30 V / 5 A on COM9") && detail.contains("SteadyState window"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switch_off_blocks_changes_but_not_reading() {
        let h = start_locked(Link::detached(Some(FakePsu::boxed()), connected())).await;
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["agent_control_enabled"], false);
        assert_eq!(structured(&s)["readings"]["set_volts"], 5.0);

        for (tool, args) in [
            ("set_voltage", json!({ "volts": 12.0 })),
            ("set_current", json!({ "amps": 1.0 })),
            ("apply", json!({ "volts": 12.0, "amps": 1.0 })),
            ("set_output", json!({ "on": true })),
        ] {
            let err = error_text(&call(&h, tool, args).await);
            assert!(err.contains("Agentic power configuration"), "{tool}: {err}");
        }
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["readings"]["set_volts"], 5.0);
        assert_eq!(structured(&s)["readings"]["output_on"], false);
        assert_eq!(h.notes.lock().unwrap().len(), 4);
        assert_eq!(h.notes.lock().unwrap()[3], "Agent blocked: output on");

        // Switching it on in the app takes effect for the open session.
        h.control.set(true);
        let r = call(&h, "set_voltage", json!({ "volts": 12.0 })).await;
        assert_eq!(structured(&r)["readings"]["set_volts"], 12.0);
        assert_eq!(structured(&r)["agent_control_enabled"], true);
        h.control.set(false);
        assert!(error_text(&call(&h, "set_output", json!({ "on": true })).await).contains("switched off"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rescan_refused_while_connected() {
        let h = start(Link::detached(Some(FakePsu::boxed()), connected())).await;
        assert!(error_text(&call(&h, "rescan", json!({})).await).contains("already connected"));
    }

    /// Raw HTTP status for a POST to the server with the given extra headers.
    async fn post_status(url: &str, host: &str, extra: &str) -> String {
        let addr = url.trim_start_matches("http://").trim_end_matches("/mcp");
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        resp.lines().next().unwrap_or_default().to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_browsers_and_foreign_hosts() {
        let h = start(Link::detached(None, connected())).await;
        let port = h.url.split(':').nth(2).unwrap().trim_end_matches("/mcp").to_string();
        let local = format!("127.0.0.1:{port}");
        let evil = post_status(&h.url, &local, "Origin: https://evil.example\r\n").await;
        assert!(evil.contains(" 403"), "{evil}");
        let rebound = post_status(&h.url, &format!("evil.example:{port}"), "").await;
        assert!(rebound.contains(" 403"), "{rebound}");
        // The same request without those is let through (any non-403 answer).
        let ok = post_status(&h.url, &local, "").await;
        assert!(!ok.contains(" 403"), "{ok}");
    }

    #[test]
    fn port_env_parsing() {
        // SAFETY: only this test touches the variable.
        unsafe { std::env::remove_var(PORT_ENV) };
        assert_eq!(port_from_env(), Some(Ok(DEFAULT_PORT)));
        unsafe { std::env::set_var(PORT_ENV, "off") };
        assert_eq!(port_from_env(), None);
        unsafe { std::env::set_var(PORT_ENV, "5000") };
        assert_eq!(port_from_env(), Some(Ok(5000)));
        unsafe { std::env::set_var(PORT_ENV, "0") };
        assert!(matches!(port_from_env(), Some(Err(_))));
        unsafe { std::env::remove_var(PORT_ENV) };
    }
}
