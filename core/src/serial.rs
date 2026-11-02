//! Serial-port transport shared by drivers (USB-serial bridges, RS-232 adapters).

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use serialport::{ClearBuffer, DataBits, Parity, SerialPort, StopBits};

use crate::modbus::Transport;

/// Also bounds how long an autodetect probe waits on a silent port. A full Modbus reply
/// is ~25 ms on the wire at 9600 baud.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

const OPEN_SETTLE: Duration = Duration::from_millis(100);

pub struct SerialTransport(Box<dyn SerialPort>);

/// Opens `path` as 8N1 at `baud`.
pub fn open(path: &str, baud: u32) -> serialport::Result<SerialTransport> {
    let port = serialport::new(path, baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .timeout(READ_TIMEOUT)
        .open()?;
    // The CH340 bridge drops a request sent immediately after the port opens.
    thread::sleep(OPEN_SETTLE);
    Ok(SerialTransport(port))
}

pub fn list_ports() -> serialport::Result<Vec<serialport::SerialPortInfo>> {
    serialport::available_ports()
}

impl Read for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for SerialTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Transport for SerialTransport {
    fn discard_input(&mut self) -> io::Result<()> {
        self.0.clear(ClearBuffer::Input).map_err(io::Error::other)
    }
}
