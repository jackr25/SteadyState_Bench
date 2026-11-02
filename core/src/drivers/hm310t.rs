//! Hanmatek HM310T interface control document (ICD) and driver.
//!
//! Sources, in order of trust:
//! 1. Register sweep of the HM310T on our bench (spec 3210, decimals 0x0233), 2026-11-02.
//! 2. joeyda3rd/hm310t (hardware-verified HM310T), incl. its copy of the OEM sheet
//!    `OEM-docs/Modbus.pdf` ("MODBUS_RTU", v20190531): <https://github.com/joeyda3rd/hm310t>
//! 3. sigrok "ETommens eTM-xxxxP Series" (HM310P/RS310P family):
//!    <https://sigrok.org/wiki/ETommens_eTM-xxxxP_Series>
//!
//! Link layer: Modbus RTU, 9600 8N1, slave address 1 (factory default), FC 0x03 / 0x06.
//! At most [`reg::MAX_READ_COUNT`] registers per read (more -> exception 0x03).
//! All values are unsigned fixed-point integers; decimal places come from [`reg::DECIMALS`].
//!
//! Not present on the HM310T (exception 0x02), although documented for the HM310P:
//! memory slots M1-M6 at 0x1000 + n*0x10, operating mode 0xA012, buzzer 0x8804,
//! 0x8801-0x8803, list dwell 0x0032, limit registers 0xC110-0xC12E. Presets are therefore
//! host-side, see [`crate::presets`].
//!
//! Live but undocumented (read-only observations, never written by this driver):
//! 0x0000 = 0, 0x0040..0x0044 = 0,1,0,0,0, 0x00F0..0x00F2 = 15,0,0.
//!
//! SAFETY (bench-tested 2026-11-02, no load): OVP/OCP are stored but NOT enforced. With
//! OVP = 6.00 V the output was driven to 7.00 V via 0x0030 and stayed on. Register 0x0002
//! read 2 in every state (off / on / over OVP), so it is not a protection latch either.
//! Lowering OVP below a live output didn't trip it, and the front panel showed no
//! OVP/OCP indication in either case.
//! Project decision: the device is untrusted. All safety limits are enforced host-side;
//! the OVP/OCP setters exist only to mirror the panel settings.
//!
//! OEM-sheet errata: OCP is 3 dp (sheet says 2); OPP is 3 dp (sheet says 2); slave range
//! is 1-250 (sheet says 1-15 in one place); the worked example contradicts the table.

use crate::modbus::{RtuMaster, Transport};
use crate::psu::{self, DeviceInfo, Error, Limits, Psu, Result, Status, check_range};
use crate::serial;

use super::Driver;

/// Register map. `R` = read-only, `RW` = read/write.
pub mod reg {
    /// Largest register count the firmware accepts in one FC 0x03 read.
    pub const MAX_READ_COUNT: u16 = 4;

    /// RW. Output enable: 1 = on, 0 = off.
    pub const OUTPUT: u16 = 0x0001;
    /// R. OEM: protection bit field (b0 OVP, b1 OCP, b2 OPP, b3 OTP, b4 SCP). On our unit it
    /// reads 2 constantly, including while above OVP, so the meaning is unknown.
    pub const PROTECTION: u16 = 0x0002;
    /// R. Rated spec, decimal VVAA: 3210 = 32 V / 10 A. Identity check.
    pub const SPEC: u16 = 0x0003;
    /// R. "Tail classification". Reads 0x4B43 (ASCII "KC") on our unit.
    pub const TAIL: u16 = 0x0004;
    /// R. Decimal places as nibbles `(V<<8)|(I<<4)|P`. HM310T = 0x0233. Identity check.
    pub const DECIMALS: u16 = 0x0005;

