use std::time::Duration;

use crate::connection::Connection;
use crate::error::{MpError, Result};

// Control characters
const CTRL_A: u8 = 0x01;
const CTRL_B: u8 = 0x02;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;

// Prompts
const RAW_REPL_BANNER: &[u8] = b"raw REPL; CTRL-B to exit\r\n>";
const OK_RESP: &[u8] = b"OK";

// Raw-paste negotiation
const RAW_PASTE_CMD: &[u8] = &[CTRL_E, b'A', CTRL_A];
const RAW_PASTE_SUPPORTED: &[u8] = b"R\x01";
const RAW_PASTE_REJECTED: &[u8] = b"R\x00";

// Defaults
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK_SIZE: usize = 256;
const CHUNK_DELAY: Duration = Duration::from_millis(2);
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(30);

/// The output of executing code on the device.
#[derive(Debug, Default)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecResult {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }

    pub fn is_error(&self) -> bool {
        !self.stderr.is_empty()
    }
}

/// Which submit mode to use for sending code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitMode {
    /// Flow-controlled raw-paste (fastest, preferred for serial)
    RawPaste,
    /// Paste mode via CTRL-E (good for WebREPL)
    #[allow(dead_code)]
    Paste,
    /// Slow 256-byte chunked raw mode (fallback)
    Raw,
}

/// REPL session managing the state machine over a Connection.
pub struct ReplSession<C: Connection> {
    conn: C,
    use_raw_paste: bool,
    submit_mode: SubmitMode,
    in_raw_repl: bool,
}

impl<C: Connection> ReplSession<C> {
    pub fn new(conn: C) -> Self {
        Self {
            conn,
            use_raw_paste: true,
            submit_mode: SubmitMode::RawPaste,
            in_raw_repl: false,
        }
    }

    pub fn conn(&mut self) -> &mut C {
        &mut self.conn
    }

    #[allow(dead_code)]
    pub fn in_raw_repl(&self) -> bool {
        self.in_raw_repl
    }

    /// Enter raw REPL mode. If `soft_reset` is true, sends CTRL-D for a reboot.
    pub fn enter_raw_repl(&mut self, soft_reset: bool) -> Result<()> {
        // Robust recovery: interrupt any running program, drain output,
        // and ensure we're in a clean state before entering raw REPL.
        // This handles the case where a previous transfer failed and left
        // the device in an unknown state with a full serial buffer.

        // Phase 1: Actively drain all pending output first.
        for _ in 0..20 {
            let data = self.conn.read_all_available()?;
            if data.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Phase 2: Send CTRL-C interrupts to stop any running code.
        for _ in 0..3 {
            let _ = self.conn.write_all(&[CTRL_C]);
            std::thread::sleep(Duration::from_millis(20));
        }

        // Phase 3: Exit raw REPL if we were in it (CTRL-B).
        let _ = self.conn.write_all(&[CTRL_B]);
        std::thread::sleep(Duration::from_millis(50));

        // Phase 4: Drain until the device goes quiet.
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(20));
            let data = self.conn.read_all_available()?;
            if data.is_empty() {
                break;
            }
        }

        // Phase 5: Now enter raw REPL.
        self.conn.write_all(b"\r")?;
        self.conn.write_all(&[CTRL_A])?;

        // Wait for raw REPL banner
        self.conn.read_until(RAW_REPL_BANNER, DEFAULT_TIMEOUT)?;

        if soft_reset {
            // Trigger soft reset
            self.conn.write_all(&[CTRL_D])?;
            // Wait for soft reboot message
            self.conn.read_until(b"soft reboot\r\n", DEFAULT_TIMEOUT)?;
            // Wait for banner again (device comes back to raw REPL)
            self.conn.read_until(RAW_REPL_BANNER, DEFAULT_TIMEOUT)?;
        }

