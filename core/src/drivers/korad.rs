//! Korad KAxxxxP / KDxxxxP single-channel supplies (and rebadges: RND 320-, Tenma 72-,
//! Velleman PS/LABPS, Stamos S-LS-31) over their USB virtual COM port or RS-232.
//!
//! These are the plain serial models, not the KWR/KA-LAN series with SCPI over Ethernet.
//!
//! Sources, in order of trust:
//! 1. sigrok `korad-kaxxxxp` driver (protocol.c / api.c), which carries quirks gathered
//!    from many units: <https://github.com/sigrokproject/libsigrok/tree/master/src/hardware/korad-kaxxxxp>
//! 2. KA-series programming manual ("single channel remote control syntax"), via the
//!    sigrok wiki: <https://sigrok.org/wiki/Korad_KAxxxxP_series>
//! 3. Nicoretti/ka3005p (Rust CLI used on Korad/Tenma/Velleman units):
//!    <https://github.com/Nicoretti/ka3005p>
//! 4. tspi.at, "Controlling the Korad KA3005P using Python" (2022): 19200 baud on its unit,
//!    and `*IDN?` answered only once per power-up on some firmware.
//!
//! Link layer: 8N1, no flow control. Manuals say 9600 baud; one report says 19200, so the
//! probe tries [`BAUD_RATES`] in order and remembers what worked. Over the USB port (a
//! CDC-ACM device, VID 0x0416) the baud rate is ignored, so the first attempt always works.
//!
//! Framing: plain ASCII, NO terminator in either direction. The device finds the end of a
//! command by a pause on the line, so commands must be spaced out (sigrok: 80 ms), or two
//! writes merge into one garbled command that is silently dropped. Replies are likewise
//! unterminated; we read until the line goes idle. Set commands are never acknowledged,
//! so the driver reads setpoints back to confirm them.
//!
//! Commands used (channel 1 only):
//! - `*IDN?` -> e.g. `KORAD KA3005P V5.8 SN:03379314`. Spacing varies by firmware
//!   (`KORADKA3005PV2.0`), and some append a stray byte (0x01 or 0xBC).
//! - `VSET1:<v>` / `VSET1?`: voltage setpoint, 2 dp (`12.00`).
//! - `ISET1:<i>` / `ISET1?`: current limit, 3 dp (`1.500`). Some firmware sends one extra
//!   junk byte after the ISET1? reply; reading to idle and parsing the leading number
//!   absorbs it.
//! - `VOUT1?` / `IOUT1?`: measured output, same formats.
//! - `OUT1` / `OUT0`: output on / off.
//! - `STATUS?` -> one raw (binary, not ASCII) byte, see [`status`].
//!
//! Not used: `OVP1/0`, `OCP1/0` only switch protection on and off (the trip points are
//! panel settings, not readable), so [`Psu::set_ovp`] / [`Psu::set_ocp`] stay unsupported.
//! `BEEP`, `LOCK`, `SAV`/`RCL` memories (RCL also switches the output off), `TRACK`.
//!
//! Known limitation: on firmware that answers `*IDN?` only once per power-up, a second
//! program run won't recognize the supply until it's power-cycled. Within one run the
//! identity is cached per port (see [`connect`]), so reconnects and the detect-then-open
//! sequence don't depend on a second `*IDN?`.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::modbus::Transport;
use crate::psu::{DeviceInfo, Error, Limits, Psu, Result, Status, check_range};
use crate::serial::{self, SerialTransport};

use super::Driver;

/// Query and command strings, sent as-is with no terminator.
pub mod cmd {
    pub const IDN: &str = "*IDN?";
    pub const STATUS: &str = "STATUS?";
    pub const VSET: &str = "VSET1?";
    pub const ISET: &str = "ISET1?";
    pub const VOUT: &str = "VOUT1?";
    pub const IOUT: &str = "IOUT1?";
    pub const OUT_ON: &str = "OUT1";
    pub const OUT_OFF: &str = "OUT0";

    pub fn set_voltage(volts: f64) -> String {
        format!("VSET1:{volts:.2}")
    }

    pub fn set_current(amps: f64) -> String {
        format!("ISET1:{amps:.3}")
    }
}

