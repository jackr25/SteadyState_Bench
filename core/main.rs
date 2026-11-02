//! `steady_state_cli` - drive a bench power supply from the command line.
//!
//! The supply is autodetected unless `--port` and/or `--driver` narrow the search.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use steady_state_bench::drivers::{self, DRIVERS, Driver, hm310t};
use steady_state_bench::presets::{Preset, PresetStore};
use steady_state_bench::psu::Psu;
use steady_state_bench::serial;

#[derive(Parser)]
#[command(version, about = "Control a bench power supply (autodetected by default)")]
struct Cli {
    /// Serial port, e.g. COM3 or /dev/ttyUSB0 [default: autodetect]
    #[arg(short, long, env = "PSU_PORT", global = true)]
    port: Option<String>,

    /// Driver id, see `drivers` [default: autodetect]
    #[arg(short, long, env = "PSU_DRIVER", global = true)]
    driver: Option<String>,

    /// Preset file [default: <config dir>/steady_state_bench/presets.txt]
    #[arg(long, env = "PSU_PRESETS", global = true)]
    presets: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List serial ports
    Ports,
    /// List supported drivers
    Drivers,
    /// Probe serial ports for supported supplies (read-only)
    Detect,
    /// Show setpoints, measurements, protection settings and device limits
    Status,
    /// Turn the output on
    On,
    /// Turn the output off
    Off,
    /// Set the voltage setpoint (V)
    Voltage { volts: f64 },
    /// Set the current limit (A)
    Current { amps: f64 },
    /// Set the device's over-voltage trip point (V). Not a safety measure on all devices.
    Ovp { volts: f64 },
    /// Set the device's over-current trip point (A). Not a safety measure on all devices.
    Ocp { amps: f64 },
    /// Named V/I presets, stored on this computer
    Preset {
        #[command(subcommand)]
        cmd: PresetCmd,
    },
    /// Read raw Modbus registers (hex or decimal address) on a Hanmatek, for probing
    RawRead {
        #[arg(value_parser = parse_u16)]
        start: u16,
        #[arg(default_value_t = 1)]
        count: u16,
    },
}

#[derive(Subcommand)]
enum PresetCmd {
    /// List saved presets
    List,
    /// Save a preset; with no values, captures the device's current setpoints
    Save { name: String, volts: Option<f64>, amps: Option<f64> },
    /// Apply a preset to the device setpoints (output state is not changed)
    Recall { name: String },
    /// Delete a preset
    Delete { name: String },
}

type CliResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn parse_u16(s: &str) -> Result<u16, String> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .map_err(|e| e.to_string())
}

fn default_preset_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("steady_state_bench").join("presets.txt"))
}

/// Selects which device to talk to; either part left `None` is autodetected.
struct Target {
    port: Option<String>,
    driver: Option<String>,
}

