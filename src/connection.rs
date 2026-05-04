use std::time::{Duration, Instant};

use crate::error::{MpError, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Abstraction over a byte-stream connection to a MicroPython device.
///
/// Implementors include serial port and (future) WebREPL.
#[allow(dead_code)]
pub trait Connection {
    /// Read exactly `n` bytes, or fail on timeout.
    fn read_exact(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>>;

    /// Read until `pattern` is found in the accumulated buffer.
    /// Returns all bytes consumed (including the pattern).
    fn read_until(&mut self, pattern: &[u8], timeout: Duration) -> Result<Vec<u8>>;

    /// Read whatever is available right now (non-blocking after first byte).
    fn read_all_available(&mut self) -> Result<Vec<u8>>;

    /// Push bytes back so the next read returns them first.
    fn unread(&mut self, data: &[u8]);

    /// Write all bytes.
    fn write_all(&mut self, data: &[u8]) -> Result<()>;

    /// Check if there's nothing waiting to be read.
    fn incoming_is_empty(&self) -> bool;

    /// Drain all pending input bytes (including prepend buffer).
    fn flush_input(&mut self);
}

/// Serial port connection using the `serialport` crate.
pub struct SerialConnection {
    port: Box<dyn serialport::SerialPort>,
    prepend_buf: Vec<u8>,
}

impl SerialConnection {
    pub fn open(port_path: &str, baud: u32) -> Result<Self> {
        let port = serialport::new(port_path, baud)
            .timeout(Duration::from_secs(10))
            .open()
            .map_err(|e| MpError::Connection(format!("failed to open {}: {}", port_path, e)))?;

        Ok(Self {
            port,
            prepend_buf: Vec::new(),
        })
    }

    /// Attempt to detect a MicroPython device on common serial paths.
    /// Returns the first matching port path.
    pub fn detect_port() -> Result<String> {
        let ports = serialport::available_ports()
            .map_err(|e| MpError::Connection(format!("failed to list ports: {}", e)))?;

        let candidates: Vec<_> = ports
            .iter()
            .filter(|p| {
                let name = &p.port_name;
                name.contains("ttyACM") || name.contains("ttyUSB")
            })
            .collect();

        match candidates.len() {
            0 => Err(MpError::Connection(
                "no MicroPython device found (no ttyACM/ttyUSB ports)".into(),
            )),
            1 => Ok(candidates[0].port_name.clone()),
            _ => {
                let names: Vec<&str> = candidates.iter().map(|p| p.port_name.as_str()).collect();
                Err(MpError::Connection(format!(
                    "multiple serial ports found: {} — use --port to specify",
                    names.join(", ")
                )))
            }
        }
    }
}

impl Connection for SerialConnection {
    fn read_exact(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(n);

        // Drain prepend buffer first
        let from_prepend = n.min(self.prepend_buf.len());
        if from_prepend > 0 {
            result.extend_from_slice(&self.prepend_buf[..from_prepend]);
            self.prepend_buf.drain(..from_prepend);
        }

        let deadline = Instant::now() + timeout;
        while result.len() < n {
            if Instant::now() >= deadline {
                return Err(MpError::Timeout(format!(
                    "read_exact: wanted {} bytes, got {}",
                    n,
                    result.len()
                )));
            }

            let mut buf = [0u8; 256];
            match self.port.read(&mut buf) {
                Ok(0) => std::thread::sleep(POLL_INTERVAL),
                Ok(count) => {
                    let need = n - result.len();
                    let take = count.min(need);
                    result.extend_from_slice(&buf[..take]);
                    if take < count {
                        self.prepend_buf.extend_from_slice(&buf[take..count]);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(POLL_INTERVAL)
                }
                Err(e) => return Err(MpError::Connection(format!("read error: {}", e))),
            }
        }

        Ok(result)
    }

    fn read_until(&mut self, pattern: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let mut result = Vec::new();

        // Start with prepend buffer
        if !self.prepend_buf.is_empty() {
            result.extend_from_slice(&self.prepend_buf);
            self.prepend_buf.clear();
        }

        let deadline = Instant::now() + timeout;
        loop {
            if result.len() >= pattern.len() && result.ends_with(pattern) {
                return Ok(result);
            }

            if Instant::now() >= deadline {
                return Err(MpError::Timeout(format!(
                    "read_until: pattern {:?} not found within timeout",
                    pattern
                )));
            }

            let mut buf = [0u8; 1];
            match self.port.read(&mut buf) {
                Ok(0) => std::thread::sleep(POLL_INTERVAL),
                Ok(_) => result.push(buf[0]),
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(POLL_INTERVAL)
                }
                Err(e) => return Err(MpError::Connection(format!("read error: {}", e))),
            }
        }
    }

    fn read_all_available(&mut self) -> Result<Vec<u8>> {
        let mut result = Vec::new();

        if !self.prepend_buf.is_empty() {
            result.extend_from_slice(&self.prepend_buf);
            self.prepend_buf.clear();
        }

        // Temporarily set a very short timeout so read() doesn't block
        // when the buffer is empty. This is critical for drain/recovery
        // loops that call read_all_available repeatedly.
        let orig_timeout = self.port.timeout();
        self.port.set_timeout(Duration::from_millis(10)).ok();

        let mut buf = [0u8; 256];
        loop {
            match self.port.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => result.extend_from_slice(&buf[..n]),
            }
        }

        self.port.set_timeout(orig_timeout).ok();
        Ok(result)
    }

    fn unread(&mut self, data: &[u8]) {
        // Prepend so that data is read back first
        let mut new = data.to_vec();
        new.extend_from_slice(&self.prepend_buf);
        self.prepend_buf = new;
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.port
            .write_all(data)
            .map_err(|e| MpError::Connection(format!("write error: {}", e)))
    }

    fn incoming_is_empty(&self) -> bool {
        if !self.prepend_buf.is_empty() {
            return false;
        }
        self.port.bytes_to_read().unwrap_or(0) == 0
    }

    fn flush_input(&mut self) {
        self.prepend_buf.clear();
        let orig_timeout = self.port.timeout();
        self.port.set_timeout(Duration::from_millis(10)).ok();
        let mut buf = [0u8; 256];
        loop {
            match self.port.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        self.port.set_timeout(orig_timeout).ok();
    }
}