/// `STATUS?` reply bits, per sigrok (agrees with Nicoretti/ka3005p on bits 0, 1, 4, 6).
pub mod status {
    /// Channel 1 mode: set = constant voltage, clear = constant current.
    pub const CH1_CV: u8 = 1 << 0;
    /// Beep enabled. Reported as always 0 by some KA3005P owners.
    pub const BEEP: u8 = 1 << 4;
    /// sigrok: OCP enabled. Nicoretti and the manual: panel lock. Unused here.
    pub const BIT5: u8 = 1 << 5;
    /// Output enabled. The only bit this driver relies on.
    pub const OUTPUT: u8 = 1 << 6;
    /// sigrok: OVP enabled (Velleman LABPS only updates it while the output is on).
    pub const OVP: u8 = 1 << 7;
}

/// Baud rates tried by the probe, most common first.
pub const BAUD_RATES: [u32; 2] = [9600, 19200];

/// USB vendor id of the supplies' built-in virtual COM port (Nuvoton, PID 0x5011).
pub const USB_VID: u16 = 0x0416;

/// Decimal places the device accepts and reports.
const VOLTAGE_DP: i32 = 2;
const CURRENT_DP: i32 = 3;

/// Upper bound on any reply (`*IDN?` is about 35 bytes); guards against a chatty device.
const MAX_REPLY: usize = 64;

/// Per-read timeout on the serial port: the granularity of the idle-gap detection.
const PORT_READ_TIMEOUT: Duration = Duration::from_millis(10);

/// Protocol timing. Only tests use anything but [`Timing::default`].
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Minimum time between the starts of two commands (sigrok: 80 ms).
    pub cmd_interval: Duration,
    /// How long to wait for the first byte of a reply. Also bounds the probe on a silent port.
    pub reply_timeout: Duration,
    /// Silence that ends a reply. A byte takes ~1 ms at 9600 baud, and USB delivers a
    /// whole reply in one packet.
    pub idle_gap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            cmd_interval: Duration::from_millis(80),
            reply_timeout: Duration::from_millis(300),
            idle_gap: Duration::from_millis(30),
        }
    }
}

pub const DRIVER: Driver = Driver {
    id: "korad-kaxxxxp",
    name: "Korad KA/KD 3005P family (serial text protocol)",
    usb_vids: &[USB_VID],
    open: open_port,
};

/// Ratings of a recognized model.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub limits: Limits,
}

impl Model {
    fn rated(volts: u32, amps: u32) -> Self {
        let (v, a) = (f64::from(volts), f64::from(amps));
        Self { limits: Limits { voltage: v, current: a, power: v * a } }
    }
}

/// Rebadges whose `*IDN?` doesn't carry a Korad-style part number (per sigrok's table).
const NAMED_MODELS: [(&str, u32, u32); 6] = [
    ("72-2535", 30, 3),
    ("72-2540", 30, 5),
    ("72-2550", 60, 3),
    ("72-2705", 30, 3),
    ("72-2710", 30, 5),
    ("S-LS-31", 30, 5),
];

/// Recognizes a model from its cleaned `*IDN?` reply.
///
/// Korad part numbers are `KA`/`KD` + `VVAA` + `P`/`D`, rated VV volts and AA amps
/// (KA3005P = 30 V / 5 A, KD6005P = 60 V / 5 A, KA3010P = 30 V / 10 A), searched anywhere
/// in the reply so vendor prefixes (`RND 320-`) and missing spaces don't matter. Velleman
/// uses the same scheme with `PS`. `33xx` parts are the multi-channel KA33 series, which
/// this single-channel driver doesn't handle. Limits are the rated values, not the few
/// percent of overrange some units accept: the host never asks for more than the rating.
pub fn parse_model(idn: &str) -> Option<Model> {
    let upper = idn.to_ascii_uppercase();
    if let Some(&(_, v, a)) = NAMED_MODELS.iter().find(|(name, ..)| upper.contains(name)) {
        return Some(Model::rated(v, a));
    }
    let velleman = upper.contains("VELLEMAN");
    let b = upper.as_bytes();
    (0..b.len().saturating_sub(6)).find_map(|i| {
        let prefix = &b[i..i + 2];
        let korad = prefix == b"KA" || prefix == b"KD";
        if !(korad || (velleman && prefix == b"PS")) {
            return None;
        }
        let digits = &b[i + 2..i + 6];
        if !digits.iter().all(u8::is_ascii_digit) || !matches!(b[i + 6], b'P' | b'D') {
            return None;
        }
        let num = |d: &[u8]| u32::from(d[0] - b'0') * 10 + u32::from(d[1] - b'0');
        let (volts, amps) = (num(&digits[..2]), num(&digits[2..]));
        (volts != 33 && volts > 0 && amps > 0).then(|| Model::rated(volts, amps))
    })
}

