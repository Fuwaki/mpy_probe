mod connection;
mod device;
mod error;
mod protocol;

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use connection::SerialConnection;
use device::Device;
use error::MpError;

#[derive(Parser)]
#[command(name = "mpy_probe", version, about = "MicroPython board interaction tool")]
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
}

fn open_device(cli: &Cli) -> Result<Device<SerialConnection>> {
    let port_path = match &cli.port {
        Some(p) => p.clone(),
        None => SerialConnection::detect_port()
            .context("failed to auto-detect MicroPython device")?,
    };

    eprintln!("Connecting to {} at {} baud...", port_path, cli.baud);
    let conn = SerialConnection::open(&port_path, cli.baud)
        .map_err(|e| anyhow::anyhow!(e))?;
    let device = Device::new(conn);
    eprintln!("Connected.");
    Ok(device)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Repl => {
            let mut device = open_device(&cli)?;
            device.open_repl().map_err(|e| anyhow::anyhow!(e))?;
        }
        Command::Run { file } => {
            if !Path::new(file).exists() {
                return Err(MpError::InvalidInput(format!("file not found: {}", file)).into());
            }

            let mut device = open_device(&cli)?;

            // Upload the file first, then run it
            let remote_name = Path::new(file)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "__run.py".to_string());
            let remote_path = format!("/flash/{}", remote_name);

            eprintln!("Uploading {} -> {}...", file, remote_path);
            device.write_file(file, &remote_path).map_err(|e| anyhow::anyhow!(e))?;

            eprintln!("Running {}...", remote_path);
            let result = device.run_file(&remote_path).map_err(|e| anyhow::anyhow!(e))?;

            // Stream output to stdout/stderr
            if !result.stdout.is_empty() {
                io::stdout().write_all(&result.stdout)?;
            }
            if !result.stderr.is_empty() {
                io::stderr().write_all(&result.stderr)?;
            }

            // Cleanup: remove the uploaded file
            let _ = device.remove(&remote_path);

            if result.is_error() {
                std::process::exit(1);
            }
        }
        Command::Put { local, remote } => {
            if !Path::new(local).exists() {
                return Err(MpError::InvalidInput(format!("file not found: {}", local)).into());
            }

            let mut device = open_device(&cli)?;
            eprintln!("Uploading {} -> {}...", local, remote);
            device.write_file(local, remote).map_err(|e| anyhow::anyhow!(e))?;
            eprintln!("Done.");
        }
        Command::Get { remote, local } => {
            let mut device = open_device(&cli)?;
            eprintln!("Downloading {} -> {}...", remote, local);
            device.get_file(remote, local).map_err(|e| anyhow::anyhow!(e))?;
            eprintln!("Done.");
        }
        Command::Exec { code } => {
            let mut device = open_device(&cli)?;
            let result = device.exec(code).map_err(|e| anyhow::anyhow!(e))?;

            if !result.stdout.is_empty() {
                io::stdout().write_all(&result.stdout)?;
            }
            if !result.stderr.is_empty() {
                io::stderr().write_all(&result.stderr)?;
            }

            if result.is_error() {
                std::process::exit(1);
            }
        }
        Command::Ls { path } => {
            let mut device = open_device(&cli)?;
            let entries = device.list_dir(path).map_err(|e| anyhow::anyhow!(e))?;
            for entry in &entries {
                println!("{}", entry);
            }
        }
        Command::Rm { path } => {
            let mut device = open_device(&cli)?;
            device.remove(path).map_err(|e| anyhow::anyhow!(e))?;
            eprintln!("Removed: {}", path);
        }
        Command::Mkdir { path } => {
            let mut device = open_device(&cli)?;
            device.mkdir(path).map_err(|e| anyhow::anyhow!(e))?;
            eprintln!("Created: {}", path);
        }
    }

    Ok(())
}
