mod link;
mod persist;

use link::{Link, Snapshot};
use tauri::{Manager, State};

#[tauri::command]
fn snapshot(link: State<'_, Link>) -> Snapshot {
    link.snapshot()
}

#[tauri::command]
fn rescan(link: State<'_, Link>) {
    link.rescan();
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
            app.manage(Link::start(app.handle().clone(), last_device));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            rescan,
            set_voltage,
            set_current,
            set_output
        ])
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
