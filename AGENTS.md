# HDC Rust - Agent Notes

## Project Overview

Rust reimplementation of Huawei's HDC (HarmonyOS Device Connector) host tool.

- **Workspace crates**: `hdc-protocol`, `hdc` (host), `hdcd` (daemon stub)
- **Target platform**: Windows (MSVC), Linux, macOS
- **USB library**: `rusb = "0.9"` → bundles `libusb1-sys` with vendored libusb v1.0.27
- **Crypto**: `rsa = "0.9"`, `sha2 = "0.10"`, `base64 = "0.22"`

---

## Critical USB Discovery (Windows + WinUSB)

### Problem
USB bulk write on endpoint 0x01 timed out with `LIBUSB_ERROR_TIMEOUT` (-7) using both sync and async libusb APIs, even though:
- Control transfers (GET_DESCRIPTOR) worked fine
- Official `hdc.exe` worked on the same device

### Root Cause
After `libusb_open` + `libusb_claim_interface`, calling **either** of these breaks WinUSB bulk transfer:

```rust
handle.set_alternate_setting(iface_num, 0)?;   // DON'T CALL
handle.clear_halt(endpoint)?;                    // DON'T CALL
```

These calls were added based on generic libusb/WinUSB advice, but they corrupt the WinUSB pipe state for this specific HarmonyOS device (VID=0x12D1, PID=0x1101).

### Official Tool Behavior
The official C++ `hdc.exe` does **NOT** call `set_alternate_setting` or `clear_halt` after `libusb_claim_interface`. It simply:
1. `libusb_open`
2. `libusb_claim_interface`
3. Start async I/O

### Solution
Remove all `set_alternate_setting` and `clear_halt` calls after interface claim. Only `claim_interface` is needed.

### Timeout Values
Match official tool:
- Bulk write timeout: **30 seconds**
- Bulk read timeout: **infinite (0)** in official tool; use reasonable value in Rust

---

## Device Profile

| Property | Value |
|----------|-------|
| Serial | `23RVB24927007698` |
| VID | `0x12D1` (Huawei) |
| PID | `0x1101` |
| Interface class | `0xFF` (vendor) |
| Interface subclass | `0x50` |
| Interface protocol | `0x01` |
| Bulk OUT | `0x01` |
| Bulk IN | `0x81` |
| Max packet size | 512 |

---

## Implemented Features

### Authentication
- [x] RSA 3-way handshake (auth_type 0→3→2→4)
- [x] Host generates 3072-bit RSA key, stores in `%USERPROFILE%/.harmony/hdckey`
- [x] Post-auth signature handling (user clicks "always trust")
- [x] `session_id` synchronization from daemon handshake response

### Shell
- [x] Single command: `hdc shell <cmd>` (e.g., `hdc shell pwd`, `hdc shell echo`)
- [x] Interactive shell: `hdc shell` (PTY-based via `ShellInit`, not `UnityExecute`)
- [x] Bidirectional stdin/stdout bridge with length-prefixed messages

### File Transfer
- [x] **File Send** (`hdc file send <local> <remote>`)
  - Host = master (reader), daemon = slave (writer)
  - Flow: `WakeupSlavetask` → `FileCheck` (TransferConfig) → `FileBegin` → `FileData` (64-byte TransferPayload header) → `FileFinish[1]` → `FileFinish[0]`
- [x] **File Recv** (`hdc file recv <remote> <local>`)
  - Daemon = master (reader), host = slave (writer)
  - Flow: `FileInit` → `WakeupSlavetask` → `FileCheck` (TransferConfig) → open local file → `FileBegin` → `FileData` (64-byte header) → `FileFinish[1]` (from host) → `FileFinish[0]` (from daemon)

### App Install
- [x] **App Install** (`hdc install <path.hap>`)
  - Same transfer protocol as file send, but commands are `AppInit`/`AppCheck`/`AppBegin`/`AppData`/`AppFinish`
  - Daemon auto-runs `bm install -p <path>` after receiving all data
  - Daemon sends `AppFinish[mode, result, message]` with install result
  - Tested successfully with 1.4MB signed HAP file

