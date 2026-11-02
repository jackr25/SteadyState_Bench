//! Driver registry and autodetection.
//!
//! To add a supply: implement [`Psu`] in a new module and append its [`Driver`] to
//! [`DRIVERS`]. Detection tries drivers in list order, so put drivers with the most
//! specific (least intrusive) probes first.

use std::thread;

use serde::Serialize;
use serialport::SerialPortType;

use crate::psu::{DeviceInfo, Error, Psu, Result};
use crate::serial;

pub mod hm310t;

pub struct Driver {
    /// Stable id used by the CLI (`--driver`) and frontend.
    pub id: &'static str,
    pub name: &'static str,
    /// Opens `port` and identifies the device.
    ///
    /// Contract: must only *read* from the device (it doubles as the autodetect probe and
    /// may be pointed at unrelated hardware), and must fail with an error, not hang, if the
    /// device isn't the one this driver drives.
    pub open: fn(port: &str) -> Result<Box<dyn Psu>>,
}

pub const DRIVERS: &[Driver] = &[hm310t::DRIVER];

pub fn find(id: &str) -> Option<&'static Driver> {
    DRIVERS.iter().find(|d| d.id == id)
}

/// A supply found by [`detect`]. The port is closed again; reopen with [`Detected::open`].
#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    pub port: String,
    pub info: DeviceInfo,
}

impl Detected {
    pub fn open(&self) -> Result<Box<dyn Psu>> {
        let driver = find(self.info.driver).expect("detected driver is registered");
        (driver.open)(&self.port)
    }
}

/// Tries each driver in `drivers` on `port`, returning the first that recognizes it.
pub fn probe_port(port: &str, drivers: &[&'static Driver]) -> Result<Box<dyn Psu>> {
    let mut last = Error::NotRecognized(format!("no driver recognized {port}"));
    for driver in drivers {
        match (driver.open)(port) {
            Ok(psu) => return Ok(psu),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Serial ports worth probing. Bluetooth virtual ports are skipped: opening one can block
/// for seconds while Windows tries to page the remote device.
pub fn candidate_ports() -> Result<Vec<String>> {
    Ok(serial::list_ports()?
        .into_iter()
        .filter(|p| !matches!(p.port_type, SerialPortType::BluetoothPort))
        .map(|p| p.port_name)
        .collect())
}

/// Probes every candidate port with `drivers` (all registered drivers if `None`). Ports are
/// probed in parallel; drivers on one port are tried sequentially, since only one can own
/// the port at a time.
pub fn detect(drivers: Option<&[&'static Driver]>) -> Result<Vec<Detected>> {
    Ok(detect_on(&candidate_ports()?, drivers))
}

/// Like [`detect`], but probes only `ports`. Lets long-running callers re-probe just the
/// ports that appeared since their last scan instead of poking every device again.
pub fn detect_on(ports: &[String], drivers: Option<&[&'static Driver]>) -> Vec<Detected> {
    let all: Vec<&'static Driver> = DRIVERS.iter().collect();
    let drivers = drivers.unwrap_or(&all);

    thread::scope(|s| {
        let handles: Vec<_> = ports
            .iter()
            .map(|port| {
                s.spawn(move || {
                    probe_port(port, drivers)
                        .ok()
                        .map(|psu| Detected { port: port.clone(), info: psu.info().clone() })
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    })
}
