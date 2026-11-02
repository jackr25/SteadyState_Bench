//! The last supply we connected to, kept between sessions so startup can try it first.

use std::fs;
use std::io;
use std::path::Path;

use crate::link::KnownDevice;

/// A missing or unreadable file just means "no previous device".
pub fn load(path: &Path) -> Option<KnownDevice> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .inspect_err(|e| log::warn!("ignoring {}: {e}", path.display()))
        .ok()
}

pub fn save(path: &Path, device: &KnownDevice) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(device)?)
}