/// Keeps the printable part of an `*IDN?` reply and drops the serial number.
fn clean_idn(reply: &[u8]) -> String {
    let text: String =
        reply.iter().filter(|b| (0x20..0x7F).contains(*b)).map(|&b| char::from(b)).collect();
    let text = match text.find("SN:") {
        Some(i) => &text[..i],
        None => &text,
    };
    text.trim().to_string()
}

/// Parses the leading number of a value reply (`"12.00"`, `"1.500"` + junk byte).
fn parse_number(command: &str, reply: &[u8]) -> Result<f64> {
    let text: String = reply
        .iter()
        .map(|&b| char::from(b))
        .skip_while(|c| c.is_ascii_whitespace())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    text.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| {
        Error::Unexpected(format!("{command} replied {:?}", String::from_utf8_lossy(reply)))
    })
}

fn round_to(value: f64, dp: i32) -> f64 {
    let s = 10f64.powi(dp);
    (value * s).round() / s
}

/// Command pacing and unterminated-reply reading.
struct Wire<T> {
    port: T,
    timing: Timing,
    last_cmd: Option<Instant>,
}

impl<T: Transport> Wire<T> {
    fn send(&mut self, command: &str) -> Result<()> {
        if let Some(last) = self.last_cmd {
            let ready = last + self.timing.cmd_interval;
            let now = Instant::now();
            if ready > now {
                thread::sleep(ready - now);
            }
        }
        // Drop anything stale (e.g. a late tail of the previous reply).
        self.port.discard_input()?;
        self.port.write_all(command.as_bytes())?;
        self.port.flush()?;
        self.last_cmd = Some(Instant::now());
        Ok(())
    }

    fn query(&mut self, command: &str) -> Result<Vec<u8>> {
        self.send(command)?;
        self.read_reply()
    }

    fn query_number(&mut self, command: &str) -> Result<f64> {
        let reply = self.query(command)?;
        parse_number(command, &reply)
    }

    /// Reads until the line has been idle for `idle_gap` after the first byte.
    fn read_reply(&mut self) -> Result<Vec<u8>> {
        let start = Instant::now();
        let mut last_rx: Option<Instant> = None;
        let mut out = Vec::new();
        let mut buf = [0u8; MAX_REPLY];
        loop {
            match self.port.read(&mut buf) {
                Ok(0) => thread::sleep(Duration::from_millis(1)),
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    last_rx = Some(Instant::now());
                    if out.len() >= MAX_REPLY {
                        return Ok(out);
                    }
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
            match last_rx {
                Some(t) if t.elapsed() >= self.timing.idle_gap => return Ok(out),
                None if start.elapsed() >= self.timing.reply_timeout => return Err(Error::Timeout),
                _ => {}
            }
        }
    }
}

/// Driver for one Korad-protocol supply.
pub struct Korad<T> {
    wire: Wire<T>,
    /// Cleaned `*IDN?` reply, kept so the port can be reopened without asking again.
    idn: String,
    info: DeviceInfo,
}

impl<T: Transport> Korad<T> {
    /// Identifies the device with `*IDN?`. Read-only.
    pub fn identify(port: T, timing: Timing) -> Result<Self> {
        let mut wire = Wire { port, timing, last_cmd: None };
        let reply = wire.query(cmd::IDN)?;
        Self::from_idn(wire, clean_idn(&reply))
    }

    /// Reattaches to a device identified earlier as `idn`, without sending `*IDN?` (see
    /// module docs). Confirms it still speaks the protocol with a `VSET1?` read. Read-only.
    pub fn resume(port: T, idn: &str, timing: Timing) -> Result<Self> {
        let mut wire = Wire { port, timing, last_cmd: None };
        let reply = wire.query(cmd::VSET)?;
        let this = Self::from_idn(wire, idn.to_string())?;
        let v = parse_number(cmd::VSET, &reply)?;
        // Units accept a little over their rating from the panel.
        if v > this.info.limits.voltage * 1.1 {
            return Err(Error::NotRecognized(format!("VSET1? = {v} on a {idn}")));
        }
        Ok(this)
    }

