//! Minimal Modbus RTU master: framing, CRC, and the function codes the HM-series speaks.
//!
//! The OEM sheet only promises FC 0x03 (read holding registers) and FC 0x06 (write single
//! register), so that is all we implement. Multi-word values (power, OPP) are read with 0x03.

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use thiserror::Error;

pub const FC_READ_HOLDING: u8 = 0x03;
pub const FC_WRITE_SINGLE: u8 = 0x06;

/// Set on the function code of an exception response.
const EXCEPTION_FLAG: u8 = 0x80;

/// Bus must be silent >= 3.5 char times (~3.6 ms at 9600 baud) between frames; the OEM
/// sheet asks for "longer than 5 ms", so we pad a little.
const INTER_FRAME_DELAY: Duration = Duration::from_millis(6);

#[derive(Debug, Error)]
pub enum ModbusError {
    #[error("serial I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("no response from device (timeout)")]
    Timeout,
    #[error("CRC mismatch: computed {computed:#06x}, received {received:#06x}")]
    Crc { computed: u16, received: u16 },
    #[error("device rejected function {function:#04x} with exception code {code:#04x}")]
    Exception { function: u8, code: u8 },
    #[error("unexpected response: {0}")]
    Unexpected(String),
}

pub type Result<T> = std::result::Result<T, ModbusError>;

/// Byte stream the master talks over. `discard_input` drops stale bytes (e.g. a late reply
/// to a timed-out request) so they can't be mistaken for the next response.
pub trait Transport: Read + Write {
    fn discard_input(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// CRC-16/MODBUS (poly 0xA001 reflected, init 0xFFFF). Sent low byte first.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

/// Builds `[slave, pdu..., crc_lo, crc_hi]`.
pub fn build_frame(slave: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(pdu.len() + 3);
    frame.push(slave);
    frame.extend_from_slice(pdu);
    frame.extend_from_slice(&crc16(&frame).to_le_bytes());
    frame
}

pub struct RtuMaster<T> {
    port: T,
    slave: u8,
}

impl<T: Transport> RtuMaster<T> {
    pub fn new(port: T, slave: u8) -> Self {
        Self { port, slave }
    }

    #[cfg(test)]
    pub(crate) fn port_mut(&mut self) -> &mut T {
        &mut self.port
    }

    /// FC 0x03: read `count` consecutive 16-bit registers starting at `start`.
    pub fn read_holding(&mut self, start: u16, count: u16) -> Result<Vec<u16>> {
        let [sh, sl] = start.to_be_bytes();
        let [ch, cl] = count.to_be_bytes();
        let body = self.transact(&[FC_READ_HOLDING, sh, sl, ch, cl])?;

        // body = [byte_count, data...]
        let byte_count = usize::from(body[0]);
        if byte_count != usize::from(count) * 2 {
            return Err(ModbusError::Unexpected(format!(
                "asked for {count} registers, got {byte_count} bytes"
            )));
        }
        Ok(body[1..]
            .chunks_exact(2)
            .map(|w| u16::from_be_bytes([w[0], w[1]]))
            .collect())
    }

    /// FC 0x06: write one register. The device echoes the request on success.
    pub fn write_single(&mut self, register: u16, value: u16) -> Result<()> {
        let [rh, rl] = register.to_be_bytes();
        let [vh, vl] = value.to_be_bytes();
        let request = [FC_WRITE_SINGLE, rh, rl, vh, vl];
        let body = self.transact(&request)?;
        if body != request[1..] {
            return Err(ModbusError::Unexpected(format!(
                "write echo mismatch: sent {:02X?}, got {body:02X?}",
                &request[1..]
            )));
        }
        Ok(())
    }

    /// Sends one request PDU and returns the validated response body (everything between
    /// the function code and the CRC).
    fn transact(&mut self, pdu: &[u8]) -> Result<Vec<u8>> {
        let function = pdu[0];
        thread::sleep(INTER_FRAME_DELAY);
        self.port.discard_input()?;
        self.port.write_all(&build_frame(self.slave, pdu))?;
        self.port.flush()?;

        let mut frame = self.read_n(2)?;
        let (slave, fc) = (frame[0], frame[1]);
        if slave != self.slave {
            return Err(ModbusError::Unexpected(format!(
                "reply from slave {slave}, expected {}",
                self.slave
            )));
        }

        let body_len = if fc == function | EXCEPTION_FLAG {
            1
        } else if fc != function {
            return Err(ModbusError::Unexpected(format!(
                "reply function {fc:#04x}, expected {function:#04x}"
            )));
        } else if function == FC_READ_HOLDING {
            let count = self.read_n(1)?;
            frame.extend_from_slice(&count);
            usize::from(count[0])
        } else {
            4 // FC 0x06 echo: register + value
        };

        frame.extend(self.read_n(body_len + 2)?);
        let (payload, crc) = frame.split_at(frame.len() - 2);
        let received = u16::from_le_bytes([crc[0], crc[1]]);
        let computed = crc16(payload);
        if received != computed {
            return Err(ModbusError::Crc { computed, received });
        }

        if fc & EXCEPTION_FLAG != 0 {
            return Err(ModbusError::Exception { function, code: payload[2] });
        }
        Ok(payload[2..].to_vec())
    }

    fn read_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        match self.port.read_exact(&mut buf) {
            Ok(()) => Ok(buf),
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::UnexpectedEof) => {
                Err(ModbusError::Timeout)
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted transport: records what was written, replays canned reply bytes.
    #[derive(Default)]
    pub struct MockPort {
        pub written: Vec<u8>,
        pub replies: VecDeque<u8>,
    }

    impl MockPort {
        pub fn reply(&mut self, slave: u8, pdu: &[u8]) {
            self.replies.extend(build_frame(slave, pdu));
        }
    }

    impl Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.replies.len());
            for b in buf.iter_mut().take(n) {
                *b = self.replies.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for MockPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for MockPort {}

    #[test]
    fn crc_matches_known_hm_frame() {
        // Captured request "read 4 regs from 0x0010": 01 03 00 10 00 04 45 CC
        assert_eq!(
            build_frame(1, &[0x03, 0x00, 0x10, 0x00, 0x04]),
            [0x01, 0x03, 0x00, 0x10, 0x00, 0x04, 0x45, 0xCC]
        );
    }

    #[test]
    fn read_holding_decodes_big_endian_words() {
        let mut port = MockPort::default();
        port.reply(1, &[0x03, 0x04, 0x0B, 0xB8, 0x01, 0xF4]);
        let mut m = RtuMaster::new(port, 1);
        assert_eq!(m.read_holding(0x0010, 2).unwrap(), vec![3000, 500]);
    }

    #[test]
    fn exception_reply_is_reported() {
        let mut port = MockPort::default();
        port.reply(1, &[0x86, 0x02]);
        let mut m = RtuMaster::new(port, 1);
        assert!(matches!(
            m.write_single(0x0030, 1),
            Err(ModbusError::Exception { function: 0x06, code: 0x02 })
        ));
    }

    #[test]
    fn corrupted_reply_fails_crc() {
        let mut port = MockPort::default();
        port.reply(1, &[0x06, 0x00, 0x30, 0x01, 0xF4]);
        port.replies[4] ^= 0xFF;
        let mut m = RtuMaster::new(port, 1);
        assert!(matches!(m.write_single(0x0030, 500), Err(ModbusError::Crc { .. })));
    }

    #[test]
    fn silent_device_times_out() {
        let mut m = RtuMaster::new(MockPort::default(), 1);
        assert!(matches!(m.read_holding(0x0001, 1), Err(ModbusError::Timeout)));
    }
}
