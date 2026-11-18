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
//! Transport: Streamable HTTP at `http://127.0.0.1:<port>/mcp`. The port is set in the app's
//! MCP Settings (default [`DEFAULT_PORT`]) and [`McpServer`] moves to a new one immediately.
//! The host is deliberately not configurable: bound to loopback only, the `Host` header must
//! be a loopback name (DNS-rebinding guard), and any request carrying a browser `Origin` is
//! refused, so neither other machines nor web pages can reach it. Agent CLIs don't send
//! `Origin`.
//!
//! Choosing which supply to connect to stays with the person at the bench: tools work on
//! the connected supply and report a pending choice, but can't answer it.
//!
//! The "Agentic power configuration" switch in the app ([`AgentControl`]) decides whether
//! agents may change setpoints. While it is off, the setpoint tools ([`SETPOINT_TOOLS`])
//! aren't offered at all; reading status and switching the output (on, off, power cycle)
//! always are, so agents can fold power control into their other work. Flipping the switch
//! sends connected agents `notifications/tools/list_changed`.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use psu_core::psu::{Limits, Psu, Status};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CacheScope, Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ResultType, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{NotificationContext, Peer, RequestContext};
use rmcp::schemars::{self, JsonSchema};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::link::{KnownDevice, Link, LinkState};

pub const DEFAULT_PORT: u16 = 47310;

/// Announces an agent action to the person at the bench (the app shows it in the footer).
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

/// Tools offered only while the "Agentic power configuration" switch is on.
pub const SETPOINT_TOOLS: [&str; 3] = ["set_voltage", "set_current", "apply"];

/// The app's "Agentic power configuration" switch: whether agents get the setpoint tools.
/// Starts off each launch, so opening the app never lets an agent change voltage or current.
#[derive(Clone, Default)]
pub struct AgentControl {
    enabled: Arc<AtomicBool>,
    /// Open MCP sessions, told when flipping the switch changes their tool list.
    peers: Arc<Mutex<Vec<Peer<RoleServer>>>>,
}

impl AgentControl {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Flips the switch and tells connected agents that their tool list changed.
    pub fn set(&self, on: bool) {
        if self.enabled.swap(on, Ordering::SeqCst) == on {
            return;
        }
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        peers.retain(|p| !p.is_transport_closed());
        for peer in peers.iter() {
            let peer = peer.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = peer.notify_tool_list_changed().await {
                    log::debug!("tools/list_changed not delivered: {e}");
                }
            });
        }
    }

    fn register(&self, peer: Peer<RoleServer>) {
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        peers.retain(|p| !p.is_transport_closed());
        peers.push(peer);
    }
}

/// Whether and where the server listens, for the frontend. `port` is the configured port,
/// also when listening on it failed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum McpStatus {
    Listening { url: String, port: u16 },
    Failed { error: String, port: u16 },
}

/// Binds the loopback listener. Done synchronously so the caller knows right away whether
/// the port is usable (e.g. a second instance finds it taken).
pub fn bind(port: u16) -> io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub fn url(addr: SocketAddr) -> String {
    format!("http://{addr}/mcp")
}

/// Serves MCP on `listener` until `stop` is cancelled, which also ends open sessions and
/// their event streams. Must run inside a Tokio runtime.
pub async fn serve(
    listener: std::net::TcpListener,
    link: Link,
    notify: Notify,
    control: AgentControl,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let service = StreamableHttpService::new(
        move || Ok(Bench::new(link.clone(), notify.clone(), control.clone())),
        LocalSessionManager::default().into(),
        // Default allowed hosts are the loopback names; additionally refuse every request
        // that carries a browser Origin.
        StreamableHttpServerConfig::default()
            .enforce_origin_validation()
            .with_cancellation_token(stop.child_token()),
    );
    axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
}

/// The running MCP server, movable to another port while the app runs.
pub struct McpServer {
    link: Link,
    notify: Notify,
    control: AgentControl,
    state: Mutex<Running>,
}