        self.in_raw_repl = true;
        Ok(())
    }

    /// Exit raw REPL mode, returning to normal REPL.
    pub fn exit_raw_repl(&mut self) -> Result<()> {
        self.conn.write_all(b"\r")?;
        self.conn.write_all(&[CTRL_B])?;
        self.in_raw_repl = false;
        Ok(())
    }

    /// Send CTRL-C to interrupt the running program.
    /// Does not enter/exit raw REPL.
    pub fn send_ctrl_c(&mut self) -> Result<()> {
        self.conn.write_all(&[CTRL_C, CTRL_C])?;
        Ok(())
    }

    /// Send CTRL-D (soft reboot / EOT).
    /// Must be called while in raw REPL.
    pub fn send_ctrl_d(&mut self) -> Result<()> {
        self.conn.write_all(&[CTRL_D])?;
        Ok(())
    }

    /// Send code bytes. Must be called while already in raw REPL.
    ///
    /// Tries raw-paste first (if enabled), falls back to raw mode.
    pub fn submit_code(&mut self, code: &[u8]) -> Result<()> {
        match self.submit_mode {
            SubmitMode::RawPaste if self.use_raw_paste => {
                match self.try_submit_raw_paste(code) {
                    Ok(()) => return Ok(()),
                    Err(_) => {
                        // Raw-paste failed. The device's output buffer may be
                        // in an unknown state. Re-enter raw REPL to get a
                        // clean slate, then use raw mode.
                        self.use_raw_paste = false;
                        self.submit_mode = SubmitMode::Raw;
                        self.exit_raw_repl()?;
                        self.enter_raw_repl(false)?;
                        // Fall through to raw mode
                    }
                }
            }
            SubmitMode::Paste => return self.submit_paste(code),
            _ => {}
        }
        self.submit_raw(code)
    }

    /// Attempt raw-paste negotiation and code transfer.
    ///
    /// Must be called while in raw REPL mode. If negotiation fails,
    /// the device is still at the raw prompt and we can fall back.
    fn try_submit_raw_paste(&mut self, code: &[u8]) -> Result<()> {
        // Send raw-paste negotiation: CTRL-E, 'A', CTRL-A
        self.conn.write_all(RAW_PASTE_CMD)?;

        // Read 2-byte response
        let resp = self.conn.read_exact(2, Duration::from_secs(3))?;

        if resp == RAW_PASTE_REJECTED {
            // Device understood but doesn't support raw-paste.
            return Err(MpError::Protocol("device rejected raw-paste".into()));
        }

        if resp != RAW_PASTE_SUPPORTED {
            // Device doesn't understand raw-paste at all.
            // The negotiation bytes were echoed/processed, and the device
            // may have sent error output. Consume until we see the raw
            // prompt ">" to get back to a clean state.
            let _ = self.conn.read_until(b">", Duration::from_secs(3));
            return Err(MpError::Protocol("device does not support raw-paste".into()));
        }

        // Device supports raw-paste! Read window size (2 bytes, LE)
        let win_bytes = self.conn.read_exact(2, DEFAULT_TIMEOUT)?;
        let window_size = u16::from_le_bytes([win_bytes[0], win_bytes[1]]) as usize;

        // Some devices send an extra byte after the window size.
        // Consume it so it doesn't corrupt the CTRL-D ack read later.
        if !self.conn.incoming_is_empty() {
            let _ = self.conn.read_exact(1, Duration::from_millis(100));
        }

        // Flow-controlled transmission
        let mut offset = 0;
        let mut window_remain = window_size;

        while offset < code.len() {
            if window_remain == 0 || !self.conn.incoming_is_empty() {
                let ctrl = self.conn.read_exact(1, DEFAULT_TIMEOUT)?;
                match ctrl[0] {
                    CTRL_A => {
                        // Grant: refill window
                        window_remain += window_size;
                    }
                    CTRL_D => {
                        // Device wants to abort
                        self.conn.write_all(&[CTRL_D])?;
                        return Err(MpError::Protocol(
                            "device aborted raw-paste transfer".into(),
                        ));
                    }
                    other => {
                        return Err(MpError::Protocol(format!(
                            "unexpected byte during raw-paste: 0x{:02x}",
                            other
                        )));
                    }
                }
            }

            let end = (offset + window_remain).min(code.len());
            self.conn.write_all(&code[offset..end])?;
            let sent = end - offset;
            offset += sent;
            window_remain -= sent;
        }

        // Signal end of data
        self.conn.write_all(&[CTRL_D])?;

        // Wait for device acknowledgment (single CTRL-D)
        let ack = self.conn.read_exact(1, DEFAULT_TIMEOUT)?;
        if ack[0] != CTRL_D {
            return Err(MpError::Protocol(format!(
                "expected CTRL-D ack after raw-paste, got 0x{:02x}",
                ack[0]
            )));
        }

        Ok(())
    }

    /// Raw mode: 256-byte chunks with 10ms delays (slow fallback).
    fn submit_raw(&mut self, code: &[u8]) -> Result<()> {
        for chunk in code.chunks(CHUNK_SIZE) {
            self.conn.write_all(chunk)?;
            std::thread::sleep(CHUNK_DELAY);
        }
        self.conn.write_all(&[CTRL_D])?;

        // Expect "OK"
        let resp = self.conn.read_exact(2, DEFAULT_TIMEOUT)?;
        if resp != OK_RESP {
            return Err(MpError::Protocol(format!(
                "expected OK after raw submit, got {:?}",
                String::from_utf8_lossy(&resp)
            )));
        }

        Ok(())
    }

    /// Paste mode: CTRL-E based sending.
    fn submit_paste(&mut self, code: &[u8]) -> Result<()> {
        // Enter paste mode
        self.conn.write_all(&[CTRL_E])?;
        // Wait for paste prompt
        self.conn.read_until(b"=== ", DEFAULT_TIMEOUT)?;

        // Send code in chunks
        for chunk in code.chunks(CHUNK_SIZE) {
            self.conn.write_all(chunk)?;
            std::thread::sleep(CHUNK_DELAY);
        }

        // End paste
        self.conn.write_all(&[CTRL_D])?;
        // Wait for execution to complete
        self.conn.read_until(b"\r\n", DEFAULT_TIMEOUT)?;

        Ok(())
    }

    /// Read execution output until two CTRL-D markers (stdout / stderr split).
    ///
    /// First CTRL-D ends stdout, second CTRL-D ends stderr.
    pub fn follow(&mut self, timeout: Duration) -> Result<ExecResult> {
        let deadline = std::time::Instant::now() + timeout;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut seen_first_ctrl_d = false;

        loop {
            if std::time::Instant::now() >= deadline {
                return Err(MpError::Timeout(
                    "timeout waiting for execution output".into(),
                ));
            }

            match self.conn.read_exact(1, Duration::from_millis(100)) {
                Ok(data) => {
                    let byte = data[0];
                    if byte == CTRL_D {
                        if !seen_first_ctrl_d {
                            seen_first_ctrl_d = true;
                        } else {
                            return Ok(ExecResult { stdout, stderr });
                        }
                    } else if seen_first_ctrl_d {
                        stderr.push(byte);
                    } else {
                        stdout.push(byte);
                    }
                }
                Err(MpError::Timeout(_)) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Execute code and collect output. Enters/exits raw REPL automatically.
    pub fn exec_raw(&mut self, code: &[u8]) -> Result<ExecResult> {
        self.enter_raw_repl(false)?;
        self.submit_code(code)?;
        let result = self.follow(FOLLOW_TIMEOUT)?;
        self.exit_raw_repl()?;
        Ok(result)
    }

    /// Execute multiple commands in a single raw REPL session.
    /// Much faster than calling `exec_raw` repeatedly, since we avoid
    /// the overhead of entering/exiting raw mode for each command.
    ///
    /// Uses raw-paste exclusively for flow control. If raw-paste is not
    /// supported, returns an error instead of falling back (which would
    /// corrupt the batch session by re-entering raw REPL).
    pub fn exec_raw_batch(&mut self, codes: &[&[u8]]) -> Result<Vec<ExecResult>> {
        self.enter_raw_repl(false)?;
        let mut results = Vec::with_capacity(codes.len());
        for code in codes.iter() {
            // Always use raw-paste for batch — raw mode has no flow control
            // and will overflow the device buffer. If raw-paste fails,
            // the batch cannot continue safely.
            self.try_submit_raw_paste(code)?;
            results.push(self.follow(FOLLOW_TIMEOUT)?);
            // Consume the trailing raw prompt '>' so the next submit_code
            // starts with a clean buffer.
            let _ = self.conn.read_exact(1, Duration::from_millis(200));
        }
        self.exit_raw_repl()?;
        Ok(results)
    }
}
