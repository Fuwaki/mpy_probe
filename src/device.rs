use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::connection::Connection;
use crate::error::{MpError, Result};
use crate::include::IncludeFilter;
use crate::protocol::{ExecResult, ReplSession};

/// Default directory names to exclude from local file collection.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "__pycache__",
    ".DS_Store",
    "node_modules",
    ".vscode",
    ".idea",
];

/// Device information returned by `device_info()`.
#[derive(Debug, Default, serde::Serialize)]
pub struct DeviceInfo {
    pub version: String,
    pub platform: String,
    pub machine: String,
    pub mem_free: Option<u64>,
    pub fs_total: Option<u64>,
    pub fs_free: Option<u64>,
}

/// File/directory status on the device.
#[derive(Debug, serde::Serialize)]
pub struct FileStat {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
}

/// Result of a sync operation.
#[derive(Debug, Default, serde::Serialize)]
pub struct SyncStats {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted: usize,
    /// Per-file actions taken (or would-be-taken in dry-run mode).
    pub actions: Vec<SyncAction>,
}

/// A single file action performed (or proposed) during sync.
#[derive(Debug, serde::Serialize)]
pub struct SyncAction {
    /// Relative path of the file.
    pub path: String,
    /// What happened: "upload", "delete".
    pub action: String,
    /// File size in bytes (for uploads: local size; for deletes: remote size).
    pub size: u64,
}

/// A single file difference between local and remote.
#[derive(Debug, serde::Serialize)]
pub struct DiffEntry {
    pub path: String,
    pub status: String, // "new", "changed", "deleted"
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
}

/// Result of a diff operation.
#[derive(Debug, serde::Serialize)]
pub struct DiffResult {
    pub entries: Vec<DiffEntry>,
    pub new_count: usize,
    pub changed_count: usize,
    pub deleted_count: usize,
}

/// High-level device operations built on top of the REPL session.
pub struct Device<C: Connection> {
    session: ReplSession<C>,
    chunk_size: usize,
}

impl<C: Connection> Device<C> {
    pub fn new_with_timeout(conn: C, timeout: Duration) -> Self {
        Self {
            session: ReplSession::with_timeout(conn, timeout),
            chunk_size: 128,
        }
    }

    /// Execute Python code on the device and return stdout/stderr.
    pub fn exec(&mut self, code: &str) -> Result<ExecResult> {
        self.session.exec_raw(code.as_bytes())
    }

