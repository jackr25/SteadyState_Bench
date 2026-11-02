//! Device-agnostic power supply interface. Every driver (Modbus, SCPI, ...) implements
//! [`Psu`]; the GUI, CLI and future guardrail layer only talk to this trait.

use serde::Serialize;
use thiserror::Error;

use crate::modbus::ModbusError;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Modbus(#[from] ModbusError),
    #[error("serial port: {0}")]
    Serial(#[from] serialport::Error),
    #[error("serial I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The device answered, but isn't what this driver drives.
    #[error("device not recognized: {0}")]
    NotRecognized(String),
    #[error("{quantity} {value} is outside 0..={max}")]
    OutOfRange { quantity: &'static str, value: f64, max: f64 },
    #[error("{0} is not supported by this device")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Rejects non-finite or out-of-range values. Rejecting rather than clamping: silently
/// sending something other than what was asked is the dangerous failure on real hardware.
pub fn check_range(quantity: &'static str, value: f64, max: f64) -> Result<()> {
    if value.is_finite() && (0.0..=max).contains(&value) {
        Ok(())
    } else {
        Err(Error::OutOfRange { quantity, value, max })
    }
}

/// Output maximums the device accepts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Limits {
    pub voltage: f64,
    pub current: f64,
    pub power: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceInfo {
    /// Registry id of the driver, see [`crate::drivers::DRIVERS`].
    pub driver: &'static str,
    /// Human-readable model, e.g. "Hanmatek HM310T 32 V / 10 A".
    pub model: String,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub output_on: bool,
    pub v_meas: f64,
    pub i_meas: f64,
    pub p_meas: f64,
    pub v_set: f64,
    pub i_set: f64,
    /// Device-side protection settings, `None` if the device has none. These are
    /// informational only: never rely on them for safety (see HM310T findings).
    pub ovp: Option<f64>,
    pub ocp: Option<f64>,
}

pub trait Psu: Send {
    fn info(&self) -> &DeviceInfo;

    fn status(&mut self) -> Result<Status>;
    fn set_output(&mut self, on: bool) -> Result<()>;
    fn set_voltage(&mut self, volts: f64) -> Result<()>;
    fn set_current(&mut self, amps: f64) -> Result<()>;

    fn set_ovp(&mut self, _volts: f64) -> Result<()> {
        Err(Error::Unsupported("OVP"))
    }

    fn set_ocp(&mut self, _amps: f64) -> Result<()> {
        Err(Error::Unsupported("OCP"))
    }

    /// Applies a V/I pair. Both are validated before either is written, so a bad pair
    /// leaves the device untouched; current goes first so a higher-voltage setting never
    /// runs briefly against the old (possibly looser) current limit.
    fn apply(&mut self, volts: f64, amps: f64) -> Result<()> {
        let limits = self.info().limits;
        check_range("voltage", volts, limits.voltage)?;
        check_range("current", amps, limits.current)?;
        self.set_current(amps)?;
        self.set_voltage(volts)
    }
}
