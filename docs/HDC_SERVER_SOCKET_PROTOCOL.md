# HDC Server Socket Protocol

This document describes the TCP socket protocol used by the HDC host server (`hdc -m`) to communicate with IDE clients and monitoring tools. The server listens on `127.0.0.1:8710` by default (configurable via the `OHOS_HDC_SERVER_PORT` environment variable).

The server supports two client protocols on the same port:

1. **HDC Client Protocol** — used by the `hdc` command-line client.
2. **IDE Socket Protocol** — used by IDEs such as DevEco Studio for device discovery and command execution.

The server detects the client type automatically after accepting a connection.

---

## Connection Overview

```
Client                              HDC Server (hdc -m)
  |                                         |
  |---------- TCP connect 127.0.0.1:8710 -->|
  |                                         |
  |<-- handshake (protocol dependent) -----|
  |                                         |
  |<---------- command/response ---------->|
```

The server distinguishes the two protocols by **peeking the first byte** of the incoming stream with a short timeout:

- If the first byte is `0x2C` (`,`) within ~800 ms, the connection uses the **IDE Socket Protocol**.
- Otherwise, the connection uses the **HDC Client Protocol**, where the server initiates the handshake.

---

## HDC Client Protocol

Used by the `hdc` command-line tool. The server initiates the handshake.

### Handshake

1. Server sends a banner message:
   ```
   ["OHOS HDC"][zero padding to 12 bytes][channel_id LE u32][zero padding to 32 bytes]
   ```
   Total: 44 bytes.

2. Client responds with the same banner format plus a connect key at offset 12:
   ```
   ["OHOS HDC"][zero padding][connect key string][zero padding to 32 bytes]
   ```

3. All subsequent messages are length-prefixed:
   ```
   [4-byte big-endian length][payload]
   ```

### Commands

After the handshake, the client sends command strings such as:

- `list targets`
- `list targets -v`
- `shell <command>`
- `file send <local> <remote>`
- `file recv <remote> <local>`
- `install <path.hap>`
- `fport tcp:<local> tcp:<remote>`

The server parses these strings and forwards them to the connected device.

---

## IDE Socket Protocol

Used by IDE clients for continuous device monitoring and command execution.

### Handshake

1. Client sends a 48-byte header immediately after connecting:

   | Offset | Size | Content |
   |--------|------|---------|
   | 0      | 4    | Length character: `48 - 4 = 44` (`,`) |
   | 4      | 8    | Magic string: `OHOS HDC` |
   | 12     | 4    | Reserved / zero |
   | 16     | 32   | Connect key (zero-padded string, often empty) |

2. Server validates the magic `OHOS HDC` at offset 4.

3. Server echoes the same 48-byte header back to the client.

4. Client typically sends an `alive` command to keep the connection open.

### Message Framing

After the handshake, all messages use the same framing:

```
[4-byte big-endian length][payload bytes]
```

- For **commands** sent by the client, the payload is the command string optionally terminated by `\0`.
- For **responses** sent by the server, the payload is the raw output data.

### Device Discovery

The IDE client polls the server approximately every 500 ms with:

```
Command: "list targets -v"
```

The server responds with one or more lines in this format:

```
<serial> <connection_type> <status> [<address>]
```

Fields are separated by single spaces.

Examples:

```
23RVB24927007698 USB Connected
23RVB24927007698 TCP Connected 192.168.1.100:10123
```

Status values:

- `Ready` — Device is present but not yet authenticated/connected. The IDE typically ignores these entries.
- `Connected` — Device is online and ready for commands.
- `Offline` — Device is known but currently unreachable.

### Command Reference

The following commands are supported over the IDE socket connection.

#### `alive`

Keeps the monitoring connection alive. The server returns an empty response.

#### `list targets`

Returns a simple list of connected devices, one serial per line.

#### `list targets -v`

Returns a verbose device list, one device per line:

```
<serial> <USB|TCP|UART> <Connected|Ready|Offline> [<address>]
```

#### `shell <command>`

Executes a shell command on the default connected device and returns the standard output/error.

Example:

```
shell pwd
```

Response:

```
/
```

#### `file send <local_path> <remote_path>`

Sends a file from the host to the device.

#### `file recv <remote_path> <local_path>`

Receives a file from the device to the host.

#### `install <path.hap>`

Installs an application package on the device.

#### `uninstall <bundle_name>`

Uninstalls an application by bundle name.

#### `fport <local_spec> <remote_spec>`

Forwards a local port/socket to the device.

Supported node formats:

- `tcp:<port>` — TCP socket (host and device)
- `localabstract:<name>` — Unix abstract domain socket (device)
- `localreserved:<name>` — Harmony reserved socket (device)
- `localfilesystem:<name>` — Unix filesystem socket (device)
- `dev:<device>` — Unix device node (device)
- `jdwp:<pid>` — JDWP transport for the given PID (device)
- `ark:<pid>@<tid>@<Debugger|package>` — ArkTS/ArkUI debug transport (device)

Examples:

```
fport tcp:8080 tcp:8080
fport tcp:8080 localabstract:my_service
fport tcp:40272 ark:31065@1@com.example.myapplication6_0_1
```

#### `rport <remote_spec> <local_spec>`

Reverse port forwarding (when implemented).

#### `jpid`

Lists JDWP-debuggable processes on the device.

#### `track-jpid [-p <pid> | -a <bundle>]`

Tracks JDWP process changes. The `-p` option tracks a specific PID; `-a` tracks by bundle name.

#### `shell hilog`

Streams device log output.

### Response Behavior

- The server reads the 4-byte length, then reads exactly that many payload bytes.
- The server executes the command and writes back `[4-byte length][payload]`.
- Simple local commands (`alive`, `list targets`) are handled directly by the server.
- Complex daemon-bound commands (`shell`, `file`, `install`, `uninstall`, `fport`, `jpid`, etc.) are dispatched internally through a loopback channel that reuses the existing `dispatch_task` logic. The loopback reader decodes HDC length-prefixed messages so the response returned to the IDE client is plain text.
- The connection remains open for subsequent commands unless the client closes it or an unrecoverable error occurs.

---

## Error Responses

When a command cannot be executed, the server returns a text response beginning with `[Fail]`:

```
[Fail]No device connected
[Fail]Unknown command: <command>
[Fail]Failed to send command to device: <reason>
```

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OHOS_HDC_SERVER_PORT` | `8710` | TCP port the HDC server listens on. |
| `OHOS_HDC_HEARTBEAT` | unset | Set to `1` to disable server-to-daemon heartbeat. |
| `OHOS_HDC_LOG_LIMIT` | unset | Reserved for future log rotation configuration. |

---

## Version Compatibility

The server advertises its version during the `checkserver` command:

```
client Ver: 3.2.0e, server Ver: 3.2.0e
```

IDE clients may compare the client-side and server-side versions to decide whether to restart the server.
