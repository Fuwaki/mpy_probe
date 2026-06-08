mod connection;
mod device;
mod error;
mod include;
mod protocol;

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use connection::SerialConnection;
use device::{Device, DEFAULT_EXCLUDES};
use error::MpError;
use include::IncludeFilter;

/// Build the full excludes list: default excludes + user-provided extras.
fn build_excludes(user_excludes: &[String]) -> Vec<&str> {
    let mut excludes: Vec<&str> = DEFAULT_EXCLUDES.to_vec();
    for e in user_excludes {
        excludes.push(e.as_str());
    }
    excludes
}

/// Try to load a `.mpyinclude` file from the local directory.
/// If found and user provided `--exclude` flags, print a warning and ignore them.
/// Returns (excludes, include_filter).
fn resolve_filter(
    local_dir: &Path,
    user_excludes: &[String],
    json: bool,
) -> Result<(Vec<String>, Option<IncludeFilter>)> {
    let include = IncludeFilter::load(local_dir)?;

    if include.is_some() {
        // .mpyinclude found — warn if --exclude was also provided
        if !user_excludes.is_empty() && !json {
            eprintln!("Warning: .mpyinclude found, --exclude flags are ignored");
        }
        // Only use default excludes (ignore user --exclude)
        let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
        Ok((excludes, include))
    } else {
        let excludes = build_excludes(user_excludes);
        Ok((excludes.into_iter().map(|s| s.to_string()).collect(), None))
    }
}

#[derive(Parser)]
#[command(
    name = "mpy_probe",
    version,
    about = "MicroPython board interaction tool"
)]
struct Cli {
    /// Serial port path (auto-detected if omitted)
    #[arg(short, long)]
    port: Option<String>,

    /// Baud rate
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Timeout in seconds
    #[arg(short, long, default_value = "10")]
    timeout: u64,

    /// Output in JSON format (for IDE integration)
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open an interactive REPL
    Repl,

    /// Run a file on the device
    Run {
        /// Local file path to run
        file: String,
    },

    /// Upload a file to the device
    Put {
        /// Local file path
        local: String,
        /// Remote file path on device
        remote: String,
    },

    /// Download a file from the device
    Get {
        /// Remote file path on device
        remote: String,
        /// Local file path to save to
        local: String,
    },

    /// Execute Python code on the device
    Exec {
        /// Python code to execute
        code: String,
    },

    /// List files in a directory on the device
    Ls {
        /// Directory path (default: /)
        #[arg(default_value = "/")]
        path: String,
    },

    /// Remove a file on the device
    Rm {
        /// File path to remove
        path: String,
    },

    /// Create a directory on the device
    Mkdir {
        /// Directory path to create
        path: String,
    },

    /// Display file contents on the device
    Cat {
        /// Remote file path
        path: String,
    },

    /// Show device information (version, platform, memory, filesystem)
    Info,

    /// Soft-reset the device (CTRL-D)
    Reset,

    /// Interrupt running program (CTRL-C)
    Interrupt,

    /// Show file/directory status on device
    Stat {
        /// Remote file path
        path: String,
    },

    /// Upload a directory recursively to the device
    PutDir {
        /// Local directory path
        local: String,
        /// Remote directory path on device
        remote: String,
        /// Extra directory names to exclude (in addition to defaults: .git, __pycache__, etc.)
        #[arg(long)]
        exclude: Vec<String>,
    },

    /// Download a directory recursively from the device
    GetDir {
        /// Remote directory path on device
        remote: String,
        /// Local directory path to save to
        local: String,
    },

    /// Sync local directory to device (only transfer changed files)
    Sync {
        /// Local directory path
        local: String,
        /// Remote directory path on device
        remote: String,
        /// Show what would be synced without making changes
        #[arg(long)]
        dry_run: bool,
        /// Extra directory names to exclude (in addition to defaults: .git, __pycache__, etc.)
        #[arg(long)]
        exclude: Vec<String>,
    },