    /// R. Measured output voltage (V dp).
    pub const V_MEAS: u16 = 0x0010;
    /// R. Measured output current (I dp).
    pub const I_MEAS: u16 = 0x0011;
    /// R. Measured output power, 32-bit: 0x0012 high word, 0x0013 low word (P dp).
    pub const P_MEAS: u16 = 0x0012;
    /// R. Device maximums: 0x0014 voltage (V dp), 0x0015 current (I dp), 0x0016/0x0017
    /// power 32-bit (P dp). Undocumented; reads 32.00 V / 10.200 A / 326.400 W.
    pub const LIMITS: u16 = 0x0014;

    /// RW. Over-voltage protection trip point (V dp). Factory value 38.40 V = 1.2 x max.
    /// Stored but not enforced when setpoints are written over Modbus (see module docs).
    pub const OVP: u16 = 0x0020;
    /// RW. Over-current protection trip point (I dp). Not verified to be enforced.
    pub const OCP: u16 = 0x0021;
    /// RW per OEM, 32-bit hi/lo at 0x0022/0x0023 (P dp). Read-only in this driver: a
    /// 32-bit write takes two FC 0x06 writes, leaving a torn intermediate value.
    pub const OPP: u16 = 0x0022;

    /// RW. Voltage setpoint (V dp).
    pub const V_SET: u16 = 0x0030;
    /// RW. Current limit setpoint (I dp).
    pub const I_SET: u16 = 0x0031;

    /// RW. Modbus slave address, 1-250. Changing it drops the current session.
    pub const COMM_ADDR: u16 = 0x9999;
}

/// Link settings (OEM sheet): 9600 8N1, factory slave address 1.
pub const BAUD_RATE: u32 = 9600;
pub const DEFAULT_SLAVE: u8 = 1;

/// Spec values sharing this map and 0x0233 scaling: 3010 (30 V/10 A, per hm310t project)
/// and 3210 (32 V/10 A, our bench unit).
pub const KNOWN_SPECS: [u16; 2] = [3010, 3210];
pub const EXPECTED_DECIMALS: u16 = 0x0233;

/// Protection trip points may exceed the output maximum by this factor (factory OVP is
/// exactly 1.2 x the 32 V maximum).
const PROTECTION_HEADROOM: f64 = 1.2;

pub const DRIVER: Driver = Driver {
    id: "hanmatek-hm310t",
    name: "Hanmatek HM310T (Modbus RTU)",
    open: open_port,
};

/// Read-only probe: opening only reads the identity and limit registers.
fn open_port(port: &str) -> Result<Box<dyn Psu>> {
    let transport = serial::open(port, BAUD_RATE)?;
    Ok(Box::new(Hm310t::connect(transport, DEFAULT_SLAVE)?))
}

/// Decimal places per quantity, decoded from [`reg::DECIMALS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimals {
    pub voltage: u8,
    pub current: u8,
    pub power: u8,
}

impl Decimals {
    pub fn from_raw(raw: u16) -> Self {
        let nibble = |shift: u16| ((raw >> shift) & 0xF) as u8;
        Self { voltage: nibble(8), current: nibble(4), power: nibble(0) }
    }
}

/// Converts a physical value to the device's fixed-point register value.
///
/// `quantity` names the value for error messages ("voltage", "OCP", ...), `decimals` is the
/// register's decimal places, `max` is the largest value the caller allows.
fn encode(quantity: &'static str, value: f64, decimals: u8, max: f64) -> Result<u16> {
    check_range(quantity, value, max)?;
    let raw = (value * scale(decimals)).round();
    if raw > f64::from(u16::MAX) {
        return Err(Error::OutOfRange { quantity, value, max });
    }
    Ok(raw as u16)
}

fn scale(decimals: u8) -> f64 {
    10f64.powi(i32::from(decimals))
}

fn decode(raw: u16, decimals: u8) -> f64 {
    f64::from(raw) / scale(decimals)
}

fn decode32(hi: u16, lo: u16, decimals: u8) -> f64 {
    f64::from((u32::from(hi) << 16) | u32::from(lo)) / scale(decimals)
}

