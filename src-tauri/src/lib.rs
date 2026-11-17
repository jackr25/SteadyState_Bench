mod link;
mod mcp;
mod persist;

use std::path::PathBuf;
use std::sync::Arc;

use link::{Link, Snapshot};
use mcp::{AgentControl, McpServer, McpStatus};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

/// Agent actions, shown in the footer so the person at the bench sees them.
pub const EVENT_AGENT: &str = "psu://agent";

#[tauri::command]
fn snapshot(link: State<'_, Link>) -> Snapshot {
    link.snapshot()
}

/// MCP settings kept between sessions. The agent-control switch is deliberately not saved:
/// it starts off every launch.
#[derive(Serialize, Deserialize)]
struct McpSettings {
    port: u16,
}

struct Mcp {
    server: McpServer,
    settings_path: PathBuf,
}

#[tauri::command]
fn mcp_info(mcp: State<'_, Mcp>) -> McpStatus {
    mcp.server.status()
}

/// Moves the MCP server to `port` and remembers it. On error the server stays where it was.
#[tauri::command]
fn set_mcp_port(mcp: State<'_, Mcp>, port: u16) -> Result<McpStatus, String> {
    let status = mcp.server.set_port(port)?;
    if let Err(e) = persist::save(&mcp.settings_path, &McpSettings { port }) {
        log::warn!("can't save {}: {e}", mcp.settings_path.display());
    }
    Ok(status)
}

#[tauri::command]
fn agent_control(control: State<'_, AgentControl>) -> bool {
    control.enabled()
}

/// The "Agentic power configuration" switch.
#[tauri::command]
fn set_agent_control(control: State<'_, AgentControl>, on: bool) -> bool {
    control.set(on);
    log::info!("agentic power configuration {}", if on { "on" } else { "off" });
    on
}

#[tauri::command]
fn rescan(link: State<'_, Link>) {
    link.rescan();
}

#[tauri::command]
fn pick_device(link: State<'_, Link>, port: String, driver: String) -> Result<(), String> {
    link.pick(port, driver)
}

#[tauri::command]
fn dismiss_choice(link: State<'_, Link>) {
    link.dismiss();
}

#[tauri::command]
async fn set_voltage(link: State<'_, Link>, volts: f64) -> Result<(), String> {
    link.with_psu(move |psu| psu.set_voltage(volts)).await
}

#[tauri::command]
async fn set_current(link: State<'_, Link>, amps: f64) -> Result<(), String> {
    link.with_psu(move |psu| psu.set_current(amps)).await
}

#[tauri::command]
async fn set_output(link: State<'_, Link>, on: bool) -> Result<(), String> {
    link.with_psu(move |psu| psu.set_output(on)).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            let config_dir = app.path().app_config_dir()?;
            let link = Link::start(app.handle().clone(), config_dir.join("last_device.json"));
            let control = AgentControl::default();
            let settings_path = config_dir.join("mcp.json");
            let server = start_mcp(app.handle(), link.clone(), control.clone(), &settings_path);
            app.manage(Mcp { server, settings_path });
            app.manage(control);
            app.manage(link);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            mcp_info,
            set_mcp_port,
            agent_control,
            set_agent_control,
            rescan,
            pick_device,
            dismiss_choice,
            set_voltage,
            set_current,
            set_output
        ])
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}

/// Starts the MCP server for agents on the saved port; never fails app startup.
fn start_mcp(
    app: &AppHandle,
    link: Link,
    control: AgentControl,
    settings_path: &std::path::Path,
) -> McpServer {
    let port = persist::load::<McpSettings>(settings_path).map_or(mcp::DEFAULT_PORT, |s| s.port);
    let emitter = app.clone();
    let notify: mcp::Notify = Arc::new(move |note| {
        log::info!("{note}");
        let _ = emitter.emit(EVENT_AGENT, note);
    });
    McpServer::start(link, notify, control, port)
}