### Port Forwarding
- [x] **fport** (`hdc fport tcp:local <remote>`)
  - Host binds local TCP port, daemon connects to remote port/socket on device
  - Protocol (plain `tcp:` / `localabstract:` / etc.): `ForwardCheck` → `ForwardCheckResult` → `ForwardActiveSlave` → `ForwardActiveMaster` → `ForwardData` ↔ `ForwardFreeContext`
  - Protocol (`ark:` / `jdwp:` debugger forwards): `ForwardInit` → `ForwardCheck` (daemon) → `ForwardCheckResult` (host) → `ForwardSuccess` → `ForwardCheck` (host follow-up) → `ForwardCheckResult` (daemon) → `ForwardActiveSlave` → `ForwardActiveMaster` → `ForwardData` ↔ `ForwardFreeContext`
    - This daemon build requires the follow-up `ForwardCheck` to trigger `SetupArkPoint`/`SetupJdwpPoint`; a plain `ForwardInit` would only create a TCP master.
    - The host does **not** send `WakeupSlavetask` before `ForwardInit`; sending it confuses the daemon.
  - Supports concurrent connections (each gets unique forward_id)
  - Daemon-side remote specs: `tcp:port`, `localabstract:name`, `localreserved:name`, `localfilesystem:name`, `dev:name`, `jdwp:pid`, `ark:pid@package`
  - `ark:...` is used by DevEco Studio for ArkTS/ArkUI debugging (e.g. `fport tcp:15037 ark:64929@com.example.myapplication6_0_1`)
  - `fport ls` / `fport rm` are implemented on the host side (list entries in memory; remove aborts listener and sends `KernelChannelClose`)
- [x] `rport` (reverse port forward) implemented
  - Host/server side: full protocol handshake (`ForwardInit` → `ForwardCheck` → `ForwardCheckResult` → `ForwardSuccess` → `ForwardActiveSlave` → `ForwardActiveMaster` → `ForwardData`/`ForwardFreeContext`), `rport ls` / `rport rm`.
  - Daemon side: a Linux stub daemon (`hdcd`) can be built and run on a remote Linux host (e.g. `ssh attect@192.168.8.157`) to verify the data path end-to-end. The stub listens on `0.0.0.0` for the remote spec, bridges to the host's local port, and cleans up on `KernelChannelClose`.

### Network (Wireless / TCP)
- [x] **Explicit TCP connect** (`hdc tconn <ip:port>`)
  - Opens a TCP socket to the device daemon and runs the standard `SessionHandShake` + RSA authentication exchange
  - Authentication flow: `None → Publickey → Signature → OK` (with 60 s confirmation timeout)
- [x] **UDP device discovery** (`hdc discover`)
  - Sends `OHOS HDC` to the broadcast address and the local /24 subnet on port `8710`
  - Parses daemon replies `OHOS HDC-<tcp_port>` and adds found devices as `Ready`
  - Note: some access points / firewalls drop UDP discovery packets; in that case use explicit `tconn <ip:port>`
- [x] **Commands over TCP**: `shell`, `file send/recv`, `install`, `fport`, `hilog`, `bugreport`, etc. reuse the same dispatch handlers as USB via a transport-agnostic `send_to_session` helper
- [x] **`-t <key>` target selection**: works for TCP (`-t ip:port`) and USB (`-t serial`) keys; auto-selects the only connected device when no `-t` is given

---

## Protocol Details

### File/App Transfer Payload Format

`FileData` / `AppData` payload = 64-byte `TransferPayload` header + actual file bytes

```rust
pub struct TransferPayload {
    pub index: u64,           // file offset
    pub compress_type: u8,    // 0 = none
    pub compress_size: u32,   // same as data len if no compression
    pub uncompress_size: u32, // same as data len if no compression
}
```

Serialized via protobuf-like TLV. No compression currently used (`compress_type = 0`).

### FileTransfer Role Matrix