    /// Evaluate a Python expression and return the repr of the result.
    #[allow(dead_code)]
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
        let path = py_str_repr(path);
        let code = format!(
            "exec(open({path}).read(), {{'__name__':'__main__','__file__':{path}}})",
        );
        self.exec(&code)
    }

    /// Upload a local file to the device.
    pub fn write_file(&mut self, local_path: &str, remote_path: &str) -> Result<()> {
        let data = std::fs::read(local_path)
            .map_err(|e| MpError::Filesystem(format!("failed to read {}: {}", local_path, e)))?;

        self.write_file_bytes(remote_path, &data)
    }

    /// Upload raw bytes to a file on the device.
    ///
    /// Splits the data into batches of chunks and uses `exec_raw_batch`
    /// to send them in a single raw REPL session with raw-paste flow control.
    /// This avoids overflowing the device's input buffer for large files.
    pub fn write_file_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let remote_path = py_str_repr(remote_path);

        // Each batch writes batch_chunks worth of data (e.g. 128 * 16 = 2048 bytes).
        // The resulting Python script stays small enough for the device to handle.
        let batch_chunks = 16;
        let chunks: Vec<&[u8]> = data.chunks(self.chunk_size).collect();

        if chunks.is_empty() {
            // Empty file: just create it
            let code = format!("__f=open({},'wb')\n__f.flush()\n__f.close()\n", remote_path);
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
                code.push_str(&format!("__f=open({},'wb')\n", remote_path));
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
        let remote_path = py_str_repr(remote_path);
        let chunk_size = self.chunk_size;

        // Single exec: open, read all chunks as hex lines, close.
        let code = format!(
            "import binascii\n\
             __f=open({},'rb')\n\
             while True:\n\
             \x20 __d=__f.read({})\n\
             \x20 if not __d:break\n\
             \x20 print(binascii.hexlify(__d).decode())\n\
             __f.close()",
            remote_path, chunk_size
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
            if line.is_empty() {
                continue;
            }
            let chunk = parse_hex_bytes(line)?;
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }

    /// Download a file from the device and write to local path.
    pub fn get_file(&mut self, remote_path: &str, local_path: &str) -> Result<()> {
        let data = self.read_file(remote_path)?;
        std::fs::write(local_path, &data)
            .map_err(|e| MpError::Filesystem(format!("failed to write {}: {}", local_path, e)))?;
        Ok(())
    }

    /// Read a text file and return its content as a string.
    pub fn cat(&mut self, remote_path: &str) -> Result<String> {
        let data = self.read_file(remote_path)?;
        Ok(String::from_utf8_lossy(&data).to_string())
    }

    /// List files in a directory on the device.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let path = py_str_repr(path);
        let code = format!(
            "import os,binascii\nfor __n in os.listdir({}):\n print(binascii.hexlify(__n.encode()).decode())",
            path
        );
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }
        result
            .stdout_str()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| parse_hex_string(line.trim()))
            .collect()
    }

    /// Remove a file on the device.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let code = format!("import os; os.remove({})", py_str_repr(path));
        let result = self.exec(&code)?;
        if result.is_error() {
            let stderr = result.stderr_str();
            if stderr.contains("ENOENT") || stderr.contains("Errno 2") {
                return Err(MpError::Filesystem(format!("not found: {}", path)));
            }
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr,
            });
        }
        Ok(())
    }

    /// Create a directory on the device.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        // Create directory and all parents, ignore if already exists.
        let code = format!(
            "import os\np={}\nfor i in range(1,len(p.split('/'))+1):\n d='/'.join(p.split('/')[:i]) or '/'\n try: os.mkdir(d)\n except: pass",
            py_str_repr(path)
        );
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
        use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use crossterm::terminal;

        // Enter raw REPL without soft reset so we keep running state
        self.session.enter_raw_repl(false)?;
        self.session.exit_raw_repl()?;

        // Set up cross-platform raw terminal
        terminal::enable_raw_mode().map_err(|e| MpError::Connection(format!("raw mode: {}", e)))?;

        let conn = self.session.conn();
        let mut stdout = io::stdout();

        let result = (|| -> Result<()> {
            loop {
                // Read data from device -> write to stdout
                match conn.read_all_available() {
                    Ok(data) if !data.is_empty() => {
                        stdout
                            .write_all(&data)
                            .map_err(|e| MpError::Connection(e.to_string()))?;
                        stdout
                            .flush()
                            .map_err(|e| MpError::Connection(e.to_string()))?;
                    }
                    _ => {}
                }

                // Check for stdin key events (non-blocking)
                if event::poll(Duration::from_millis(0))
                    .map_err(|e| MpError::Connection(format!("poll: {}", e)))?
                {
                    if let Event::Key(KeyEvent {
                        code,
                        modifiers,
                        kind: KeyEventKind::Press,
                        ..
                    }) =
                        event::read().map_err(|e| MpError::Connection(format!("read: {}", e)))?
                    {
                        match code {
                            // Ctrl-D exits REPL
                            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                                break
                            }
                            // Ctrl-<char> -> control character (0x01..0x1a)
                            KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) => {
                                let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a' - 1);
                                if (1..=26).contains(&byte) {
                                    conn.write_all(&[byte])?;
                                }
                            }
                            KeyCode::Char(c) => {
                                conn.write_all(&[c as u8])?;
                            }
                            KeyCode::Enter => {
                                conn.write_all(b"\r")?;
                            }
                            KeyCode::Backspace => {
                                conn.write_all(&[0x08])?;
                            }
                            KeyCode::Tab => {
                                conn.write_all(&[0x09])?;
                            }
                            KeyCode::Esc => {
                                conn.write_all(&[0x1b])?;
                            }
                            // Arrow keys -> VT100 escape sequences
                            KeyCode::Up => conn.write_all(b"\x1b[A")?,
                            KeyCode::Down => conn.write_all(b"\x1b[B")?,
                            KeyCode::Right => conn.write_all(b"\x1b[C")?,
                            KeyCode::Left => conn.write_all(b"\x1b[D")?,
                            KeyCode::Home => conn.write_all(b"\x1b[H")?,
                            KeyCode::End => conn.write_all(b"\x1b[F")?,
                            KeyCode::Delete => conn.write_all(b"\x1b[3~")?,
                            KeyCode::PageUp => conn.write_all(b"\x1b[5~")?,
                            KeyCode::PageDown => conn.write_all(b"\x1b[6~")?,
                            _ => {}
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        })();

        // Always restore terminal
        let _ = terminal::disable_raw_mode();
        result
    }

    /// Soft-reset the device (CTRL-D).
    pub fn soft_reset(&mut self) -> Result<()> {
        self.session.enter_raw_repl(false)?;
        self.session.send_ctrl_d()?;
        self.session.exit_raw_repl()?;
        Ok(())
    }

    /// Get device information: version, platform, board, memory, filesystem.
    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        let code = "import sys, os, gc\ngc.collect()\nprint('version:' + sys.version)\nprint('platform:' + sys.platform)\ntry:\n m=str(sys.implementation._machine)\nexcept:\n m=str(sys.implementation)\nprint('machine:' + m)\nprint('mem_free:' + str(gc.mem_free()))\ntry:\n s=os.statvfs('/flash')\n print('fs_total:' + str(s[0]*s[2]))\n print('fs_free:' + str(s[0]*s[4]))\nexcept:\n try:\n  s=os.statvfs('/')\n  print('fs_total:' + str(s[0]*s[2]))\n  print('fs_free:' + str(s[0]*s[4]))\n except:\n  pass\n";
        let result = self.exec(code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }

        let mut info = DeviceInfo::default();
        for line in result.stdout_str().lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once(':') {
                match key {
                    "version" => info.version = val.to_string(),
                    "platform" => info.platform = val.to_string(),
                    "machine" => info.machine = val.to_string(),
                    "mem_free" => info.mem_free = val.parse().ok(),
                    "fs_total" => info.fs_total = val.parse().ok(),
                    "fs_free" => info.fs_free = val.parse().ok(),
                    _ => {}
                }
            }
        }

        Ok(info)
    }

    /// Send interrupt (CTRL-C) to the device without entering raw REPL.
    pub fn interrupt(&mut self) -> Result<()> {
        self.session.send_ctrl_c()
    }

    /// Get file/directory status on the device.
    pub fn stat(&mut self, path: &str) -> Result<FileStat> {
        let code = format!(
            "import os\n\
             s=os.stat({})\n\
             print('mode:' + str(s[0]))\n\
             print('size:' + str(s[6]))\n\
             print('mtime:' + str(s[8]))",
            py_str_repr(path)
        );
        let result = self.exec(&code)?;
        if result.is_error() {
            return Err(MpError::Execution {
                stdout: result.stdout_str(),
                stderr: result.stderr_str(),
            });
        }

        let mut mode: u32 = 0;
        let mut size: u64 = 0;
        let mut mtime: Option<u64> = None;

        for line in result.stdout_str().lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once(':') {
                match key {
                    "mode" => mode = val.parse().unwrap_or(0),
                    "size" => size = val.parse().unwrap_or(0),
                    "mtime" => {
                        let v: u64 = val.parse().unwrap_or(0);
                        if v > 0 {
                            mtime = Some(v);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(FileStat {
            path: path.to_string(),
            is_dir: (mode & 0o170000) == 0o040000,
            size,
            mtime,
        })
    }

    /// Upload a local directory recursively to the device.
    /// Returns the number of files uploaded.
    pub fn put_dir(
        &mut self,
        local_dir: &str,
        remote_dir: &str,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
        base_dir: &Path,
    ) -> Result<usize> {
        let local_path = Path::new(local_dir);
        let mut count = 0;

        // Ensure remote directory exists
        let _ = self.mkdir(remote_dir);

        for entry in fs::read_dir(local_path)
            .map_err(|e| MpError::Filesystem(format!("failed to read {}: {}", local_dir, e)))?
        {
            let entry = entry.map_err(|e| MpError::Filesystem(e.to_string()))?;
            let file_name = entry.file_name().to_string_lossy().to_string();

            // Skip excluded directory/file names
            if excludes.contains(&file_name.as_str()) {
                continue;
            }

            let local_file = entry.path();
            let remote_file = format!("{}/{}", remote_dir.trim_end_matches('/'), file_name);

            if local_file.is_dir() {
                // Always recurse into directories — a matching file might be deeper
                count += self.put_dir(
                    &local_file.to_string_lossy(),
                    &remote_file,
                    excludes,
                    include,
                    base_dir,
                )?;
            } else {
                // Check include filter if active
                if let Some(filter) = include {
                    let rel = local_file
                        .strip_prefix(base_dir)
                        .unwrap_or(&local_file)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !filter.is_match(&rel) {
                        continue;
                    }
                }
                self.write_file(&local_file.to_string_lossy(), &remote_file)?;
                count += 1;
            }
        }

        Ok(count)
    }

    /// Download a remote directory recursively from the device.
    /// Returns the number of files downloaded.
    pub fn get_dir(&mut self, remote_dir: &str, local_dir: &str) -> Result<usize> {
        let local_path = Path::new(local_dir);
        fs::create_dir_all(local_path)
            .map_err(|e| MpError::Filesystem(format!("failed to create {}: {}", local_dir, e)))?;

        let entries = self.list_dir(remote_dir)?;
        let mut count = 0;

        for name in &entries {
            let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
            let local_file = local_path.join(name);

            match self.stat(&remote_path) {
                Ok(s) if s.is_dir => {
                    count += self.get_dir(&remote_path, &local_file.to_string_lossy())?;
                }
                _ => {
                    self.get_file(&remote_path, &local_file.to_string_lossy())?;
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Sync local directory to device. Uploads new/changed files, deletes
    /// remote files that don't exist locally. If dry_run is true, only report
    /// what would be done without making changes.
    pub fn sync(
        &mut self,
        local_dir: &str,
        remote_dir: &str,
        dry_run: bool,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
    ) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        let local_path = Path::new(local_dir);

        // Build set of local files
        let local_files = self.collect_local_files(local_path, "", excludes, include)?;

        // Build set of remote files
        let remote_files = self.collect_remote_files(remote_dir, excludes, include)?;

        // Upload new/changed files
        for (rel_path, local_file) in &local_files {
            let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), rel_path);

            let needs_upload = match remote_files.get(rel_path) {
                None => true,
                Some(remote_stat) => {
                    let local_meta =
                        fs::metadata(local_file).map_err(|e| MpError::Filesystem(e.to_string()))?;
                    local_meta.len() != remote_stat.size
                }
            };

            if needs_upload {
                let local_meta =
                    fs::metadata(local_file).map_err(|e| MpError::Filesystem(e.to_string()))?;
                if !dry_run {
                    // Ensure parent directory exists
                    if let Some(parent) = Path::new(&remote_path).parent() {
                        let _ = self.mkdir(&parent.to_string_lossy());
                    }
                    self.write_file(local_file, &remote_path)?;
                }
                stats.actions.push(SyncAction {
                    path: rel_path.clone(),
                    action: "upload".to_string(),
                    size: local_meta.len(),
                });
                stats.uploaded += 1;
            }
        }

        // Delete remote files not present locally
        for (rel_path, remote_stat) in &remote_files {
            if !local_files.contains_key(rel_path) {
                if !dry_run {
                    let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), rel_path);
                    self.remove(&remote_path)?;
                }
                stats.actions.push(SyncAction {
                    path: rel_path.clone(),
                    action: "delete".to_string(),
                    size: remote_stat.size,
                });
                stats.deleted += 1;
            }
        }

        Ok(stats)
    }

    /// Compare local and remote, returning a list of differences.
    /// Supports both single files and directories.
    pub fn diff(
        &mut self,
        local_path: &str,
        remote_path: &str,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
    ) -> Result<DiffResult> {
        let local = Path::new(local_path);

        // Single file diff
        if local.is_file() {
            let file_name = local
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let remote_file = if remote_path.ends_with('/') {
                format!("{}{}", remote_path, file_name)
            } else {
                remote_path.to_string()
            };

            let local_meta = fs::metadata(local).map_err(|e| MpError::Filesystem(e.to_string()))?;
            let local_size = local_meta.len();

            let mut entries = Vec::new();
            match self.stat(&remote_file) {
                Ok(remote_stat) => {
                    if local_size != remote_stat.size {
                        entries.push(DiffEntry {
                            path: file_name,
                            status: "changed".to_string(),
                            local_size: Some(local_size),
                            remote_size: Some(remote_stat.size),
                        });
                        return Ok(DiffResult {
                            entries,
                            new_count: 0,
                            changed_count: 1,
                            deleted_count: 0,
                        });
                    }
                }
                Err(_) => {
                    entries.push(DiffEntry {
                        path: file_name,
                        status: "new".to_string(),
                        local_size: Some(local_size),
                        remote_size: None,
                    });
                    return Ok(DiffResult {
                        entries,
                        new_count: 1,
                        changed_count: 0,
                        deleted_count: 0,
                    });
                }
            }
            return Ok(DiffResult {
                entries,
                new_count: 0,
                changed_count: 0,
                deleted_count: 0,
            });
        }

        // Directory diff
        let local_files = self.collect_local_files(local, "", excludes, include)?;
        let remote_files = self.collect_remote_files(remote_path, excludes, include)?;

        let mut entries = Vec::new();
        let mut new_count = 0;
        let mut changed_count = 0;
        let mut deleted_count = 0;

        // Check local files against remote
        for (rel_path, local_file) in &local_files {
            let local_meta =
                fs::metadata(local_file).map_err(|e| MpError::Filesystem(e.to_string()))?;
            let local_size = local_meta.len();

            match remote_files.get(rel_path) {
                None => {
                    entries.push(DiffEntry {
                        path: rel_path.clone(),
                        status: "new".to_string(),
                        local_size: Some(local_size),
                        remote_size: None,
                    });
                    new_count += 1;
                }
                Some(remote_stat) => {
                    if local_size != remote_stat.size {
                        entries.push(DiffEntry {
                            path: rel_path.clone(),
                            status: "changed".to_string(),
                            local_size: Some(local_size),
                            remote_size: Some(remote_stat.size),
                        });
                        changed_count += 1;
                    }
                }
            }
        }

        // Check for remote-only files (deleted locally)
        for (rel_path, remote_stat) in &remote_files {
            if !local_files.contains_key(rel_path) {
                entries.push(DiffEntry {
                    path: rel_path.clone(),
                    status: "deleted".to_string(),
                    local_size: None,
                    remote_size: Some(remote_stat.size),
                });
                deleted_count += 1;
            }
        }

        // Sort by path for consistent output
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(DiffResult {
            entries,
            new_count,
            changed_count,
            deleted_count,
        })
    }

    /// Recursively collect local files as (relative_path, absolute_path).
    fn collect_local_files(
        &self,
        base: &Path,
        rel: &str,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut map = std::collections::HashMap::new();
        let dir = if rel.is_empty() {
            base.to_path_buf()
        } else {
            base.join(rel)
        };

        for entry in fs::read_dir(&dir)
            .map_err(|e| MpError::Filesystem(format!("failed to read {}: {}", dir.display(), e)))?
        {
            let entry = entry.map_err(|e| MpError::Filesystem(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip excluded directory/file names
            if excludes.contains(&name.as_str()) {
                continue;
            }

            let rel_path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel, name)
            };

            if entry.path().is_dir() {
                // Always recurse — a matching file might be deeper
                map.extend(self.collect_local_files(base, &rel_path, excludes, include)?);
            } else {
                // Check include filter if active
                if let Some(filter) = include {
                    if !filter.is_match(&rel_path) {
                        continue;
                    }
                }
                map.insert(rel_path, entry.path().to_string_lossy().to_string());
            }
        }

        Ok(map)
    }

    /// Recursively collect remote files as (relative_path, FileStat).
    fn collect_remote_files(
        &mut self,
        remote_dir: &str,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
    ) -> Result<std::collections::HashMap<String, FileStat>> {
        self.collect_remote_files_inner(remote_dir, remote_dir, excludes, include)
    }

    fn collect_remote_files_inner(
        &mut self,
        root_dir: &str,
        remote_dir: &str,
        excludes: &[&str],
        include: Option<&IncludeFilter>,
    ) -> Result<std::collections::HashMap<String, FileStat>> {
        let mut map = std::collections::HashMap::new();
        let entries = match self.list_dir(remote_dir) {
            Ok(e) => e,
            Err(_) => return Ok(map),
        };

        for name in &entries {
            if excludes.contains(&name.as_str()) {
                continue;
            }
            let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
            match self.stat(&remote_path) {
                Ok(s) if s.is_dir => {
                    // Always recurse — a matching file might be deeper
                    map.extend(self.collect_remote_files_inner(
                        root_dir,
                        &remote_path,
                        excludes,
                        include,
                    )?);
                }
                Ok(s) => {
                    let rel = remote_path
                        .strip_prefix(&format!("{}/", root_dir.trim_end_matches('/')))
                        .unwrap_or(name)
                        .to_string();
                    // Check include filter if active
                    if let Some(filter) = include {
                        if !filter.is_match(&rel) {
                            continue;
                        }
                    }
                    map.insert(rel, s);
                }
                Err(_) => {}
            }
        }

        Ok(map)
    }
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err(MpError::Protocol(format!("invalid hex byte string: {}", s)));
    }

    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16)
            .map_err(|e| MpError::Protocol(format!("invalid hex byte string '{}': {}", s, e)))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn parse_hex_string(s: &str) -> Result<String> {
    let bytes = parse_hex_bytes(s)?;
    String::from_utf8(bytes)
        .map_err(|e| MpError::Protocol(format!("invalid UTF-8 filename from device: {}", e)))
}

fn py_str_repr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Generate a Python bytes literal from raw bytes.
/// Produces valid Python `b'...'` syntax that can be eval'd.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_str_repr_escapes_python_string_boundaries() {
        assert_eq!(py_str_repr("/flash/a'b\\c.py"), "'/flash/a\\'b\\\\c.py'");
        assert_eq!(py_str_repr("line\nnext"), "'line\\nnext'");
    }

    #[test]
    fn parse_hex_bytes_round_trips_binary_data() {
        assert_eq!(
            parse_hex_bytes("000a0d5cfff0").unwrap(),
            vec![0x00, b'\n', b'\r', b'\\', 0xff, 0xf0]
        );
    }

    #[test]
    fn parse_hex_string_handles_punctuation_in_filenames() {
        assert_eq!(
            parse_hex_string("61272c625c632e7079").unwrap(),
            "a',b\\c.py"
        );
    }

    #[test]
    fn parse_hex_bytes_rejects_invalid_input() {
        assert!(parse_hex_bytes("abc").is_err());
        assert!(parse_hex_bytes("xx").is_err());
    }
}
