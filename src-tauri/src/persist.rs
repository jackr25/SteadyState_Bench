//! Small JSON files kept between sessions: the last supply we connected to (so startup can
//! try it first) and the MCP settings.

use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A missing or unreadable file just means "nothing saved".
pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .inspect_err(|e| log::warn!("ignoring {}: {e}", path.display()))
        .ok()
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)
}