| Command | Host Role | Daemon Role | Who sends FileFinish[1] first |
|---------|-----------|-------------|-------------------------------|
| `file send` | master (reader) | slave (writer) | host |
| `file recv` | slave (writer) | master (reader) | host |
| `app install` | master (reader) | slave (writer) | daemon (auto after write) |

### TransferConfig (protobuf-like TLV)

```rust
pub struct TransferConfig {
    pub file_size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub options: String,
    pub path: String,
    pub optional_name: String,
    pub update_if_new: bool,
    pub compress_type: u8,
    pub hold_timestamp: bool,
    pub function_name: String,
    pub client_cwd: String,
    pub reserve1: String,
    pub reserve2: String,
}
```

### AppInstall Finish Payload

Daemon sends `AppFinish` with payload:
- `payload[0]` = mode (`1` = install, `2` = uninstall)
- `payload[1]` = result (`0` = success, `1` = fail) — **inverted due to official daemon bug**
- `payload[2..]` = message string

**Bug explanation**: `AsyncInstallFinish` receives `exitStatus` as `bool result` (true=success=1) from `AsyncCmd::FinishShellProc`, but checks `exitStatus == 0`. So `payload[1] == 0` means success.

---

## Known Issues & Workarounds

### Git Bash Path Translation
Git Bash automatically converts Unix-style paths like `/data/path` to Windows paths. Must use `//data//path` syntax to disable this translation.

```bash
# WRONG - Git Bash converts to C:/Program Files/Git/data/path
hdc file send test.txt /data/local/tmp/test.txt

# CORRECT - double slashes prevent translation
hdc file send test.txt //data//local//tmp//test.txt
```

### `.app` vs `.hap` Files
DevEco Studio outputs `.app` files (App Pack) which are ZIP archives containing `.hap` files.
The Rust hdc host now auto-extracts `.app` packages during `hdc install <path>.app`:
- Detects `.app` extension (case-insensitive).
- Extracts all `.hap` entries to a temporary directory.
- Installs each `.hap` sequentially using the same app-install protocol.
- Cleans up the temporary directory after success or failure.
For manual installation of a single `.hap`, use `hdc install entry-default-signed.hap` as before.

### USB Device Re-enumeration
Device occasionally reports "removed" during enumeration (Windows re-enumeration quirk). Server auto-retries every ~2 seconds.

---

## Build & Test

```bash
# Build release binary
cargo build --release --bin hdc

# Run server mode (background)
target/release/hdc.exe -m

# Client commands (in another terminal)
target/release/hdc.exe list targets
target/release/hdc.exe shell echo hello
target/release/hdc.exe file send test.txt //data//local//tmp//test.txt
target/release/hdc.exe file recv //data//local//tmp//test.txt recv.txt
target/release/hdc.exe install entry-default-signed.hap

# Wireless / TCP commands
target/release/hdc.exe discover
target/release/hdc.exe tconn <ip>:<port>
target/release/hdc.exe list targets -v
target/release/hdc.exe -t <ip>:<port> shell echo hello   # -t selection pending
```

---

# WARNING

禁止使用构建的hdc覆盖官方的hdc

## Build Rules

### `hdcd` target

`hdcd` is the **HDC daemon** that runs on the **OpenHarmony device**. It is not built for Android and there is no Android cross-compile configuration in this project.

- Build `hdc` (host) for the development workstation (Windows/Linux/macOS).
- `hdcd` must be built for the OpenHarmony target when a proper OpenHarmony toolchain is available.

## Version History

| Date | Version | Notes |
|------|---------|-------|
| 2026-06-11 | **3.2.0e** | Upgraded from 3.0.0e. Aligned with official C++ protocol. Added heartbeat, large file transfer optimization (511KB IO buf), new commands (HeartbeatMsg, SpawnSub, KernelTargetReconnect, SslHandshake, UnityExecuteEx). |
| 2026-06-11 | **3.2.0e+deveco** | Fixed DevEco Studio compatibility. Discovered DevEco Studio connects to hdc server via TCP socket using a 48-byte `OHOS HDC` handshake + length-prefixed command protocol. Implemented dual-protocol server to support both our hdc client and DevEco Studio IDE. |
| Before | 3.0.0e | Initial implementation based on OpenHarmony 3.0.0e protocol. |

