use std::io::{self, Read, Write};
use std::time::Duration;

use crate::connection::Connection;
use crate::error::{MpError, Result};
use crate::protocol::{ExecResult, ReplSession};

/// High-level device operations built on top of the REPL session.
pub struct Device<C: Connection> {
    session: ReplSession<C>,
    chunk_size: usize,
}

impl<C: Connection> Device<C> {
    pub fn new(conn: C) -> Self {
        Self {
            session: ReplSession::new(conn),
            chunk_size: 128,
        }
    }

    /// Execute Python code on the device and return stdout/stderr.
    pub fn exec(&mut self, code: &str) -> Result<ExecResult> {
        self.session.exec_raw(code.as_bytes())
    }

    /// Evaluate a Python expression and return the repr of the result.
    pub fn eval(&mut self, expr: &str) -> Result<String> {
        let code = format!("print(repr({}))", expr);
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }
        Ok(result.stdout_str().trim_end_matches("\r\n").to_string())
    }

    /// Run a file on the device (execfile-style).
    pub fn run_file(&mut self, path: &str) -> Result<ExecResult> {
        let code = format!(
            "import sys\ntry:\n    exec(open('{p}').read(), {{'__name__':'__main__','__file__':'{p}'}})\nexcept Exception as e:\n    sys.print_exception(e)",
            p = path
        );
        self.exec(&code)
    }

    /// Upload a local file to the device.
    pub fn write_file(&mut self, local_path: &str, remote_path: &str) -> Result<()> {
        let data = std::fs::read(local_path).map_err(|e| {
            MpError::Filesystem(format!("failed to read {}: {}", local_path, e))
        })?;

        self.write_file_bytes(remote_path, &data)
    }

    /// Upload raw bytes to a file on the device.
    ///
    /// Splits the data into batches of chunks and uses `exec_raw_batch`
    /// to send them in a single raw REPL session with raw-paste flow control.
    /// This avoids overflowing the device's input buffer for large files.
    pub fn write_file_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let escaped_path = remote_path.replace('\'', "\\'");

        // Each batch writes batch_chunks worth of data (e.g. 128 * 16 = 2048 bytes).
        // The resulting Python script stays small enough for the device to handle.
        let batch_chunks = 16;
        let chunks: Vec<&[u8]> = data.chunks(self.chunk_size).collect();

        if chunks.is_empty() {
            // Empty file: just create it
            let code = format!(
                "__f=open('{}','wb')\n__f.flush()\n__f.close()\n",
                escaped_path
            );
            let result = self.exec(&code)?;
            if result.is_error() {
                return Err(MpError::Execution {
                    stdout: result.stdout_str(),
                    stderr: result.stderr_str(),
                });
            }
            return Ok(());
        }

        // Build batched Python scripts
        let mut scripts: Vec<String> = Vec::new();
        for (i, batch) in chunks.chunks(batch_chunks).enumerate() {
            let mut code = String::new();
            if i == 0 {
                code.push_str(&format!("__f=open('{}','wb')\n", escaped_path));
            }
            for chunk in batch {
                let hex: String = chunk.iter().map(|b| format!("{:02x}", b)).collect();
                code.push_str(&format!("__f.write(bytes.fromhex('{}'))\n", hex));
            }
            // Only close on the last batch
            if (i + 1) * batch_chunks >= chunks.len() {
                code.push_str("__f.flush()\n__f.close()\n");
            }
            scripts.push(code);
        }

        let code_refs: Vec<&[u8]> = scripts.iter().map(|s| s.as_bytes()).collect();
        let results = self.session.exec_raw_batch(&code_refs)?;

        // Check for errors in any batch
        for result in results.iter() {
            if result.is_error() {
                return Err(MpError::Execution {
                    stdout: result.stdout_str(),
                    stderr: result.stderr_str(),
                });
            }
        }

        Ok(())
    }

    /// Download a file from the device.
    ///
    /// Uses a single exec call with a Python loop to read all chunks,
    /// separated by a unique delimiter for reliable parsing.
    pub fn read_file(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let escaped_path = remote_path.replace('\'', "\\'");
        let chunk_size = self.chunk_size;

        // Single exec: open, read all chunks with delimiter, close
        let code = format!(
            "__f=open('{}','rb')\n\
             while True:\n\
             \x20 __d=__f.read({})\n\
             \x20 if not __d:break\n\
             \x20 print(repr(__d))\n\
             __f.close()",
            escaped_path, chunk_size
        );

        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }

        let output = result.stdout_str();
        let mut bytes = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() || line == "b''" {
                continue;
            }
            let chunk = parse_py_bytes_literal(line)?;
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }

    /// Download a file from the device and write to local path.
    pub fn get_file(&mut self, remote_path: &str, local_path: &str) -> Result<()> {
        let data = self.read_file(remote_path)?;
        std::fs::write(local_path, &data).map_err(|e| {
            MpError::Filesystem(format!("failed to write {}: {}", local_path, e))
        })?;
        Ok(())
    }

    /// List files in a directory on the device.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let code = format!("import os; print(repr(os.listdir('{}')))", path.replace('\'', "\\'"));
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }
        let repr = result.stdout_str().trim_end_matches("\r\n").to_string();
        parse_py_list_of_strings(&repr)
    }

    /// Remove a file on the device.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let code = format!("import os; os.remove('{}')", path.replace('\'', "\\'"));
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }
        Ok(())
    }

    /// Create a directory on the device.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let code = format!("import os; os.mkdir('{}')", path.replace('\'', "\\'"));
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }
        Ok(())
    }

    /// Open an interactive REPL — pipes stdin/stdout bidirectionally.
    /// Blocks until the user exits (Ctrl-C / Ctrl-D / "exit()").
    pub fn open_repl(&mut self) -> Result<()> {
        use std::os::fd::{AsRawFd, BorrowedFd};

        // Enter raw REPL without soft reset so we keep running state
        self.session.enter_raw_repl(false)?;
        self.session.exit_raw_repl()?;

        // Now we're in normal REPL. Set up raw terminal for interactive use.
        let stdin = io::stdin();
        let stdout = io::stdout();

        // Put terminal in raw mode
        let orig_termios = set_raw_mode(stdin.as_raw_fd())?;

        let conn = self.session.conn();

        // REPL loop: read from stdin, write to device; read from device, write to stdout
        let mut stdout = stdout.lock();
        let mut stdin_buf = [0u8; 1];

        loop {
            // Check for data from device
            match conn.read_all_available() {
                Ok(data) if !data.is_empty() => {
                    stdout.write_all(&data).map_err(|e| MpError::Connection(e.to_string()))?;
                    stdout.flush().map_err(|e| MpError::Connection(e.to_string()))?;
                }
                _ => {}
            }

            // Check for stdin input
            use nix::poll::{poll, PollFd, PollFlags};

            let stdin_fd = unsafe { BorrowedFd::borrow_raw(stdin.as_raw_fd()) };
            let mut poll_fds = [PollFd::new(stdin_fd, PollFlags::POLLIN)];

            let poll_result = poll(&mut poll_fds, 0u16).map_err(|e| MpError::Connection(format!("poll error: {}", e)))?;
            if poll_result > 0 {
                match io::stdin().read(&mut stdin_buf) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // Ctrl-D detection
                        if stdin_buf[0] == 0x04 {
                            break;
                        }
                        conn.write_all(&stdin_buf)?;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(MpError::Connection(format!("stdin read error: {}", e))),
                }
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        // Restore terminal
        restore_termios(stdin.as_raw_fd(), orig_termios)?;

        Ok(())
    }

    /// Soft-reset the device.
    pub fn soft_reset(&mut self) -> Result<()> {
        self.exec("import machine; machine.reset()")?;
        Ok(())
    }
}