impl Target {
    fn drivers(&self) -> CliResult<Vec<&'static Driver>> {
        match &self.driver {
            None => Ok(DRIVERS.iter().collect()),
            Some(id) => Ok(vec![drivers::find(id).ok_or_else(|| {
                format!("unknown driver {id:?}; run `drivers` to list them")
            })?]),
        }
    }

    /// Returns the port to use, autodetecting it when not given. Exactly one supply must
    /// match; with several, the user has to pick, since guessing could drive the wrong one.
    fn resolve_port(&self, drivers: &[&'static Driver]) -> CliResult<String> {
        if let Some(port) = &self.port {
            return Ok(port.clone());
        }
        let mut found = drivers::detect(Some(drivers))?;
        match found.len() {
            0 => Err("no supported supply found on any serial port (try `detect`, `ports`)".into()),
            1 => {
                let d = found.remove(0);
                eprintln!("using {} on {}", d.info.model, d.port);
                Ok(d.port)
            }
            _ => {
                let list: Vec<String> =
                    found.iter().map(|d| format!("  {}  {}", d.port, d.info.model)).collect();
                Err(format!("several supplies found, pick one with --port:\n{}", list.join("\n")).into())
            }
        }
    }

    fn connect(&self) -> CliResult<Box<dyn Psu>> {
        let drivers = self.drivers()?;
        let port = self.resolve_port(&drivers)?;
        Ok(drivers::probe_port(&port, &drivers)?)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> CliResult {
    let target = Target { port: cli.port, driver: cli.driver };

    match cli.cmd {
        Cmd::Ports => {
            for p in serial::list_ports()? {
                println!("{}\t{:?}", p.port_name, p.port_type);
            }
        }
        Cmd::Drivers => {
            for d in DRIVERS {
                println!("{:<20} {}", d.id, d.name);
            }
        }
        Cmd::Detect => {
            let found = drivers::detect(Some(&target.drivers()?))?;
            if found.is_empty() {
                println!("no supported supplies found");
            }
            for d in found {
                println!("{:<8} {:<20} {}", d.port, d.info.driver, d.info.model);
            }
        }
        Cmd::Preset { cmd } => {
            let path = cli.presets.or_else(default_preset_path).ok_or("no preset path: pass --presets")?;
            run_preset(cmd, PresetStore::load(path)?, || target.connect())?;
        }
        Cmd::RawRead { start, count } => {
            let port = target.resolve_port(&[&hm310t::DRIVER])?;
            let mut psu = hm310t::Hm310t::connect(
                serial::open(&port, hm310t::BAUD_RATE)?,
                hm310t::DEFAULT_SLAVE,
            )?;
            for (i, v) in psu.raw_read(start, count)?.iter().enumerate() {
                println!("{:#06x}: {v:#06x} ({v})", usize::from(start) + i);
            }
        }
        cmd => {
            let mut psu = target.connect()?;
            match cmd {
                Cmd::Status => print_status(psu.as_mut())?,
                Cmd::On => psu.set_output(true)?,
                Cmd::Off => psu.set_output(false)?,
                Cmd::Voltage { volts } => psu.set_voltage(volts)?,
                Cmd::Current { amps } => psu.set_current(amps)?,
                Cmd::Ovp { volts } => psu.set_ovp(volts)?,
                Cmd::Ocp { amps } => psu.set_ocp(amps)?,
                _ => unreachable!("handled above"),
            }
        }
    }
    Ok(())
}

fn run_preset(
    cmd: PresetCmd,
    mut store: PresetStore,
    connect: impl FnOnce() -> CliResult<Box<dyn Psu>>,
) -> CliResult {
    match cmd {
        PresetCmd::List => {
            println!("# {}", store.path().display());
            for (name, p) in store.iter() {
                println!("{name:<12} {:6.2} V  {:6.3} A", p.voltage, p.current);
            }
        }
        PresetCmd::Save { name, volts, amps } => {
            let preset = match (volts, amps) {
                (Some(voltage), Some(current)) => Preset { voltage, current },
                (None, None) => {
                    let s = connect()?.status()?;
                    Preset { voltage: s.v_set, current: s.i_set }
                }
                _ => return Err("give both volts and amps, or neither".into()),
            };
            store.set(&name, preset)?;
            store.save()?;
            println!("saved {name}: {:.2} V, {:.3} A", preset.voltage, preset.current);
        }
        PresetCmd::Recall { name } => {
            let p = store.get(&name).ok_or_else(|| format!("no preset named {name:?}"))?;
            connect()?.apply(p.voltage, p.current)?;
            println!("{name} -> {:.2} V, {:.3} A", p.voltage, p.current);
        }
        PresetCmd::Delete { name } => {
            if !store.remove(&name) {
                return Err(format!("no preset named {name:?}").into());
            }
            store.save()?;
        }
    }
    Ok(())
}

fn print_status(psu: &mut dyn Psu) -> CliResult {
    let s = psu.status()?;
    let info = psu.info();
    let l = info.limits;
    let opt = |v: Option<f64>, unit: &str| v.map_or("n/a".to_string(), |v| format!("{v:.3} {unit}"));
    println!("device   : {} [{}]", info.model, info.driver);
    println!("output   : {}", if s.output_on { "ON" } else { "off" });
    println!("measured : {:6.2} V  {:6.3} A  {:7.3} W", s.v_meas, s.i_meas, s.p_meas);
    println!("setpoint : {:6.2} V  {:6.3} A", s.v_set, s.i_set);
    println!("protect  : OVP {}  OCP {}  (device-side, not trusted)", opt(s.ovp, "V"), opt(s.ocp, "A"));
    println!("limits   : {:.2} V  {:.3} A  {:.3} W", l.voltage, l.current, l.power);
    Ok(())
}