/// Driver for one HM310T on a Modbus bus.
pub struct Hm310t<T> {
    bus: RtuMaster<T>,
    decimals: Decimals,
    info: DeviceInfo,
}

impl<T: Transport> Hm310t<T> {
    /// Verifies the device identity before handing out a driver, so we never write
    /// setpoints with the wrong scaling to an unknown supply. Read-only.
    pub fn connect(port: T, slave: u8) -> Result<Self> {
        let mut bus = RtuMaster::new(port, slave);
        let id = bus.read_holding(reg::SPEC, 3)?; // SPEC, TAIL, DECIMALS
        let (spec, raw_decimals) = (id[0], id[2]);
        if !KNOWN_SPECS.contains(&spec) || raw_decimals != EXPECTED_DECIMALS {
            return Err(Error::NotRecognized(format!(
                "spec register {spec} (want one of {KNOWN_SPECS:?}), \
                 decimals {raw_decimals:#06x} (want {EXPECTED_DECIMALS:#06x})"
            )));
        }
        let d = Decimals::from_raw(raw_decimals);
        let l = bus.read_holding(reg::LIMITS, 4)?;
        let limits = Limits {
            voltage: decode(l[0], d.voltage),
            current: decode(l[1], d.current),
            power: decode32(l[2], l[3], d.power),
        };
        let info = DeviceInfo {
            driver: DRIVER.id,
            model: format!("Hanmatek HM310T {} V / {} A", spec / 100, spec % 100),
            limits,
        };
        Ok(Self { bus, decimals: d, info })
    }

    /// Unscaled read for probing undocumented registers. Read-only on purpose; split into
    /// chunks the firmware accepts.
    pub fn raw_read(&mut self, start: u16, count: u16) -> Result<Vec<u16>> {
        let mut out = Vec::with_capacity(usize::from(count));
        let mut addr = start;
        let mut left = count;
        while left > 0 {
            let n = left.min(reg::MAX_READ_COUNT);
            out.extend(self.bus.read_holding(addr, n)?);
            addr = addr.wrapping_add(n);
            left -= n;
        }
        Ok(out)
    }
}

impl<T: Transport + Send> Psu for Hm310t<T> {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn set_output(&mut self, on: bool) -> Result<()> {
        Ok(self.bus.write_single(reg::OUTPUT, u16::from(on))?)
    }

    fn set_voltage(&mut self, volts: f64) -> Result<()> {
        let raw = encode("voltage", volts, self.decimals.voltage, self.info.limits.voltage)?;
        Ok(self.bus.write_single(reg::V_SET, raw)?)
    }

    fn set_current(&mut self, amps: f64) -> Result<()> {
        let raw = encode("current", amps, self.decimals.current, self.info.limits.current)?;
        Ok(self.bus.write_single(reg::I_SET, raw)?)
    }

    fn set_ovp(&mut self, volts: f64) -> Result<()> {
        let max = self.info.limits.voltage * PROTECTION_HEADROOM;
        let raw = encode("OVP", volts, self.decimals.voltage, max)?;
        Ok(self.bus.write_single(reg::OVP, raw)?)
    }

    fn set_ocp(&mut self, amps: f64) -> Result<()> {
        let max = self.info.limits.current * PROTECTION_HEADROOM;
        let raw = encode("OCP", amps, self.decimals.current, max)?;
        Ok(self.bus.write_single(reg::OCP, raw)?)
    }

