# mpy_probe

A fast, lightweight CLI tool for interacting with MicroPython boards over serial USB. Designed for IDE integration (VSCode, etc.) — upload files, run scripts, open REPL, sync projects.

## Install

### From GitHub Releases

Download the latest binary for your platform from [Releases](../../releases).

### From source

```bash
cargo install --path .
```

## Quick Start

```bash
# Auto-detect device and open REPL
mpy_probe repl

# Upload and run a script
mpy_probe run main.py

# Upload / download files
mpy_probe put local.py /flash/main.py
mpy_probe get /flash/main.py local.py

# Execute Python code directly
mpy_probe exec "print(1 + 2)"

# Device info
mpy_probe info
```

## Commands

| Command | Description |
|---------|-------------|
| `repl` | Open interactive REPL |
| `run <file>` | Upload and execute a Python file |
| `exec <code>` | Execute Python code string |
| `put <local> <remote>` | Upload file to device |
| `get <remote> <local>` | Download file from device |
| `put-dir <local> <remote>` | Upload directory recursively |
| `get-dir <remote> <local>` | Download directory recursively |
| `sync <local> <remote>` | Sync local dir to device (changed files only) |
| `ls [path]` | List files (default: `/`) |
| `stat <path>` | Show file size, type, mtime |
| `rm <path>` | Remove file |
| `mkdir <path>` | Create directory |
| `info` | Show device version, memory, filesystem |
| `reset` | Soft-reset device (CTRL-D) |
| `interrupt` | Interrupt running program (CTRL-C) |

## Global Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--port <path>` | auto-detect | Serial port (`/dev/ttyACM0`, `COM3`, etc.) |
| `--baud <rate>` | 115200 | Baud rate (ignored by USB CDC devices) |
| `--timeout <secs>` | 10 | Connection timeout |
| `--json` | off | Output structured JSON for IDE integration |

## JSON Output (IDE Integration)

All commands support `--json` for machine-readable output:

```bash
$ mpy_probe --json info
{"version":"3.4.0; MicroPython v1.20.0 on 2025-12-23","platform":"mimxrt","machine":"RT1021","mem_free":122784,"fs_total":8388608,"fs_free":7512064}

$ mpy_probe --json ls /flash
{"path":"/flash","entries":["boot.py","main.py","lib"]}

$ mpy_probe --json stat /flash/boot.py
{"path":"/flash/boot.py","is_dir":false,"size":503,"mtime":1776009983}

$ mpy_probe --json sync ./src /flash/src
{"success":true,"uploaded":3,"downloaded":0,"deleted":1}
```

Error output (on stderr regardless of `--json`):
```json
{"error":"connection error: ..."}
```

## Sync Workflow

The `sync` command compares local and remote directories by file size, uploads new/changed files, and deletes remote files that don't exist locally:

```bash
# First sync — uploads everything
mpy_probe sync ./my_project /flash

# Edit some files locally...
# Second sync — only uploads changed files
mpy_probe sync ./my_project /flash
```

## Protocol

mpy_probe uses the MicroPython raw REPL protocol with raw-paste flow control:

- **Raw-paste mode** (default): Window-based flow control, fastest, prevents buffer overflow
- **Fallback**: 256-byte chunks with delay for devices that don't support raw-paste

File transfers encode data as hex strings (`bytes.fromhex()`) executed on the device. Upload batches 16 chunks per raw REPL session for large file support (tested up to 128KB+).

## Supported Devices

Any MicroPython board with a USB serial port (CDC or UART bridge). Tested on:

- NXP i.MX RT (mimxrt)
- ESP32 / ESP8266 (via USB-UART bridge)
- Raspberry Pi Pico (USB CDC)
- STM32 boards

## Build

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Cross-compile for Linux ARM64
cross build --release --target aarch64-unknown-linux-gnu
```

### Dependencies

- `libudev-dev` (Linux only, for serial port detection)
- `pkg-config` (Linux only)

```bash
# Ubuntu/Debian
sudo apt install libudev-dev pkg-config

# Arch
sudo pacman -S systemd
```

## License

MIT