struct Running {
    status: McpStatus,
    /// Stops the current server; `None` while not listening.
    stop: Option<CancellationToken>,
}

impl McpServer {
    /// Starts serving on `port`. Never fails: a port that can't be used is reported through
    /// [`Self::status`], and the user can pick another.
    pub fn start(link: Link, notify: Notify, control: AgentControl, port: u16) -> Self {
        let error = "not started".to_string();
        let state = Mutex::new(Running { status: McpStatus::Failed { error, port }, stop: None });
        let server = Self { link, notify, control, state };
        if let Err(error) = server.set_port(port) {
            log::warn!("MCP server not started: {error}");
            server.lock().status = McpStatus::Failed { error, port };
        }
        server
    }

    pub fn status(&self) -> McpStatus {
        self.lock().status.clone()
    }

    /// Moves the server to `port`. The new port is bound before the old server stops, so if
    /// it can't be used the current server keeps running and the error is returned. Open
    /// sessions on the old port end; agents reconnect at the new address.
    pub fn set_port(&self, port: u16) -> Result<McpStatus, String> {
        let mut running = self.lock();
        if matches!(running.status, McpStatus::Listening { port: p, .. } if p == port) {
            return Ok(running.status.clone());
        }
        if port == 0 {
            return Err("port must be between 1 and 65535".into());
        }
        let listener =
            bind(port).map_err(|e| format!("can't listen on 127.0.0.1:{port}: {e}"))?;
        let url = url(listener.local_addr().map_err(|e| e.to_string())?);

        if let Some(old) = running.stop.take() {
            old.cancel();
        }
        let stop = CancellationToken::new();
        let (link, notify, control) = (self.link.clone(), self.notify.clone(), self.control.clone());
        let token = stop.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = serve(listener, link, notify, control, token).await {
                log::error!("MCP server on port {port} stopped: {e}");
            }
        });
        log::info!("MCP server listening at {url}");
        running.status = McpStatus::Listening { url, port };
        running.stop = Some(stop);
        Ok(running.status.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Running> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

const INSTRUCTIONS: &str = "\
Controls the bench power supply connected to the SteadyState app on this computer. \
A person is usually at the bench watching the app, and every action you take is shown to them.

Start with get_status: it reports whether a supply is connected, its limits (max_volts, \
max_amps) and live readings. Values outside the limits are rejected without touching the \
supply. Setpoints are the voltage target and the current limit; the supply regulates to \
whichever it hits first.

Switching the output (set_output, power_cycle) is always available, so you can power the \
device under test on, off or through a reset as part of a larger task. Only switch the output \
on when the user has asked for it.

Changing voltage or current (set_voltage, set_current, apply) needs the user's permission: \
the switch under \"MCP Settings\" in the app, which reads \"Output On/Off Controls Only\" \
when off and \"Full Agent Controls\" when on. While it is off (setpoint_tools_enabled is \
false in get_status) those tools aren't offered; if you need different setpoints, ask the \
user to set them or to switch to Full Agent Controls. The tool list updates \
when they do. Readings take a moment to settle after a change; call get_status again to \
see them.

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
    /// The switch under "MCP Settings" in the app: true = "Full Agent Controls", false =
    /// "Output On/Off Controls Only". While false, set_voltage, set_current and apply aren't
    /// offered; only the user can switch it. Switching the output is always available.
    pub setpoint_tools_enabled: bool,
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
            setpoint_tools_enabled: false,
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
pub struct PowerCycleArgs {
    /// Seconds the output stays off, from 0.1 to 60. Default 2.
    #[serde(default = "default_off_seconds")]
    pub off_seconds: f64,
}

fn default_off_seconds() -> f64 {
    2.0
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
        report.setpoint_tools_enabled = self.control.enabled();
        if matches!(link, LinkState::Connected { .. }) {
            match self.link.with_psu(|psu| psu.status()).await {
                Ok(status) => report.readings = Some(status.into()),
                Err(e) => report.detail = format!("connected, but reading the supply failed: {e}"),
            }
        }
        report
    }

    /// Whether `tool` is offered right now.
    fn offered(&self, tool: &str) -> bool {
        self.control.enabled() || !SETPOINT_TOOLS.contains(&tool)
    }

    /// [`Self::act`] for setpoint changes. Those tools aren't listed while the switch is off,
    /// but an agent may still call one from an older tool list, or race the switch.
    async fn act_setpoint<F>(&self, what: String, f: F) -> ToolResult
    where
        F: FnOnce(&mut dyn Psu) -> psu_core::psu::Result<()> + Send + 'static,
    {
        if !self.control.enabled() {
            (self.notify)(format!("Agent blocked: {what}"));
            return Err(format!(
                "{what} refused: changing voltage and current needs \"Full Agent Controls\", \
                 switched on under MCP Settings in the SteadyState app. Ask the user to switch \
                 it on or to set the values themselves. Output on/off is still available."
            ));
        }
        self.act(what, f).await
    }

    /// Runs a device command, tells the person at the bench, and reports the result.
    async fn act<F>(&self, what: String, f: F) -> ToolResult
    where
        F: FnOnce(&mut dyn Psu) -> psu_core::psu::Result<()> + Send + 'static,
    {
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
        self.act_setpoint(format!("set voltage to {volts} V"), move |psu| psu.set_voltage(volts))
            .await
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
        self.act_setpoint(format!("set current limit to {amps} A"), move |psu| {
            psu.set_current(amps)
        })
        .await
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
        self.act_setpoint(format!("set {volts} V, {amps} A"), move |psu| psu.apply(volts, amps))
            .await
    }

    #[tool(
        description = "Switch the output terminals on or off, e.g. to power the device under \
                       test for a step of a larger task. Uses the setpoints shown by get_status. \
                       Only switch on when the user asked for it. Returns the updated status.",
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
        description = "Switch the output off, wait, and switch it back on, e.g. to reset the \
                       device under test. Only works while the output is on, and leaves it on. \
                       Blocks for off_seconds. Returns the updated status.",
        annotations(title = "Power cycle output", destructive_hint = true, open_world_hint = false)
    )]
    async fn power_cycle(
        &self,
        Parameters(PowerCycleArgs { off_seconds }): Parameters<PowerCycleArgs>,
    ) -> ToolResult {
        if !(0.1..=60.0).contains(&off_seconds) {
            return Err(format!("off_seconds must be between 0.1 and 60, got {off_seconds}"));
        }
        let on = self.link.with_psu(|psu| psu.status()).await.map(|s| s.output_on);
        match on {
            Ok(true) => {}
            Ok(false) => {
                return Err("the output is off; switch it on with set_output instead".into());
            }
            Err(e) => return Err(format!("power cycle failed: {e}")),
        }
        self.act(format!("power cycle: output off for {off_seconds} s"), |psu| {
            psu.set_output(false)
        })
        .await?;
        tokio::time::sleep(Duration::from_secs_f64(off_seconds)).await;
        self.act("power cycle: output on".into(), |psu| psu.set_output(true)).await
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

/// `call_tool` comes from the macro; listing is filtered by the switch.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Bench {
    fn get_info(&self) -> ServerConfig {
        let capabilities =
            ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("steadystate", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = self.tool_router.list_all();
        tools.retain(|t| self.offered(&t.name));
        // The list depends on the switch, so it must not be cached.
        let hints = context.protocol_version().is_some_and(|v| v >= ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: hints.then_some(0),
            cache_scope: hints.then_some(CacheScope::Private),
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).filter(|t| self.offered(&t.name)).cloned()
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.control.register(context.peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use psu_core::psu::{DeviceInfo, Error, check_range};
    use rmcp::{ClientHandler, RoleClient, ServiceExt};
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
        client: RunningService<RoleClient, TestClient>,
    }

    /// Counts `notifications/tools/list_changed`.
    #[derive(Clone, Default)]
    struct TestClient {
        list_changed: Arc<AtomicUsize>,
    }

    impl ClientHandler for TestClient {
        async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
            self.list_changed.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn tool_names(h: &Harness) -> Vec<String> {
        let mut names: Vec<_> =
            h.client.list_all_tools().await.unwrap().iter().map(|t| t.name.to_string()).collect();
        names.sort();
        names
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
        tokio::spawn(serve(listener, link, notify, control.clone(), CancellationToken::new()));
        let transport = StreamableHttpClientTransport::from_uri(url.clone());
        let client = TestClient::default().serve(transport).await.unwrap();
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

        assert!(info.capabilities.tools.as_ref().unwrap().list_changed == Some(true));
        assert_eq!(
            tool_names(&h).await,
            ["apply", "get_status", "power_cycle", "rescan", "set_current", "set_output", "set_voltage"]
        );
        let tools = h.client.list_all_tools().await.unwrap();
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

    const ALWAYS: [&str; 4] = ["get_status", "power_cycle", "rescan", "set_output"];

    #[tokio::test(flavor = "multi_thread")]
    async fn switch_off_hides_setpoint_tools_but_keeps_power_switching() {
        let h = start_locked(Link::detached(Some(FakePsu::boxed()), connected())).await;
        assert_eq!(tool_names(&h).await, ALWAYS);

        // The agent can still see the supply...
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["connection"], "connected");
        assert_eq!(structured(&s)["device"]["model"], "Fake 30 V / 5 A");
        assert_eq!(structured(&s)["setpoint_tools_enabled"], false);
        // ...and switch its output.
        let r = call(&h, "set_output", json!({ "on": true })).await;
        assert_eq!(structured(&r)["readings"]["output_on"], true);
        let r = call(&h, "set_output", json!({ "on": false })).await;
        assert_eq!(structured(&r)["readings"]["output_on"], false);

        // Setpoint tools called anyway (stale list) are refused and change nothing.
        for (tool, args) in [
            ("set_voltage", json!({ "volts": 12.0 })),
            ("set_current", json!({ "amps": 1.0 })),
            ("apply", json!({ "volts": 12.0, "amps": 1.0 })),
        ] {
            let err = error_text(&call(&h, tool, args).await);
            assert!(err.contains("Full Agent Controls"), "{tool}: {err}");
        }
        let s = call(&h, "get_status", json!({})).await;
        assert_eq!(structured(&s)["readings"]["set_volts"], 5.0);
        assert_eq!(structured(&s)["readings"]["set_amps"], 1.0);
        assert_eq!(
            *h.notes.lock().unwrap(),
            [
                "Agent: output on",
                "Agent: output off",
                "Agent blocked: set voltage to 12 V",
                "Agent blocked: set current limit to 1 A",
                "Agent blocked: set 12 V, 1 A",
            ]
        );
    }

    async fn wait_for_list_changed(h: &Harness, count: usize) {
        for _ in 0..200 {
            if h.client.service().list_changed.load(Ordering::SeqCst) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no tools/list_changed #{count}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flipping_the_switch_updates_open_sessions() {
        let h = start_locked(Link::detached(Some(FakePsu::boxed()), connected())).await;
        assert_eq!(tool_names(&h).await, ALWAYS);

        h.control.set(true);
        wait_for_list_changed(&h, 1).await;
        assert_eq!(tool_names(&h).await.len(), 7);
        let r = call(&h, "set_voltage", json!({ "volts": 12.0 })).await;
        assert_eq!(structured(&r)["readings"]["set_volts"], 12.0);
        assert_eq!(structured(&r)["setpoint_tools_enabled"], true);

        // Setting it to what it already is sends nothing.
        h.control.set(true);
        h.control.set(false);
        wait_for_list_changed(&h, 2).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.client.service().list_changed.load(Ordering::SeqCst), 2);
        assert_eq!(tool_names(&h).await, ALWAYS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn power_cycle_turns_off_then_on() {
        let h = start_locked(Link::detached(Some(FakePsu::boxed()), connected())).await;
        let err = error_text(&call(&h, "power_cycle", json!({ "off_seconds": 0.1 })).await);
        assert!(err.contains("output is off"), "{err}");
        assert!(error_text(&call(&h, "power_cycle", json!({ "off_seconds": 0.0 })).await)
            .contains("between 0.1 and 60"));

        call(&h, "set_output", json!({ "on": true })).await;
        let started = std::time::Instant::now();
        let r = call(&h, "power_cycle", json!({ "off_seconds": 0.2 })).await;
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(structured(&r)["readings"]["output_on"], true);
        assert_eq!(
            h.notes.lock().unwrap()[1..],
            ["Agent: power cycle: output off for 0.2 s", "Agent: power cycle: output on"]
        );
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

    /// A port nothing listens on right now.
    fn free_port() -> u16 {
        bind(0).unwrap().local_addr().unwrap().port()
    }

    async fn connect(url: &str) -> Result<RunningService<RoleClient, TestClient>, String> {
        let transport = StreamableHttpClientTransport::from_uri(url.to_string());
        TestClient::default().serve(transport).await.map_err(|e| e.to_string())
    }

    async fn reachable(port: u16) -> bool {
        tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_moves_to_a_new_port() {
        let link = Link::detached(Some(FakePsu::boxed()), connected());
        let notify: Notify = Arc::new(|_| {});
        let (a, b) = (free_port(), free_port());
        let server = McpServer::start(link, notify, AgentControl::default(), a);
        let url_a = format!("http://127.0.0.1:{a}/mcp");
        assert_eq!(server.status(), McpStatus::Listening { url: url_a.clone(), port: a });
        let old = connect(&url_a).await.unwrap();
        assert_eq!(old.list_all_tools().await.unwrap().len(), 4);

        // Same port again: nothing happens, the session survives.
        server.set_port(a).unwrap();
        assert_eq!(old.list_all_tools().await.unwrap().len(), 4);

        let url_b = format!("http://127.0.0.1:{b}/mcp");
        assert_eq!(server.set_port(b).unwrap(), McpStatus::Listening { url: url_b.clone(), port: b });
        let new = connect(&url_b).await.unwrap();
        assert_eq!(new.list_all_tools().await.unwrap().len(), 4);
        // The old port is released and its session ended.
        for _ in 0..100 {
            if !reachable(a).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!reachable(a).await, "old port still open");
        assert!(old.list_all_tools().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn taken_port_keeps_the_current_server() {
        let link = Link::detached(None, connected());
        let notify: Notify = Arc::new(|_| {});
        let a = free_port();
        let server = McpServer::start(link, notify, AgentControl::default(), a);
        let taken = bind(0).unwrap();
        let b = taken.local_addr().unwrap().port();

        let err = server.set_port(b).unwrap_err();
        assert!(err.contains(&format!("127.0.0.1:{b}")), "{err}");
        assert!(matches!(server.status(), McpStatus::Listening { port, .. } if port == a));
        assert!(connect(&format!("http://127.0.0.1:{a}/mcp")).await.is_ok());
        assert!(server.set_port(0).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn taken_port_at_startup_is_reported_and_recoverable() {
        let taken = bind(0).unwrap();
        let b = taken.local_addr().unwrap().port();
        let notify: Notify = Arc::new(|_| {});
        let server = McpServer::start(Link::detached(None, connected()), notify, AgentControl::default(), b);
        assert!(matches!(server.status(), McpStatus::Failed { port, .. } if port == b));
        let a = free_port();
        assert!(matches!(server.set_port(a).unwrap(), McpStatus::Listening { port, .. } if port == a));
        assert!(connect(&format!("http://127.0.0.1:{a}/mcp")).await.is_ok());
    }
}