    fn status(&mut self) -> psu::Result<Status> {
        let d = self.decimals;
        let state = self.bus.read_holding(reg::OUTPUT, 1)?;
        let meas = self.bus.read_holding(reg::V_MEAS, 4)?;
        let prot = self.bus.read_holding(reg::OVP, 2)?;
        let set = self.bus.read_holding(reg::V_SET, 2)?;

        Ok(Status {
            output_on: state[0] != 0,
            v_meas: decode(meas[0], d.voltage),
            i_meas: decode(meas[1], d.current),
            p_meas: decode32(meas[2], meas[3], d.power),
            v_set: decode(set[0], d.voltage),
            i_set: decode(set[1], d.current),
            ovp: Some(decode(prot[0], d.voltage)),
            ocp: Some(decode(prot[1], d.current)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::tests::MockPort;

    fn connected() -> Hm310t<MockPort> {
        let mut port = MockPort::default();
        // SPEC=3210, TAIL="KC", DECIMALS=0x0233
        port.reply(1, &[0x03, 0x06, 0x0C, 0x8A, 0x4B, 0x43, 0x02, 0x33]);
        // LIMITS: 32.00 V, 10.200 A, 326.400 W (as read from the bench unit)
        port.reply(1, &[0x03, 0x08, 0x0C, 0x80, 0x27, 0xD8, 0x00, 0x04, 0xFB, 0x00]);
        Hm310t::connect(port, 1).unwrap()
    }

    #[test]
    fn decimals_nibbles() {
        assert_eq!(
            Decimals::from_raw(0x0233),
            Decimals { voltage: 2, current: 3, power: 3 }
        );
    }

    #[test]
    fn identity_and_limits() {
        let psu = connected();
        assert_eq!(psu.info().limits, Limits { voltage: 32.0, current: 10.2, power: 326.4 });
        assert_eq!(psu.info().model, "Hanmatek HM310T 32 V / 10 A");
        assert_eq!(psu.info().driver, "hanmatek-hm310t");
    }

    #[test]
    fn rejects_unknown_model() {
        let mut port = MockPort::default();
        port.reply(1, &[0x03, 0x06, 0x0B, 0xBA, 0x00, 0x00, 0x02, 0x33]); // 3002
        assert!(matches!(Hm310t::connect(port, 1), Err(Error::NotRecognized(_))));
    }

    #[test]
    fn encode_scales_and_rounds() {
        assert_eq!(encode("voltage", 12.34, 2, 30.0).unwrap(), 1234);
        assert_eq!(encode("current", 1.5, 3, 10.0).unwrap(), 1500);
        // 0.29 * 100 = 28.999999999999996 in f64 - must not truncate to 28.
        assert_eq!(encode("voltage", 0.29, 2, 30.0).unwrap(), 29);
        assert_eq!(encode("voltage", 30.0, 2, 30.0).unwrap(), 3000);
    }

    #[test]
    fn encode_rejects_bad_values() {
        for bad in [-0.01, 30.01, f64::NAN, f64::INFINITY] {
            assert!(encode("voltage", bad, 2, 30.0).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn set_voltage_writes_scaled_register() {
        let mut psu = connected();
        psu.port().reply(1, &[0x06, 0x00, 0x30, 0x04, 0xB0]);
        psu.port().written.clear();
        psu.set_voltage(12.0).unwrap();
        assert_eq!(&psu.port().written[..6], &[0x01, 0x06, 0x00, 0x30, 0x04, 0xB0]);
    }

    #[test]
    fn apply_rejects_pair_without_writing() {
        let mut psu = connected();
        psu.port().written.clear();
        assert!(psu.apply(40.0, 1.0).is_err());
        assert!(psu.port().written.is_empty());
    }

    #[test]
    fn raw_read_chunks_to_four_registers() {
        let mut psu = connected();
        psu.port().reply(1, &[0x03, 0x08, 0, 1, 0, 2, 0, 3, 0, 4]);
        psu.port().reply(1, &[0x03, 0x02, 0, 5]);
        assert_eq!(psu.raw_read(0x0001, 5).unwrap(), vec![1, 2, 3, 4, 5]);
    }

    impl Hm310t<MockPort> {
        fn port(&mut self) -> &mut MockPort {
            self.bus.port_mut()
        }
    }
}

