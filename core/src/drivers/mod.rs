//! Driver registry and autodetection.
//!
//! To add a supply: implement [`Psu`] in a new module and append its [`Driver`] to
//! [`DRIVERS`]. Detection tries drivers in list order, so put drivers with the most
//! specific (least intrusive) probes first. On a USB port, drivers whose
//! [`Driver::usb_vids`] match the port's vendor id go first, so a supply usually sees only
//! its own probe even with several kinds attached.

use std::collections::BTreeMap;
use std::thread;

use serde::Serialize;
use serialport::SerialPortType;

use crate::psu::{DeviceInfo, Error, Psu, Result};
use crate::serial;

pub mod hm310t;
pub mod korad;

pub struct Driver {
    /// Stable id used by the CLI (`--driver`) and frontend.
    pub id: &'static str,
    pub name: &'static str,
    /// USB vendor ids of the device's serial bridge. Only a hint for probe order, never a
    /// filter: the same supply may sit behind a different adapter.
    pub usb_vids: &'static [u16],
    /// Opens `port` and identifies the device.
    ///
    /// Contract: must only *read* from the device (it doubles as the autodetect probe and
    /// may be pointed at unrelated hardware), and must fail with an error, not hang, if the
    /// device isn't the one this driver drives.
    pub open: fn(port: &str) -> Result<Box<dyn Psu>>,
}

pub const DRIVERS: &[Driver] = &[hm310t::DRIVER, korad::DRIVER];

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
/// Drivers matching the port's USB vendor id are tried first.
pub fn probe_port(port: &str, drivers: &[&'static Driver]) -> Result<Box<dyn Psu>> {
    if drivers.len() < 2 {
        return probe_in_order(port, drivers);
    }
    let vids = usb_vids()?;
    probe_in_order(port, &ordered_for(vids.get(port).copied(), drivers))
}

/// USB vendor id of each USB serial port, by port name.
fn usb_vids() -> Result<BTreeMap<String, u16>> {
    Ok(serial::list_ports()?
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(usb) => Some((p.port_name, usb.vid)),
            _ => None,
        })
        .collect())
}

/// `drivers`, with those claiming `vid` moved to the front (order otherwise kept).
fn ordered_for(vid: Option<u16>, drivers: &[&'static Driver]) -> Vec<&'static Driver> {
    let claims = |d: &&Driver| vid.is_some_and(|v| d.usb_vids.contains(&v));
    let (mut first, rest): (Vec<_>, Vec<_>) = drivers.iter().copied().partition(claims);
    first.extend(rest);
    first
}

fn probe_in_order(port: &str, drivers: &[&'static Driver]) -> Result<Box<dyn Psu>> {
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
    // Listed once here rather than per port in `probe_port`. If listing fails, probe in
    // plain list order.
    let vids = usb_vids().unwrap_or_default();

    thread::scope(|s| {
        let handles: Vec<_> = ports
            .iter()
            .map(|port| {
                let order = ordered_for(vids.get(port).copied(), drivers);
                s.spawn(move || {
                    probe_in_order(port, &order)
                        .ok()
                        .map(|psu| Detected { port: port.clone(), info: psu.info().clone() })
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_vid_reorders_drivers() {
        let all: Vec<&'static Driver> = DRIVERS.iter().collect();
        let ids = |v: Vec<&Driver>| v.iter().map(|d| d.id).collect::<Vec<_>>();
        assert_eq!(ids(ordered_for(None, &all)), ["hanmatek-hm310t", "korad-kaxxxxp"]);
        assert_eq!(ids(ordered_for(Some(0x1234), &all)), ["hanmatek-hm310t", "korad-kaxxxxp"]);
        assert_eq!(
            ids(ordered_for(Some(korad::USB_VID), &all)),
            ["korad-kaxxxxp", "hanmatek-hm310t"]
        );
    }

    #[test]
    fn driver_ids_are_unique() {
        for (i, d) in DRIVERS.iter().enumerate() {
            assert!(DRIVERS[i + 1..].iter().all(|o| o.id != d.id), "duplicate {}", d.id);
        }
    }
}
