//! Host-side named V/I presets.
//!
//! The HM310T has no Modbus-accessible memory slots, so presets live in a plain text file,
//! one per line: `<name> <volts> <amps>`. `#` starts a comment. Values are only checked for
//! sanity here; device limits are enforced when a preset is applied.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub voltage: f64,
    pub current: f64,
}

pub struct PresetStore {
    path: PathBuf,
    presets: BTreeMap<String, Preset>,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn check(name: &str, p: Preset) -> io::Result<()> {
    if name.is_empty() || name.chars().any(char::is_whitespace) || name.starts_with('#') {
        return Err(invalid(format!("bad preset name {name:?}")));
    }
    for v in [p.voltage, p.current] {
        if !(v.is_finite() && v >= 0.0) {
            return Err(invalid(format!("preset {name}: bad value {v}")));
        }
    }
    Ok(())
}

impl PresetStore {
    /// Loads the store; a missing file is an empty store.
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let presets = Self::parse(&text).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        Ok(Self { path, presets })
    }

    fn parse(text: &str) -> Result<BTreeMap<String, Preset>, String> {
        let mut presets = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [name, v, i] = fields[..] else {
                return Err(format!("line {}: expected `<name> <volts> <amps>`", n + 1));
            };
            let num = |s: &str| s.parse::<f64>().map_err(|e| format!("line {}: {e}", n + 1));
            let preset = Preset { voltage: num(v)?, current: num(i)? };
            check(name, preset).map_err(|e| format!("line {}: {e}", n + 1))?;
            presets.insert(name.to_string(), preset);
        }
        Ok(presets)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, name: &str) -> Option<Preset> {
        self.presets.get(name).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, Preset)> {
        self.presets.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn set(&mut self, name: &str, preset: Preset) -> io::Result<()> {
        check(name, preset)?;
        self.presets.insert(name.to_string(), preset);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.presets.remove(name).is_some()
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = String::from("# name volts amps\n");
        for (name, p) in &self.presets {
            text.push_str(&format!("{name} {} {}\n", p.voltage, p.current));
        }
        fs::write(&self.path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_and_round_trips() {
        let p = PresetStore::parse("# hdr\n3v3 3.3 0.5\n\n5v 5 1 # usb\n").unwrap();
        assert_eq!(p["3v3"], Preset { voltage: 3.3, current: 0.5 });
        assert_eq!(p["5v"], Preset { voltage: 5.0, current: 1.0 });
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(PresetStore::parse("3v3 3.3").is_err());
        assert!(PresetStore::parse("3v3 abc 0.5").is_err());
        assert!(PresetStore::parse("3v3 -1 0.5").is_err());
        assert!(PresetStore::parse("3v3 NaN 0.5").is_err());
    }
}
