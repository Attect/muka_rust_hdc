# Muka Rust HDC

A Rust reimplementation of Huawei's **HDC (HarmonyOS Device Connector)** host tool — a drop-in alternative to the official `hdc` binary for connecting to HarmonyOS / OpenHarmony devices over **USB** and **TCP/Wi-Fi**.

- **Author**: Attect
- **Project website**: <https://muka.cool/rust_hdc>
- **Repository**: <https://github.com/Attect/muka_rust_hdc>
- **License**: MIT
- **Protocol version**: aligned with official HDC **3.2.0e**

## Why

The official HDC is a closed-source C++ binary shipped with DevEco Studio. Muka Rust HDC reimplements the host side (and a daemon stub) in Rust, with the same wire protocol, so it can:

- talk to real HarmonyOS devices (tested with VID `0x12D1` / PID `0x1101`),
- act as a server for **DevEco Studio** (dual-protocol server on `127.0.0.1:8710`),
- run on Windows (MSVC), Linux, and macOS from a single codebase.

## Workspace Layout

| Crate | Path | Role |
|-------|------|------|
| `hdc` | `hdc/` | Host tool: client CLI + background server (`hdc -m`) |
| `hdcd` | `hdcd/` | Daemon stub (for OpenHarmony target; a Linux stub is used for end-to-end testing) |
| `hdc-protocol` | `crates/hdc-protocol/` | Shared protocol primitives (packet framing, serializer, encryption) |

Protocol references reverse-engineered from the official implementation live in `docs/`:

- `docs/HDC_SERVER_SOCKET_PROTOCOL.md` — client↔server socket protocol (incl. DevEco Studio handshake)
- `docs/USB_TRANSPORT.md` — USB transport notes (WinUSB quirks, endpoints, timeouts)

## Features

- **Transports**
  - USB via `rusb`/libusb (WinUSB on Windows; no `set_alternate_setting` / `clear_halt` after interface claim — see `docs/USB_TRANSPORT.md`)
  - TCP: `hdc tconn <ip:port>`, UDP discovery (`hdc discover`, port 8710)
  - Optional AES-128-GCM PSK encrypted channel (`OHOS_HDC_ENCRYPT_CHANNEL=1`)
- **Authentication**: RSA 3-way handshake (3072-bit key in `%USERPROFILE%/.harmony/hdckey` / `~/.harmony/hdckey`), "always trust" signature flow, heartbeat (`HeartbeatMsg`)
- **Shell**: one-shot (`hdc shell <cmd>`) and interactive PTY (`hdc shell`)
- **File transfer**: `hdc file send` / `hdc file recv`, 511 KB IO buffer
- **Apps**: `hdc install` (`.hap`, and `.app` App Pack auto-extraction), `hdc uninstall`
- **Port forwarding**: `fport` / `rport` (incl. `ls` / `rm`), `tcp:`, `localabstract:`, `localreserved:`, `localfilesystem:`, `dev:`, `jdwp:pid`, `ark:pid@package` (ArkTS debugging used by DevEco Studio)
- **Device control**: `target mount`, `target boot`, `target reconnect`, `smode`
- **Diagnostics**: `hilog`, `bugreport`, `jpid` / `track-jpid` (JDWP)
- **Flashd**: `update`, `flash`, `erase`, `format` (host-side protocol, verified against the stub daemon)
- **Multi-device**: `-t <key>` target selection (USB serial or `ip:port`), auto-select when only one device is connected
- **DevEco Studio compatibility**: `hdc -m` serves both the Rust client protocol and the IDE's 48-byte `OHOS HDC` handshake + length-prefixed command framing on the same port

## Build

Requires a recent stable Rust toolchain. On Windows, use the MSVC toolchain; libusb v1.0.27 is vendored via `libusb1-sys`.

```bash
cargo build --release --bin hdc
```

`hdcd` targets OpenHarmony and requires an OpenHarmony toolchain; it is not built for Android.

## Usage

Start the background server first (clients auto-start it when needed):

```bash
hdc -m                          # server mode (also serves DevEco Studio on 127.0.0.1:8710)
```

Common commands:

```bash
hdc list targets -v             # enumerate devices
hdc shell echo hello            # one-shot shell command
hdc shell                       # interactive PTY shell
hdc file send test.txt /data/local/tmp/test.txt
hdc file recv /data/local/tmp/test.txt recv.txt
hdc install entry-default-signed.hap
hdc install myapp.app           # .app App Pack: .hap entries are extracted and installed
hdc uninstall com.example.myapp
hdc fport tcp:8080 tcp:8080     # forward host :8080 to device :8080
hdc fport ls / hdc fport rm tcp:8080
hdc rport tcp:9000 tcp:9000     # reverse forward
hdc tconn 192.168.1.10:5555     # connect over TCP/Wi-Fi
hdc discover                    # UDP device discovery
hdc -t 192.168.1.10:5555 shell echo hello   # pick a specific device
hdc hilog                       # stream device logs
hdc bugreport                   # collect bug report
hdc jpid                        # list debuggable (JDWP) processes
```

### Git Bash path note (Windows)

Git Bash mangles Unix-style remote paths. Use double slashes to disable path translation:

```bash
# wrong: /data/local/tmp becomes C:/Program Files/Git/data/local/tmp
hdc file send test.txt //data//local//tmp//test.txt
```

## Development Notes

- `AGENTS.md` contains the authoritative engineering log: protocol details, role matrix for file/app transfer, known issues, and version history.
- `.cargo/config.toml` (machine-specific build paths), `.codex/`, `.idea/`, and other local tool directories are git-ignored.
- Do **not** overwrite an official `hdc.exe` installation with binaries built from this repository.

---

*This project was implemented with the assistance of AI.*