    fn from_idn(wire: Wire<T>, idn: String) -> Result<Self> {
        let model = parse_model(&idn)
            .ok_or_else(|| Error::NotRecognized(format!("*IDN? replied {idn:?}")))?;
        let l = model.limits;
        let info = DeviceInfo {
            driver: DRIVER.id,
            model: format!("{idn} ({} V / {} A)", l.voltage, l.current),
            limits: l,
        };
        Ok(Self { wire, idn, info })
    }

    pub fn idn(&self) -> &str {
        &self.idn
    }

    /// Sends a query and returns the raw reply, for probing. Only `?` queries are allowed,
    /// so this can't change the device.
    pub fn raw_query(&mut self, query: &str) -> Result<Vec<u8>> {
        if !query.ends_with('?') {
            return Err(Error::Unexpected(format!("{query:?} is not a query (must end in '?')")));
        }
        self.wire.query(query)
    }

    /// Sends a setpoint and reads it back: the device never acknowledges commands, and
    /// drops ones that run together on the line.
    fn set_verified(&mut self, command: String, query: &str, want: f64, dp: i32) -> Result<()> {
        self.wire.send(&command)?;
        let got = self.wire.query_number(query)?;
        if (got - want).abs() > 0.5 * 10f64.powi(-dp) {
            return Err(Error::Unexpected(format!(
                "sent {command}, but {query} reads back {got}"
            )));
        }
        Ok(())
    }
}

impl<T: Transport + Send> Psu for Korad<T> {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn status(&mut self) -> Result<Status> {
        let flags = *self
            .wire
            .query(cmd::STATUS)?
            .first()
            .ok_or_else(|| Error::Unexpected("empty STATUS? reply".into()))?;
        let v_meas = self.wire.query_number(cmd::VOUT)?;
        let i_meas = self.wire.query_number(cmd::IOUT)?;
        let v_set = self.wire.query_number(cmd::VSET)?;
        let i_set = self.wire.query_number(cmd::ISET)?;
        Ok(Status {
            output_on: flags & status::OUTPUT != 0,
            v_meas,
            i_meas,
            p_meas: v_meas * i_meas,
            v_set,
            i_set,
            ovp: None,
            ocp: None,
        })
    }

    /// Not read back: STATUS? bits are less consistent across firmware than the setpoint
    /// queries, and the next poll shows the real output state anyway.
    fn set_output(&mut self, on: bool) -> Result<()> {
        self.wire.send(if on { cmd::OUT_ON } else { cmd::OUT_OFF })
    }

    fn set_voltage(&mut self, volts: f64) -> Result<()> {
        check_range("voltage", volts, self.info.limits.voltage)?;
        let want = round_to(volts, VOLTAGE_DP);
        self.set_verified(cmd::set_voltage(want), cmd::VSET, want, VOLTAGE_DP)
    }