/// Parse a Python bytes literal like `b'\\x01\\x02hello'` into raw bytes.
fn parse_py_bytes_literal(s: &str) -> Result<Vec<u8>> {
    // Expecting b'...' or b"..."
    let s = s.trim();
    if !s.starts_with("b'") && !s.starts_with("b\"") {
        // Might be a text string, just return as bytes
        return Ok(s.as_bytes().to_vec());
    }

    let inner = &s[2..s.len() - 1];
    let mut result = Vec::new();
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('x') => {
                    let h1 = chars.next().unwrap_or('0');
                    let h2 = chars.next().unwrap_or('0');
                    let hex_str = format!("{}{}", h1, h2);
                    let byte = u8::from_str_radix(&hex_str, 16).unwrap_or(0);
                    result.push(byte);
                }
                Some('n') => result.push(b'\n'),
                Some('r') => result.push(b'\r'),
                Some('t') => result.push(b'\t'),
                Some('\\') => result.push(b'\\'),
                Some('\'') => result.push(b'\''),
                Some('"') => result.push(b'"'),
                Some('0') => result.push(0),
                Some(other) => {
                    result.push(b'\\');
                    result.push(other as u8);
                }
                None => result.push(b'\\'),
            }
        } else {
            result.push(c as u8);
        }
    }

    Ok(result)
}