    /// Compare local and remote, show differences (supports single file or directory)
    Diff {
        /// Local file or directory path
        local: String,
        /// Remote file or directory path on device
        remote: String,
        /// Extra directory names to exclude (in addition to defaults: .git, __pycache__, etc.)
        #[arg(long)]
        exclude: Vec<String>,
    },
}

fn open_device(cli: &Cli) -> Result<Device<SerialConnection>> {
    let port_path = match &cli.port {
        Some(p) => p.clone(),
        None => {
            SerialConnection::detect_port().context("failed to auto-detect MicroPython device")?
        }
    };

    if !cli.json {
        eprintln!("Connecting to {} at {} baud...", port_path, cli.baud);
    }
    let timeout = Duration::from_secs(cli.timeout);
    let conn = SerialConnection::open_with_timeout(&port_path, cli.baud, timeout)
        .map_err(|e| anyhow::anyhow!(e))?;
    let device = Device::new_with_timeout(conn, timeout);
    if !cli.json {
        eprintln!("Connected.");
    }
    Ok(device)
}

fn print_output<T: serde::Serialize>(json: bool, value: &T) {
    if json {
        println!("{}", serde_json::to_string(value).unwrap());
    }
}

fn print_output_pretty<T: serde::Serialize>(json: bool, value: &T) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(&cli) {
        if cli.json {
            #[derive(serde::Serialize)]
            struct ErrorOutput {
                error: String,
            }
            eprintln!(
                "{}",
                serde_json::to_string(&ErrorOutput {
                    error: err.to_string(),
                })
                .unwrap()
            );
        } else {
            eprintln!("Error: {:#}", err);
        }
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Repl => {
            let mut device = open_device(cli)?;
            device.open_repl().map_err(|e| anyhow::anyhow!(e))?;
        }
        Command::Run { file } => {
            if !Path::new(file).exists() {
                return Err(MpError::InvalidInput(format!("file not found: {}", file)).into());
            }

            let mut device = open_device(cli)?;

            let remote_name = Path::new(file)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "__run.py".to_string());
            let remote_path = format!("/flash/{}", remote_name);

            if !cli.json {
                eprintln!("Uploading {} -> {}...", file, remote_path);
            }
            device
                .write_file(file, &remote_path)
                .map_err(|e| anyhow::anyhow!(e))?;

            if !cli.json {
                eprintln!("Running {}...", remote_path);
            }
            let result = device
                .run_file(&remote_path)
                .map_err(|e| anyhow::anyhow!(e))?;

            if cli.json {
                #[derive(serde::Serialize)]
                struct RunResult {
                    success: bool,
                    stdout: String,
                    stderr: String,
                }
                let r = RunResult {
                    success: !result.is_error(),
                    stdout: result.stdout_str(),
                    stderr: result.stderr_str(),
                };
                println!("{}", serde_json::to_string(&r).unwrap());
            } else {
                if !result.stdout.is_empty() {
                    io::stdout().write_all(&result.stdout)?;
                }
                if !result.stderr.is_empty() {
                    io::stderr().write_all(&result.stderr)?;
                }
            }

            let _ = device.remove(&remote_path);

            if result.is_error() {
                std::process::exit(1);
            }
        }
        Command::Put { local, remote } => {
            if !Path::new(local).exists() {
                return Err(MpError::InvalidInput(format!("file not found: {}", local)).into());
            }

            let mut device = open_device(cli)?;
            if !cli.json {
                eprintln!("Uploading {} -> {}...", local, remote);
            }
            device
                .write_file(local, remote)
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    local: String,
                    remote: String,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        local: local.clone(),
                        remote: remote.clone(),
                    },
                );
            } else {
                eprintln!("Done.");
            }
        }
        Command::Get { remote, local } => {
            let mut device = open_device(cli)?;
            if !cli.json {
                eprintln!("Downloading {} -> {}...", remote, local);
            }
            device
                .get_file(remote, local)
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    remote: String,
                    local: String,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        remote: remote.clone(),
                        local: local.clone(),
                    },
                );
            } else {
                eprintln!("Done.");
            }
        }
        Command::Exec { code } => {
            let mut device = open_device(cli)?;
            let result = device.exec(code).map_err(|e| anyhow::anyhow!(e))?;

            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    stdout: String,
                    stderr: String,
                }
                print_output(
                    true,
                    &R {
                        success: !result.is_error(),
                        stdout: result.stdout_str(),
                        stderr: result.stderr_str(),
                    },
                );
            } else {
                if !result.stdout.is_empty() {
                    io::stdout().write_all(&result.stdout)?;
                }
                if !result.stderr.is_empty() {
                    io::stderr().write_all(&result.stderr)?;
                }
            }

            if result.is_error() {
                std::process::exit(1);
            }
        }
        Command::Ls { path } => {
            let mut device = open_device(cli)?;
            let entries = device.list_dir(path).map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    path: String,
                    entries: Vec<String>,
                }
                print_output(
                    true,
                    &R {
                        path: path.clone(),
                        entries,
                    },
                );
            } else {
                for entry in &entries {
                    println!("{}", entry);
                }
            }
        }
        Command::Rm { path } => {
            let mut device = open_device(cli)?;
            device.remove(path).map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    path: String,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        path: path.clone(),
                    },
                );
            } else {
                eprintln!("Removed: {}", path);
            }
        }
        Command::Mkdir { path } => {
            let mut device = open_device(cli)?;
            device.mkdir(path).map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    path: String,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        path: path.clone(),
                    },
                );
            } else {
                eprintln!("Created: {}", path);
            }
        }
        Command::Cat { path } => {
            let mut device = open_device(cli)?;
            let content = device.cat(path).map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    path: String,
                    content: String,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        path: path.clone(),
                        content,
                    },
                );
            } else {
                print!("{}", content);
            }
        }
        Command::Info => {
            let mut device = open_device(cli)?;
            let info = device.device_info().map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                print_output_pretty(true, &info);
            } else {
                println!("version:  {}", info.version);
                println!("platform: {}", info.platform);
                println!("machine:  {}", info.machine);
                if let Some(mem) = info.mem_free {
                    println!("mem_free: {} bytes ({:.1} KB)", mem, mem as f64 / 1024.0);
                }
                if let Some(total) = info.fs_total {
                    println!(
                        "fs_total: {} bytes ({:.1} KB)",
                        total,
                        total as f64 / 1024.0
                    );
                }
                if let Some(free) = info.fs_free {
                    println!("fs_free:  {} bytes ({:.1} KB)", free, free as f64 / 1024.0);
                }
            }
        }
        Command::Reset => {
            let mut device = open_device(cli)?;
            device.soft_reset().map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                }
                print_output(true, &R { success: true });
            } else {
                eprintln!("Device reset.");
            }
        }
        Command::Interrupt => {
            let mut device = open_device(cli)?;
            device.interrupt().map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                }
                print_output(true, &R { success: true });
            } else {
                eprintln!("Interrupt sent.");
            }
        }
        Command::Stat { path } => {
            let mut device = open_device(cli)?;
            let stat = device.stat(path).map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                print_output_pretty(true, &stat);
            } else {
                println!("path:  {}", stat.path);
                println!("type:  {}", if stat.is_dir { "directory" } else { "file" });
                println!("size:  {} bytes", stat.size);
                if let Some(mtime) = stat.mtime {
                    println!("mtime: {}", mtime);
                }
            }
        }
        Command::PutDir {
            local,
            remote,
            exclude,
        } => {
            let local_path = Path::new(local);
            if !local_path.exists() || !local_path.is_dir() {
                return Err(
                    MpError::InvalidInput(format!("directory not found: {}", local)).into(),
                );
            }
            let (excludes, include) = resolve_filter(local_path, exclude, cli.json)?;
            let exclude_refs: Vec<&str> = excludes.iter().map(|s| s.as_str()).collect();
            let mut device = open_device(cli)?;
            let count = device
                .put_dir(local, remote, &exclude_refs, include.as_ref(), local_path)
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    local: String,
                    remote: String,
                    count: usize,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        local: local.clone(),
                        remote: remote.clone(),
                        count,
                    },
                );
            } else {
                eprintln!("Uploaded {} files.", count);
            }
        }
        Command::GetDir { remote, local } => {
            let mut device = open_device(cli)?;
            let count = device
                .get_dir(remote, local)
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    remote: String,
                    local: String,
                    count: usize,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        remote: remote.clone(),
                        local: local.clone(),
                        count,
                    },
                );
            } else {
                eprintln!("Downloaded {} files.", count);
            }
        }
        Command::Sync {
            local,
            remote,
            dry_run,
            exclude,
        } => {
            let local_path = Path::new(local);
            if !local_path.exists() || !local_path.is_dir() {
                return Err(
                    MpError::InvalidInput(format!("directory not found: {}", local)).into(),
                );
            }
            let (excludes, include) = resolve_filter(local_path, exclude, cli.json)?;
            let exclude_refs: Vec<&str> = excludes.iter().map(|s| s.as_str()).collect();
            let mut device = open_device(cli)?;
            let stats = device
                .sync(local, remote, *dry_run, &exclude_refs, include.as_ref())
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                #[derive(serde::Serialize)]
                struct R {
                    success: bool,
                    dry_run: bool,
                    uploaded: usize,
                    downloaded: usize,
                    deleted: usize,
                    actions: Vec<device::SyncAction>,
                }
                print_output(
                    true,
                    &R {
                        success: true,
                        dry_run: *dry_run,
                        uploaded: stats.uploaded,
                        downloaded: stats.downloaded,
                        deleted: stats.deleted,
                        actions: stats.actions,
                    },
                );
            } else {
                for action in &stats.actions {
                    match action.action.as_str() {
                        "upload" => {
                            if *dry_run {
                                eprintln!(
                                    "  + {} ({} bytes, would upload)",
                                    action.path, action.size
                                );
                            } else {
                                eprintln!("  + {} ({} bytes)", action.path, action.size);
                            }
                        }
                        "delete" => {
                            if *dry_run {
                                eprintln!("  - {} (would delete)", action.path);
                            } else {
                                eprintln!("  - {}", action.path);
                            }
                        }
                        _ => {}
                    }
                }
                if *dry_run {
                    eprintln!(
                        "Dry run — {} would upload, {} would delete",
                        stats.uploaded, stats.deleted
                    );
                } else {
                    eprintln!(
                        "Sync complete: {} uploaded, {} deleted",
                        stats.uploaded, stats.deleted
                    );
                }
            }
        }
        Command::Diff {
            local,
            remote,
            exclude,
        } => {
            let local_path = Path::new(local);
            if !local_path.exists() {
                return Err(MpError::InvalidInput(format!("path not found: {}", local)).into());
            }
            let (excludes, include) = resolve_filter(local_path, exclude, cli.json)?;
            let exclude_refs: Vec<&str> = excludes.iter().map(|s| s.as_str()).collect();
            let mut device = open_device(cli)?;
            let diff = device
                .diff(local, remote, &exclude_refs, include.as_ref())
                .map_err(|e| anyhow::anyhow!(e))?;
            if cli.json {
                print_output_pretty(true, &diff);
            } else if diff.entries.is_empty() {
                eprintln!("No differences — local and remote are in sync.");
            } else {
                for entry in &diff.entries {
                    match entry.status.as_str() {
                        "new" => println!(
                            "+ {} (local: {} bytes)",
                            entry.path,
                            entry.local_size.unwrap_or(0)
                        ),
                        "changed" => println!(
                            "~ {} (local: {} → remote: {} bytes)",
                            entry.path,
                            entry.local_size.unwrap_or(0),
                            entry.remote_size.unwrap_or(0)
                        ),
                        "deleted" => println!(
                            "- {} (remote: {} bytes)",
                            entry.path,
                            entry.remote_size.unwrap_or(0)
                        ),
                        _ => {}
                    }
                }
                eprintln!(
                    "\n{} new, {} changed, {} deleted",
                    diff.new_count, diff.changed_count, diff.deleted_count
                );
            }
        }
    }

    Ok(())
}