    fn set_current(&mut self, amps: f64) -> Result<()> {
        check_range("current", amps, self.info.limits.current)?;
        let want = round_to(amps, CURRENT_DP);
        self.set_verified(cmd::set_current(want), cmd::ISET, want, CURRENT_DP)
    }
}

/// Ports identified this run: port -> (baud, cleaned `*IDN?`).
static IDENTIFIED: Mutex<BTreeMap<String, (u32, String)>> = Mutex::new(BTreeMap::new());

fn identified() -> std::sync::MutexGuard<'static, BTreeMap<String, (u32, String)>> {
    IDENTIFIED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_serial(port: &str, baud: u32) -> Result<SerialTransport> {
    Ok(serial::open_with_timeout(port, baud, PORT_READ_TIMEOUT)?)
}

/// Opens and identifies a supply on `port`, trying each of [`BAUD_RATES`]. A port already
/// identified this run is reopened at its known baud and confirmed with `VSET1?` instead
/// of `*IDN?`. Read-only either way.
pub fn connect(port: &str) -> Result<Korad<SerialTransport>> {
    let timing = Timing::default();
    let cached = identified().get(port).cloned();
    if let Some((baud, idn)) = cached {
        if let Ok(k) = Korad::resume(open_serial(port, baud)?, &idn, timing) {
            return Ok(k);
        }
        identified().remove(port);
    }

    let mut err = Error::Timeout;
    for baud in BAUD_RATES {
        match Korad::identify(open_serial(port, baud)?, timing) {
            Ok(k) => {
                identified().insert(port.to_string(), (baud, k.idn.clone()));
                return Ok(k);
            }
            // Keep the most telling error: "answered, but not a Korad" beats a timeout.
            Err(e) if matches!(err, Error::Timeout) => err = e,
            Err(_) => {}
        }
    }
    Err(err)
}

fn open_port(port: &str) -> Result<Box<dyn Psu>> {
    Ok(Box::new(connect(port)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};

    /// Simulated Korad: answers the protocol from internal state and logs every command.
    struct FakeKorad {
        idn: &'static [u8],
        /// Answers left for `*IDN?` (the once-per-power-up firmware quirk).
        idn_answers: u32,
        vset: f64,
        iset: f64,
        output: bool,
        /// Drops set commands, like a garbled write.
        deaf: bool,
        log: Vec<String>,
        rx: VecDeque<u8>,
    }

    impl Default for FakeKorad {
        fn default() -> Self {
            Self {
                idn: b"KORAD KA3005P V5.8 SN:03379314",
                idn_answers: u32::MAX,
                vset: 5.0,
                iset: 1.0,
                output: false,
                deaf: false,
                log: Vec::new(),
                rx: VecDeque::new(),
            }
        }
    }

    impl FakeKorad {
        fn handle(&mut self, command: &str) {
            let reply: Vec<u8> = match command {
                "*IDN?" if self.idn_answers > 0 => {
                    self.idn_answers -= 1;
                    self.idn.to_vec()
                }
                "VSET1?" => format!("{:05.2}", self.vset).into(),
                // Trailing junk byte, as some firmware sends.
                "ISET1?" => format!("{:.3}K", self.iset).into(),
                "VOUT1?" => format!("{:05.2}", if self.output { self.vset } else { 0.0 }).into(),
                "IOUT1?" => b"0.250".to_vec(),
                "STATUS?" => vec![status::CH1_CV | if self.output { status::OUTPUT } else { 0 }],
                _ if self.deaf => vec![],
                "OUT1" => {
                    self.output = true;
                    vec![]
                }
                "OUT0" => {
                    self.output = false;
                    vec![]
                }
                c => {
                    if let Some(v) = c.strip_prefix("VSET1:") {
                        self.vset = v.parse().unwrap();
                    } else if let Some(i) = c.strip_prefix("ISET1:") {
                        self.iset = i.parse().unwrap();
                    }
                    vec![]
                }
            };
            self.rx.extend(reply);
        }
    }

    impl Read for FakeKorad {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.rx.len());
            for b in buf.iter_mut().take(n) {
                *b = self.rx.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for FakeKorad {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let command = String::from_utf8(buf.to_vec()).unwrap();
            self.log.push(command.clone());
            self.handle(&command);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for FakeKorad {}

    const FAST: Timing = Timing {
        cmd_interval: Duration::ZERO,
        reply_timeout: Duration::from_millis(20),
        idle_gap: Duration::from_millis(2),
    };

    fn connected() -> Korad<FakeKorad> {
        Korad::identify(FakeKorad::default(), FAST).unwrap()
    }

    #[test]
    fn recognizes_models() {
        let rated = |idn: &str| parse_model(idn).map(|m| (m.limits.voltage, m.limits.current));
        assert_eq!(rated("KORAD KA3005P V5.8"), Some((30.0, 5.0)));
        assert_eq!(rated("KORADKA3005PV2.0"), Some((30.0, 5.0)));
        assert_eq!(rated("KORAD KD6005P V2.0"), Some((60.0, 5.0)));
        assert_eq!(rated("KORAD KA3010P V5.5"), Some((30.0, 10.0)));
        assert_eq!(rated("RND 320-KD3005P V2.0"), Some((30.0, 5.0)));
        assert_eq!(rated("VELLEMANPS3005DV1.3"), Some((30.0, 5.0)));
        assert_eq!(rated("TENMA 72-2550 V2.0"), Some((60.0, 3.0)));
        assert_eq!(rated("S-LS-31 V2.0"), Some((30.0, 5.0)));
        // Multi-channel, LAN series, other vendors.
        assert_eq!(rated("KORAD KA3305P V5.0"), None);
        assert_eq!(rated("KORAD KWR102 V1.0"), None);
        assert_eq!(rated("RIGOL TECHNOLOGIES,DP832,DP8B0000,00.01.14"), None);
        assert_eq!(rated("FOO PS3005D"), None);
    }

    #[test]
    fn cleans_idn() {
        assert_eq!(clean_idn(b"KORAD KA3005P V5.8 SN:03379314"), "KORAD KA3005P V5.8");
        assert_eq!(clean_idn(b"KORADKA3005PV2.0\x01"), "KORADKA3005PV2.0");
        assert_eq!(clean_idn(b"KORAD KA3005P V4.2\xbc"), "KORAD KA3005P V4.2");
    }

    #[test]
    fn parses_value_replies() {
        assert_eq!(parse_number("VSET1?", b"12.00").unwrap(), 12.0);
        assert_eq!(parse_number("ISET1?", b"1.500K").unwrap(), 1.5);
        assert_eq!(parse_number("ISET1?", b"10.000").unwrap(), 10.0);
        assert!(parse_number("VOUT1?", b"").is_err());
        assert!(parse_number("VOUT1?", b"\x51").is_err());
    }

    #[test]
    fn identifies_and_reads_status() {
        let mut psu = connected();
        assert_eq!(psu.info().driver, "korad-kaxxxxp");
        assert_eq!(psu.info().model, "KORAD KA3005P V5.8 (30 V / 5 A)");
        assert_eq!(psu.info().limits, Limits { voltage: 30.0, current: 5.0, power: 150.0 });
        assert_eq!(psu.wire.port.log, ["*IDN?"]);

        psu.set_output(true).unwrap();
        let s = psu.status().unwrap();
        assert!(s.output_on);
        assert_eq!((s.v_set, s.i_set, s.v_meas, s.i_meas), (5.0, 1.0, 5.0, 0.25));
        assert_eq!(s.p_meas, 1.25);
        assert_eq!((s.ovp, s.ocp), (None, None));
    }

    #[test]
    fn rejects_other_devices() {
        let port = FakeKorad { idn: b"RIGOL TECHNOLOGIES,DP832", ..Default::default() };
        assert!(matches!(Korad::identify(port, FAST), Err(Error::NotRecognized(_))));
    }

    #[test]
    fn silent_port_times_out() {
        let port = FakeKorad { idn_answers: 0, ..Default::default() };
        assert!(matches!(Korad::identify(port, FAST), Err(Error::Timeout)));
    }

    #[test]
    fn resume_skips_idn() {
        // Firmware that already spent its one *IDN? answer.
        let port = FakeKorad { idn_answers: 0, ..Default::default() };
        let psu = Korad::resume(port, "KORAD KA3005P V5.8", FAST).unwrap();
        assert_eq!(psu.wire.port.log, ["VSET1?"]);
        assert_eq!(psu.info().limits.voltage, 30.0);
    }

    #[test]
    fn setpoints_are_formatted_and_verified() {
        let mut psu = connected();
        psu.wire.port.log.clear();
        psu.set_voltage(12.0).unwrap();
        psu.set_current(0.2999999).unwrap();
        assert_eq!(psu.wire.port.log, ["VSET1:12.00", "VSET1?", "ISET1:0.300", "ISET1?"]);
    }

    #[test]
    fn dropped_setpoint_is_reported() {
        let mut psu = connected();
        psu.wire.port.deaf = true;
        assert!(matches!(psu.set_voltage(12.0), Err(Error::Unexpected(_))));
    }

    #[test]
    fn apply_rejects_pair_without_writing() {
        let mut psu = connected();
        psu.wire.port.log.clear();
        assert!(psu.apply(31.0, 1.0).is_err());
        assert!(psu.apply(5.0, 5.1).is_err());
        assert!(psu.wire.port.log.is_empty());
    }

    #[test]
    fn raw_query_refuses_commands() {
        let mut psu = connected();
        assert!(psu.raw_query("OUT1").is_err());
        assert_eq!(psu.raw_query("VSET1?").unwrap(), b"05.00");
    }

    #[test]
    fn commands_are_paced() {
        let timing = Timing { cmd_interval: Duration::from_millis(30), ..FAST };
        let mut psu = Korad::identify(FakeKorad::default(), timing).unwrap();
        let start = Instant::now();
        psu.set_output(true).unwrap();
        psu.set_output(false).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(50));
    }
}
