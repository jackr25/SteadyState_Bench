mod link;
mod mcp;
mod persist;

use std::sync::Arc;

use link::{Link, Snapshot};
use mcp::{AgentControl, McpStatus};
use tauri::{AppHandle, Emitter, Manager, State};

/// Agent actions, shown in the footer so the person at the bench sees them.
pub const EVENT_AGENT: &str = "psu://agent";

#[tauri::command]
fn snapshot(link: State<'_, Link>) -> Snapshot {
    link.snapshot()
}

#[tauri::command]
fn mcp_info(status: State<'_, McpStatus>) -> McpStatus {
    status.inner().clone()
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
            let last_device = app.path().app_config_dir()?.join("last_device.json");
            let link = Link::start(app.handle().clone(), last_device);
            let control = AgentControl::default();
            app.manage(start_mcp(app.handle(), link.clone(), control.clone()));
            app.manage(control);
            app.manage(link);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            mcp_info,
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

/// Starts the MCP server for agents unless disabled; never fails app startup.
fn start_mcp(app: &AppHandle, link: Link, control: AgentControl) -> McpStatus {
    let port = match mcp::port_from_env() {
        None => return McpStatus::Disabled,
        Some(Err(error)) => return McpStatus::Failed { error },
        Some(Ok(port)) => port,
    };
    let listener = match mcp::bind(port) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("MCP server not started: {e}");
            let error = format!("can't listen on 127.0.0.1:{port}: {e}");
            return McpStatus::Failed { error };
        }
    };
    let url = match listener.local_addr() {
        Ok(addr) => mcp::url(addr),
        Err(e) => return McpStatus::Failed { error: e.to_string() },
    };
    let emitter = app.clone();
    let notify: mcp::Notify = Arc::new(move |note| {
        log::info!("{note}");
        let _ = emitter.emit(EVENT_AGENT, note);
    });
    tauri::async_runtime::spawn(async move {
        if let Err(e) = mcp::serve(listener, link, notify, control).await {
            log::error!("MCP server stopped: {e}");
        }
    });
    McpStatus::Listening { url }
}
