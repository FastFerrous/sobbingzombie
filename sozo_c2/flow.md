# Sozo — Command Flow & Architecture

> Reference document for adding and understanding commands in the Sozo C2 framework.

---

## Table of Contents

1. [Project Structure](#project-structure)
2. [Component Overview](#component-overview)
3. [Module Identity System](#module-identity-system)
4. [Packet Format](#packet-format)
5. [Response Format](#response-format)
6. [End-to-End Flow: `ps`](#end-to-end-flow-ps)
7. [Adding a New Command](#adding-a-new-command)
8. [File Responsibility Matrix](#file-responsibility-matrix)

---

## Project Structure

```
sozo/
├── bridge.py          # All-in-one process: HTTP + WebSocket + QUIC orchestration
├── server.py          # QUIC server, packet crafting, chunk reassembly
├── modules.py         # Module registry, command map, response decoders
└── web/
    └── sozo_web.html  # Operator/observer UI, command renderers
```

Client-side (Rust, on the remote agent):
```
src/
├── bus/mod.rs         # Module trait, ModuleIdentity constants, IPC bus
├── shell/mod.rs       # Shell module — opcodes, send_response(), chunking
├── shell/tasklist.rs  # Tasklist implementation, pack_snapshot()
├── shell/netstat.rs   # Netstat implementation
└── shell/ls.rs        # Directory listing implementation
```

---

## Component Overview

```
Browser (sozo_web.html)
        │  WebSocket ws://127.0.0.1:8765
        ▼
bridge.py  ──────────────────────────────────────────────────
        │  calls proto.send_command(cmd_text)
        ▼
server.py  (SozoServerProtocol)
        │  resolves via modules.py → craft_packet(identity, payload)
        │  QUIC  0.0.0.0:4443  mTLS  ALPN: sozo
        ▼
[Remote Agent]
  Comms module  →  routes by identity field  →  Shell module
                                                      │
                                               executes action
                                                      │
                                               send_response() → chunks
        │  StreamDataReceived (per chunk)
        ▼
server.py  reassembles chunks  →  modules.py decoder  →  typed dict
        │  _push_event()  →  asyncio queue
        ▼
bridge.py  event_consumer()  →  tags seq  →  broadcast()
        │  WebSocket
        ▼
Browser  onRecv()  →  per-command renderer  →  DOM
```

---

## Module Identity System

Module identities are `u32` constants defined in `bus/mod.rs` on the client and mirrored in `modules.py` on the server. The identity field in the packet header is how the Comms module routes an inbound packet to the correct handler.

| Module   | Identity       | Location         |
|----------|----------------|------------------|
| `COMMS`  | `0xFF3A7C12`   | `modules.py`     |
| `SHELL`  | `0xFF8B2E45`   | `modules.py`     |

Custom/dynamic identities use the lower 24 bits (`0x00FFFFFF` range) and are assigned at runtime.

---

## Packet Format

### Outer sozo packet (every packet in both directions)

Built by `craft_packet()` in `server.py`. Big-endian throughout.

```
┌─────────────────────────────────────────┐
│ u64  total_size   (header + data + pad) │  8 bytes
│ u32  identity     (module identity)     │  4 bytes
│ u32  data_len                           │  4 bytes
│ u32  pad_len      (random noise)        │  4 bytes
├─────────────────────────────────────────┤  HEADER = 20 bytes
│ [data_len bytes]  payload               │
│ [pad_len bytes]   random padding        │
└─────────────────────────────────────────┘
```

`pad_len` is randomly chosen between `5%` and `15%` of `data_len` (minimum 1) to obscure payload size.

### Shell outbound payload (server → agent)

The data field of the outer packet. One byte opcode, optional arguments.

```
┌──────────────────────────────────────────┐
│ u8  opcode                               │
│ [optional argument bytes — UTF-8]        │
└──────────────────────────────────────────┘
```

| Opcode | Enum              | Command(s)          | Argument     |
|--------|-------------------|---------------------|--------------|
| `0x00` | `Tasklist`        | `ps`, `tasklist`    | none         |
| `0x01` | `Netstat`         | `netstat`, `ss`     | none         |
| `0x02` | `List`            | `ls`                | path (UTF-8) |

---

## Response Format

### Shell `send_response()` chunking

Large responses are split into chunks bounded by `MAXIMUM_DATA_SIZE`. Each chunk is wrapped in a separate outer sozo packet. `identity` in the outer header is always `0xFF8B2E45` (SHELL) on inbound responses.

#### Initial chunk (`chunk_index == 0`) — inner payload

```
┌─────────────────────────────────────────────────────┐
│ u64  total_data_len   raw payload size (all chunks) │  8 bytes
│ u8   retcode          ShellError repr u8            │  1 byte
│ u16  chunk_index      always 0                      │  2 bytes
│ u16  total_chunks                                   │  2 bytes
│ u32  chunk_data_len   bytes in this chunk           │  4 bytes
├─────────────────────────────────────────────────────┤  INIT HDR = 17 bytes
│ [chunk_data_len bytes of data]                      │
└─────────────────────────────────────────────────────┘
```

#### Continuation chunks (`chunk_index > 0`) — inner payload

```
┌─────────────────────────────────────────────────────┐
│ u16  chunk_index                                    │  2 bytes
│ u16  total_chunks                                   │  2 bytes
│ u32  chunk_data_len                                 │  4 bytes
├─────────────────────────────────────────────────────┤  CONT HDR = 8 bytes
│ [chunk_data_len bytes of data]                      │
└─────────────────────────────────────────────────────┘
```

### ShellError retcodes

| Value | Name                |
|-------|---------------------|
| `0`   | `Success`           |
| `1`   | `Critical`          |
| `2`   | `UnableToOpenDir`   |
| `3`   | `UnableToOpenFile`  |
| `4`   | `PermissionDenied`  |
| `5`   | `InvalidArguments`  |
| `6`   | `PathNotFound`      |
| `7`   | `Unknown`           |

### Tasklist data sub-format (after reassembly)

Produced by `tasklist.rs pack_snapshot()`. Parsed by `modules._decode_tasklist()`.

```
┌──────────────────────────────────────────┐
│ u32  total_size  (includes itself)       │  4 bytes
├──────────────────────────────────────────┤
│  ┌── per process (repeated) ──────────┐  │
│  │ u32  pid                           │  4 bytes
│  │ u32  ppid                          │  4 bytes
│  │ u64  stime   (unix epoch seconds)  │  8 bytes
│  │ u8   user_len                      │  1 byte
│  │ u8   tty_len                       │  1 byte
│  │ u8   exe_len                       │  1 byte
│  │ [user_len bytes]  username         │
│  │ [tty_len bytes]   tty path         │
│  │ [exe_len bytes]   command line     │
│  └────────────────────────────────────┘  │
└──────────────────────────────────────────┘
```

### Check-in data format

Sent unsolicited by Shell at startup, before any commands. Parsed by `modules._decode_checkin()`.

```
┌──────────────────────────────────────────┐
│ u32  total_length                        │  4 bytes
│ u8   user_len  + [user bytes]            │
│ u8   hostname_len + [hostname bytes]     │
│ u8   arch_len  + [arch bytes]            │
│ u8   version_len + [version bytes]       │
│ u32  pid                                 │  4 bytes
└──────────────────────────────────────────┘
```

---

## End-to-End Flow: `ps`

### Step 1 — `sozo_web.html`: user presses Enter

`sendCommand()` extracts verb `ps`, looks it up in `CMD_MAP`, confirms it is not a local command (`help`/`?`). Generates a random `seq`. Sends over WebSocket:

```json
{"type": "cmd", "target": "0xabc12345", "cmd": "ps", "seq": 7291043}
```

No card is created yet.

---

### Step 2 — `bridge.py`: routes the command

`ws_handler()` validates operator role. Resolves `target` to a `SozoServerProtocol` instance from `quic_server.active_connections`. Calls:

```python
ok = proto.send_command("ps")
inflight[proto.conn_identity] = 7291043
```

Broadcasts ack to all browsers (stored in `event_history` for replay):

```json
{"type": "ack", "seq": 7291043, "targets": ["0xabc12345"], "cmd": "ps"}
```

---

### Step 3 — `sozo_web.html`: ack creates the card

`onAck()` creates an entry and adds it to the left pane with a blue `run` badge. Entry stored in `S.pending[7291043]`. A 15-second timeout is armed. The card is auto-selected.

---

### Step 4 — `modules.py`: command resolution

`resolve_command("ps")` looks up `COMMAND_MAP["ps"]`:

```python
"ps": (SHELL, lambda args: _shell_payload(SHELL_OP_TASKLIST))
# returns (0xFF8B2E45, b"\x00")
```

---

### Step 5 — `server.py`: packet built and sent

`send_command("ps")` receives `(0xFF8B2E45, b"\x00")`. Records:

```python
self._stream_opcodes[stream_id] = 0       # SHELL_OP_TASKLIST
self._stream_module[stream_id]  = 0xFF8B2E45
```

Calls `craft_packet(0xFF8B2E45, b"\x00")`. Outer packet structure:

```
[u64 total][u32 0xFF8B2E45][u32 1][u32 pad_len][0x00][padding]
```

Sent via `send_stream_data()` + `transmit()`.

---

### Step 6 — Agent Rust: Comms routes to Shell

Comms unpacks the outer header, reads `identity = 0xFF8B2E45`, calls `shell.enqueue(msg)`.

---

### Step 7 — Agent Rust: Shell dispatches

`msg.msg[0] = 0x00` → `ShellOpcodes::Tasklist`. Calls `self.tasklist.get_snapshot()`.

---

### Step 8 — Agent Rust: Tasklist serialises

Reads `/proc`, builds process list, calls `pack_snapshot()`. Returns packed binary to Shell.

---

### Step 9 — Agent Rust: Shell chunks and sends

`send_response()` calculates chunk count. Sends one initial chunk (17-byte header) plus zero or more continuation chunks (8-byte header). Each chunk goes to Comms which wraps it in an outer sozo packet (`identity = 0xFF8B2E45`) and sends over QUIC.

---

### Step 10 — `server.py`: chunks reassembled

Each `StreamDataReceived` event calls `_handle_inner()`. `parse_chunk()` reads the response headers. Chunks accumulate in `self._chunks[stream_id]`. When `buf.complete`:

```python
opcode  = self._stream_opcodes.pop(stream_id)   # 0
module  = self._stream_module.pop(stream_id)     # 0xFF8B2E45
decoded = modules.decode_response(module, opcode, buf.data)
```

---

### Step 11 — `modules.py`: binary decoded

`decode_response(0xFF8B2E45, 0, data)` → `_decode_tasklist(data)`. Parses the binary process list into:

```python
{"cmd": "ps", "processes": [{"pid": 1, "ppid": 0, "stime": 1700000000, "user": "root", "tty": "?", "exe": "/sbin/init"}, ...]}
```

---

### Step 12 — `server.py`: event pushed

```python
_push_event({
    "type": "recv", "id": "0xabc12345",
    "module": "0xff8b2e45", "retcode": "Success",
    "data": {"cmd": "ps", "processes": [...]}
})
```

---

### Step 13 — `bridge.py`: seq tagged and broadcast

`event_consumer()` pops `inflight[0xabc12345]` → `7291043`. Adds `"seq": 7291043` to the event. Calls `broadcast()` — appended to `event_history`, sent to all browsers.

---

### Step 14 — `sozo_web.html`: rendered

`onRecv()` finds `S.pending[7291043]`, clears timeout. `data.cmd === "ps"` → `renderTasklist()` → sets `entry.structuredView`. `buildTasklistDOM()` renders a `<table>` with PID, PPID, USER, TTY, STIME, COMMAND columns in the right pane. Badge turns green `ok`.

---

## Adding a New Command

Only **`modules.py`** and **`sozo_web.html`** need to change. `bridge.py` and `server.py` are command-agnostic.

### 1. Define the module identity (if new module)

In `modules.py`:

```python
MY_MODULE = 0xFFXXXXXX    # mirror the value in bus/mod.rs
```

### 2. Add the command to `COMMAND_MAP`

```python
"mycommand": (MY_MODULE, lambda args: bytes([OPCODE]) + args.encode()),
```

### 3. Write the response decoder

```python
def _decode_mycommand(data: bytes) -> dict:
    # parse the binary format defined in the Rust module
    return {"cmd": "mycommand", "field": value, ...}
```

Register it:

```python
DECODERS[MY_MODULE] = {
    OPCODE: _decode_mycommand,
}
```

### 4. Add the renderer in `sozo_web.html`

In the `onRecv` switch:

```js
case 'mycommand': renderMyCommand(entry, data); break;
```

Implement the renderer and DOM builder:

```js
function renderMyCommand(entry, data) {
    entry.structuredView = { type: 'mycommand', data };
    finalizeEntry(entry, 'ok');
    if (S.selectedId === entry.id) renderOutputBody(entry);
}

function buildMyCommandDOM(data) {
    // return a DOM node
}
```

In `renderOutputBody`:

```js
case 'mycommand': body.appendChild(buildMyCommandDOM(sv.data)); break;
```

### 5. Add help text

In the `COMMANDS` array:

```js
{ name: "mycommand", aliases: ["mc"], help: "Description of what it does  [args]" },
```

---

## File Responsibility Matrix

| Concern                          | File            |
|----------------------------------|-----------------|
| Module identity constants        | `modules.py`    |
| Command → identity + opcode      | `modules.py`    |
| Binary response decoder          | `modules.py`    |
| Packet construction (`craft_packet`) | `server.py` |
| Chunk reassembly                 | `server.py`     |
| QUIC connection lifecycle        | `server.py`     |
| WebSocket auth + routing         | `bridge.py`     |
| Seq ↔ recv correlation           | `bridge.py`     |
| Event history + replay           | `bridge.py`     |
| HTTP serving of the UI           | `bridge.py`     |
| Token generation + auth          | `bridge.py`     |
| Card creation + selection        | `sozo_web.html` |
| Per-command DOM renderers        | `sozo_web.html` |
| Observer / operator role lock    | `sozo_web.html` |
| Target filter + connection naming| `sozo_web.html` |
| Help text registry               | `sozo_web.html` |