/// Parse a Python list of strings like `['file1.py', 'file2.py']`.
fn parse_py_list_of_strings(s: &str) -> Result<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Err(MpError::Protocol(format!(
            "expected Python list literal, got: {}",
            s
        )));
    }

    let inner = &s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    // Simple parser: split by ', ' and strip quotes
    // Handle the case where strings might contain commas (rare for filenames)
    let mut current = String::new();
    let mut in_string = false;
    let mut quote_char = '\0';

    for c in inner.chars() {
        if !in_string {
            if c == '\'' || c == '"' {
                in_string = true;
                quote_char = c;
            } else if c == ',' {
                let trimmed = current.trim().trim_matches(|c| c == '\'' || c == '"');
                if !trimmed.is_empty() {
                    result.push(trimmed.to_string());
                }
                current.clear();
            } else if !c.is_whitespace() {
                current.push(c);
            }
        } else {
            if c == quote_char {
                in_string = false;
            } else {
                current.push(c);
            }
        }
    }

    // Last element
    if !current.is_empty() {
        let trimmed = current.trim().trim_matches(|c| c == '\'' || c == '"');
        if !trimmed.is_empty() {
            result.push(trimmed.to_string());
        }
    }

    Ok(result)
}

/// Generate a Python bytes literal from raw bytes.
/// Produces valid Python `b'...'` syntax that can be eval'd.
fn py_bytes_repr(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() + 10);
    out.push_str("b'");
    for &byte in data {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\x{:02x}", byte)),
        }
    }
    out.push('\'');
    out
}

// Terminal raw mode helpers (Unix only)

#[cfg(unix)]
fn set_raw_mode(raw_fd: std::os::fd::RawFd) -> Result<nix::sys::termios::Termios> {
    use std::os::fd::BorrowedFd;
    use nix::sys::termios;

    let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let orig = termios::tcgetattr(fd).map_err(|e| MpError::Connection(format!("tcgetattr: {}", e)))?;
    let mut raw = orig.clone();
    raw.input_flags.remove(
        termios::InputFlags::ICRNL | termios::InputFlags::IXON,
    );
    raw.local_flags.remove(
        termios::LocalFlags::ECHO
            | termios::LocalFlags::ICANON
            | termios::LocalFlags::ISIG
            | termios::LocalFlags::IEXTEN,
    );
    raw.output_flags.remove(termios::OutputFlags::OPOST);
    raw.control_chars[termios::SpecialCharacterIndices::VMIN as usize] = 0;
    raw.control_chars[termios::SpecialCharacterIndices::VTIME as usize] = 0;
    termios::tcsetattr(fd, termios::SetArg::TCSAFLUSH, &raw)
        .map_err(|e| MpError::Connection(format!("tcsetattr: {}", e)))?;
    Ok(orig)
}

#[cfg(unix)]
fn restore_termios(raw_fd: std::os::fd::RawFd, orig: nix::sys::termios::Termios) -> Result<()> {
    use std::os::fd::BorrowedFd;
    use nix::sys::termios;

    let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    termios::tcsetattr(fd, termios::SetArg::TCSAFLUSH, &orig)
        .map_err(|e| MpError::Connection(format!("tcsetattr restore: {}", e)))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_raw_mode(_raw_fd: std::os::fd::RawFd) -> Result<()> {
    Err(MpError::Connection(
        "raw terminal mode not supported on this platform".into(),
    ))
}

#[cfg(not(unix))]
fn restore_termios(_raw_fd: std::os::fd::RawFd, _: ()) -> Result<()> {
    Ok(())
}