### DevEco Studio Compatibility Note

DevEco Studio does **not** invoke `hdc.exe` as a simple command-line tool for device discovery. Instead:
1. It starts `hdc.exe -m` (server mode) via `cmd /c ".\hdc.exe start"`.
2. It opens a TCP socket to `127.0.0.1:8710`.
3. It performs a 48-byte handshake: `[len-char]["OHOS HDC"][connectKey]`.
4. It polls every 500ms with `list targets -v` using `[4-byte BE length][command][\0]` framing.
5. It expects `serial connection status [address]` per line (space-separated).

Our server detects the client type by peeking the first byte (`,` / 0x2C indicates DevEco Studio) and handles both protocols on the same port.

For IDE socket commands that require daemon interaction, the server creates a virtual loopback channel and dispatches the command through the existing `dispatch_task` infrastructure. The loopback reader decodes HDC length-prefixed messages so the response returned to DevEco Studio is plain text. Supported IDE socket commands include:

- `alive` – keep-alive probe, returns empty body.
- `list targets` / `list targets -v` – device enumeration.
- `shell <cmd>` – non-interactive shell execution.
- `file send <local> <remote>` / `file recv <remote> <local>` – file transfer.
- `install <hap>` / `uninstall <bundle>` – app install/uninstall.
- `fport ...` / `rport ...` / `fport ls` / `fport rm` – port forwarding.
- `jpid` / `track-jpid` – JDWP process list / tracking.
- `target mount` / `target boot` / `smode` / `hilog` / `bugreport` – unity commands.

## 官方HDC
克隆https://gitcode.com/openharmony/developtools_hdc.git

## TODO / Remaining Work

### Completed in 3.2.0e upgrade
- [x] Port forwarding: implement `fport ls` and `fport rm`
- [x] Protocol upgrade: 3.0.0e → 3.2.0e (version, commands, constants)
- [x] Heartbeat mechanism (`CMD_HEARTBEAT_MSG = 5000`)
- [x] Large file transfer optimization (511KB IO buf)
- [x] `target reconnect` command
- [x] `spawn-sub` / `killall-sub` commands (basic support)
- [x] `shell -` / `-b` parameter support (`UnityExecuteEx = 1200`)
- [x] Forward `-e <ip>` binding (host listen IP for port forwarding)

### Remaining / Optional
- [x] SSL/PSK encrypted channel support
  - Implemented a pragmatic AES-128-GCM PSK encrypted TCP channel behind `OHOS_HDC_ENCRYPT_CHANNEL=1`.
  - Daemon generates a 32-byte PSK, encrypts it with the host RSA public key, and sends it as `AUTH_SSL_TLS_PSK`.
  - Host decrypts the PSK with its RSA private key and enables symmetric encryption for the session.
  - Verified end-to-end with the Linux stub daemon on `192.168.8.157:15555`: `hdc -t ... shell echo ...` works with the env var set.
- [x] Port forwarding: implement `rport` (reverse port forward) daemon-side (Linux stub daemon sufficient for host-side testing)
- [x] App install: support `.app` (App Pack) auto-extraction
- [x] App uninstall: implement `hdc uninstall <bundle>` (server-side + IDE socket support done)
- [x] JDWP debugging support (`jpid` / `track-jpid` via CLI and IDE socket)
- [x] Hilog support (`hilog` via CLI and IDE socket)
- [x] Bugreport support (`bugreport` via CLI and IDE socket)
- [x] Target reboot / remount commands (`target boot` / `target mount` via CLI and IDE socket)
- [x] TCP network transport (`tconn`, `discover`, file/app/forward/shell/hilog over Wi-Fi)
- [x] Multi-device support (`-t <key>` target selection; auto-select when only one device, error and list devices when multiple)
- [x] Flashd commands (`update`, `flash`, `erase`, `format`) — host-side protocol implemented and verified end-to-end against the Linux stub daemon. Real device flashing cannot be tested without actual firmware.
