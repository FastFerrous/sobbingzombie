"""
modules.py — Sozo module registry
===================================
Single source of truth for:
  - Module identity constants  (mirrors module.rs)
  - Command → (module_identity, packet_payload) mapping
  - Per-module response decoders that turn raw binary into typed dicts
    the bridge can push to the browser as JSON

Adding a new module
-------------------
1. Add its identity constant below.
2. Add command entries to COMMAND_MAP pointing at the new identity.
3. Implement a decoder function and register it in DECODERS.

Response wire format (from shell.rs send_response)
----------------------------------------------------
Initial chunk (chunk_index == 0):
  u64  total_data_len   raw payload size across ALL chunks (excl. response headers)
  u8   retcode          ShellError repr u8
  u16  chunk_index      always 0 for first
  u16  total_chunks
  u32  chunk_data_len
  [chunk_data_len bytes]

Continuation chunks (chunk_index > 0):
  u16  chunk_index
  u16  total_chunks
  u32  chunk_data_len
  [chunk_data_len bytes]
"""

from __future__ import annotations

import ipaddress
import struct
from dataclasses import dataclass, field
from typing import Callable, Optional

# ---------------------------------------------------------------------------
# Module identities  (mirrors module.rs ModuleIdentity consts)
# ---------------------------------------------------------------------------
COMMS = 0xFF3A7C12
SHELL = 0xFF8B2E45

# ---------------------------------------------------------------------------
# Shell opcodes  (mirrors shell.rs ShellOpcodes repr u8)
# ---------------------------------------------------------------------------
SHELL_OP_PS = 0x00  # Tasklist
SHELL_OP_NETSTAT = 0x01  # Netstat
SHELL_OP_LIST = 0x02  # DirWalker (ls)

# ---------------------------------------------------------------------------
# Command map
# ---------------------------------------------------------------------------


def _shell_payload(opcode: int, args: bytes = b"") -> bytes:
    return bytes([opcode]) + args


def _ls_payload(args: str) -> bytes:
    """
    DirWalker::parse_args expects:  u16 path_len (big-endian) + path bytes.
    Default path is "." if none supplied.
    """
    path = (args.strip() or ".").encode("utf-8")
    return bytes([SHELL_OP_LIST]) + struct.pack(">H", len(path)) + path


COMMAND_MAP: dict[str, tuple[int, Callable[[str], bytes]]] = {
    "ps": (SHELL, lambda args: _shell_payload(SHELL_OP_PS)),
    "netstat": (SHELL, lambda args: _shell_payload(SHELL_OP_NETSTAT)),
    "ls": (SHELL, lambda args: _ls_payload(args)),
}


def resolve_command(cmd_text: str) -> tuple[int, bytes] | None:
    """
    Given the full command string (e.g. "ls /tmp"), return
    (module_identity, packet_payload) or None if unrecognised.
    """
    parts = cmd_text.strip().split(None, 1)
    verb = parts[0].lower()
    args = parts[1] if len(parts) > 1 else ""
    entry = COMMAND_MAP.get(verb)
    if entry is None:
        return None
    identity, builder = entry
    return identity, builder(args)


# ---------------------------------------------------------------------------
# Response wire format constants
# ---------------------------------------------------------------------------
INIT_HDR_FMT = ">QBHHI"  # total_data_len u64, retcode u8, chunk_idx u16, total_chunks u16, data_len u32
INIT_HDR_SIZE = struct.calcsize(INIT_HDR_FMT)  # 17

CONT_HDR_FMT = ">HHI"  # chunk_idx u16, total_chunks u16, data_len u32
CONT_HDR_SIZE = struct.calcsize(CONT_HDR_FMT)  # 8

# ---------------------------------------------------------------------------
# ShellError retcodes  (mirrors shell.rs ShellError repr u8)
# ---------------------------------------------------------------------------
SHELL_ERRORS = {
    0: "Success",
    1: "Critical",
    2: "UnableToOpenDir",
    3: "UnableToOpenFile",
    4: "PermissionDenied",
    5: "InvalidArguments",
    6: "PathNotFound",
    7: "Unknown",
}


# ---------------------------------------------------------------------------
# Chunk reassembly
# ---------------------------------------------------------------------------
@dataclass
class ChunkBuffer:
    total_data_len: int = 0
    retcode: int = 0
    total_chunks: int = 0
    chunks: dict = field(default_factory=dict)

    @property
    def complete(self) -> bool:
        return len(self.chunks) == self.total_chunks and self.total_chunks > 0

    @property
    def data(self) -> bytes:
        return b"".join(self.chunks[i] for i in range(self.total_chunks))


def parse_initial_chunk(raw: bytes) -> tuple | None:
    """
    Parse an initial chunk (chunk_index == 0).
    Returns (total_chunks, retcode, chunk_data, total_data_len) or None.
    """
    if len(raw) < INIT_HDR_SIZE:
        return None
    total_data_len, retcode, chunk_idx, total_chunks, data_len = struct.unpack_from(
        INIT_HDR_FMT, raw
    )
    if chunk_idx != 0:
        return None
    chunk_data = raw[INIT_HDR_SIZE : INIT_HDR_SIZE + data_len]
    return (total_chunks, retcode, chunk_data, total_data_len)


def parse_continuation_chunk(raw: bytes) -> tuple | None:
    """
    Parse a continuation chunk (chunk_index > 0).
    Returns (chunk_idx, total_chunks, chunk_data) or None.
    """
    if len(raw) < CONT_HDR_SIZE:
        return None
    chunk_idx, total_chunks, data_len = struct.unpack_from(CONT_HDR_FMT, raw)
    chunk_data = raw[CONT_HDR_SIZE : CONT_HDR_SIZE + data_len]
    return (chunk_idx, total_chunks, chunk_data)


# Keep parse_chunk as a compatibility shim — no longer used by server.py
def parse_chunk(raw: bytes) -> tuple | None:
    result = parse_initial_chunk(raw)
    if result is not None:
        total_chunks, retcode, chunk_data, total_data_len = result
        return (0, total_chunks, retcode, chunk_data, total_data_len)
    result = parse_continuation_chunk(raw)
    if result is not None:
        chunk_idx, total_chunks, chunk_data = result
        return (chunk_idx, total_chunks, -1, chunk_data, -1)
    return None


# ---------------------------------------------------------------------------
# Decoders
# ---------------------------------------------------------------------------


def _decode_ps(data: bytes) -> dict:
    """
    Tasklist wire format (tasklist.rs pack_snapshot):
      u32  total_size   (includes itself)
      per process:
        u32  pid
        u32  ppid
        u64  stime      unix epoch seconds
        u8   user_len
        u8   tty_len
        u8   exe_len
        [user bytes][tty bytes][exe bytes]
    """
    if len(data) < 4:
        return {"cmd": "ps", "error": "truncated data", "processes": []}

    offset = 4  # skip total_size u32
    processes = []
    PROC_FIXED = 19  # 4+4+8+1+1+1

    while offset < len(data):
        if offset + PROC_FIXED > len(data):
            return {
                "cmd": "ps",
                "error": f"truncated at process header (offset {offset})",
                "processes": processes,
            }

        pid, ppid, stime = struct.unpack_from(">IIQ", data, offset)
        offset += 16
        user_len, tty_len, exe_len = struct.unpack_from(">BBB", data, offset)
        offset += 3

        needed = user_len + tty_len + exe_len
        if offset + needed > len(data):
            return {
                "cmd": "ps",
                "error": f"truncated at process strings (offset {offset})",
                "processes": processes,
            }

        user = data[offset : offset + user_len].decode("utf-8", errors="replace")
        offset += user_len
        tty = data[offset : offset + tty_len].decode("utf-8", errors="replace")
        offset += tty_len
        exe = data[offset : offset + exe_len].decode("utf-8", errors="replace")
        offset += exe_len

        processes.append(
            {
                "pid": pid,
                "ppid": ppid,
                "stime": stime,
                "user": user,
                "tty": tty,
                "exe": exe,
            }
        )

    return {"cmd": "ps", "processes": processes}


def _decode_netstat(data: bytes) -> dict:
    """
    Netstat wire format (netstat.rs parse_connections):
      u32  total_size   (includes itself)
      per connection:
        u8   protocol       0=TCP 1=UDP 2=TCP6 3=UDP6
        u8   local_addr_len (bytes: 4 for IPv4, 16 for IPv6)
        u16  local_port
        u8   remote_addr_len
        u16  remote_port
        u8   state
        u32  pid
        u8   exe_len
        u8   user_len
        [local_addr_len bytes]   local address
        [remote_addr_len bytes]  remote address
        [exe bytes]
        [user bytes]

    Note: addr_len field holds number of bytes (4 or 16), matching
    FIXED_IPV4_ADDRS_LEN/2 and FIXED_IPV6_ADDRS_LEN/2 from the Rust side.
    """
    if len(data) < 4:
        return {"cmd": "netstat", "error": "truncated data", "connections": []}

    CONN_FIXED = 14  # matches FIXED_CONNECTION_HDR_LEN in Rust

    PROTO_NAMES = {0: "TCP", 1: "UDP", 2: "TCP6", 3: "UDP6"}

    TCP_STATES = {
        0x01: "ESTABLISHED",
        0x02: "SYN_SENT",
        0x03: "SYN_RECV",
        0x04: "FIN_WAIT1",
        0x05: "FIN_WAIT2",
        0x06: "TIME_WAIT",
        0x07: "CLOSE",
        0x08: "CLOSE_WAIT",
        0x09: "LAST_ACK",
        0x0A: "LISTEN",
        0x0B: "CLOSING",
        0x00: "",
    }

    offset = 4  # skip total_size
    connections = []

    while offset < len(data):
        if offset + CONN_FIXED > len(data):
            return {
                "cmd": "netstat",
                "error": f"truncated at connection header (offset {offset})",
                "connections": connections,
            }

        # All values are in network byte order (big-endian) per RFC standard.
        protocol = data[offset]
        offset += 1
        local_addr_len = data[offset]
        offset += 1
        (local_port,) = struct.unpack_from(">H", data, offset)
        offset += 2
        remote_addr_len = data[offset]
        offset += 1
        (remote_port,) = struct.unpack_from(">H", data, offset)
        offset += 2
        state = data[offset]
        offset += 1
        (pid,) = struct.unpack_from(">I", data, offset)
        offset += 4
        exe_len = data[offset]
        offset += 1
        user_len = data[offset]
        offset += 1

        # DEBUG — remove once port values confirmed correct
        print(
            f"[DBG netstat] proto={protocol} raw_lport_bytes={data[offset - 8 : offset - 6].hex()} lport={local_port} raw_rport_bytes={data[offset - 5 : offset - 3].hex()} rport={remote_port}"
        )
        needed = local_addr_len + remote_addr_len + exe_len + user_len
        if offset + needed > len(data):
            return {
                "cmd": "netstat",
                "error": f"truncated at connection data (offset {offset})",
                "connections": connections,
            }

        local_addr_bytes = data[offset : offset + local_addr_len]
        offset += local_addr_len
        remote_addr_bytes = data[offset : offset + remote_addr_len]
        offset += remote_addr_len
        exe = data[offset : offset + exe_len].decode("utf-8", errors="replace")
        offset += exe_len
        user = data[offset : offset + user_len].decode("utf-8", errors="replace")
        offset += user_len

        # Parse addresses
        def fmt_addr(raw: bytes) -> str:
            try:
                if len(raw) == 4:
                    return str(ipaddress.IPv4Address(bytes(reversed(raw))))
                elif len(raw) == 16:
                    # IPv6 comes as 4 × u32 each in host byte order (little-endian on x86)
                    words = struct.unpack_from(">IIII", raw)
                    reordered = b"".join(struct.pack("<I", w) for w in words)
                    return str(ipaddress.IPv6Address(reordered))
                return raw.hex()
            except Exception:
                return raw.hex()

        # UDP/UDP6: only ESTABLISHED (0x01) is meaningful — a connected UDP
        # socket with a specific remote. Everything else (typically CLOSE/0x07)
        # means unconnected and should display as '-'.
        is_udp = protocol in (1, 3)  # Protocol::UDP or Protocol::UDP6
        if is_udp:
            state_str = "ESTABLISHED" if state == 0x01 else "-"
        else:
            state_str = TCP_STATES.get(state, f"0x{state:02X}")

        connections.append(
            {
                "proto": PROTO_NAMES.get(protocol, str(protocol)),
                "local_addr": fmt_addr(local_addr_bytes),
                "local_port": local_port,
                "remote_addr": fmt_addr(remote_addr_bytes),
                "remote_port": remote_port,
                "state": state_str,
                "pid": pid if pid != 0 else None,  # 0 = no owning process found
                "exe": exe if exe != "-" else None,
                "user": user,
            }
        )

    return {"cmd": "netstat", "connections": connections}


def _decode_ls(data: bytes) -> dict:
    """
    DirWalker wire format (ls.rs pack_entries):
      u32  total_size   (includes itself)
      per entry:
        u32  permissions   (stat mode bits)
        u64  inode
        u64  link_count
        u8   user_len
        u8   group_len
        u64  size
        u64  mtime         unix epoch seconds
        u64  ctime         unix epoch seconds
        u16  fn_len        filename length
        u16  link_len      symlink target length (0 if not a symlink)
        [user bytes][group bytes][filename bytes][link bytes]
    """
    if len(data) < 4:
        return {"cmd": "ls", "error": "truncated data", "entries": []}

    ENTRY_FIXED = 50  # matches FIXED_DIRENTRY_HDR_LEN in Rust

    offset = 4  # skip total_size
    entries = []

    while offset < len(data):
        if offset + ENTRY_FIXED > len(data):
            return {
                "cmd": "ls",
                "error": f"truncated at entry header (offset {offset})",
                "entries": entries,
            }

        (
            permissions,
            inode,
            link_count,
            user_len,
            group_len,
            size,
            mtime,
            ctime,
            fn_len,
            link_len,
        ) = struct.unpack_from(">IQQBBQQQhh", data, offset)
        # fn_len and link_len are u16 — unpack as signed then mask to handle large values safely
        fn_len = fn_len & 0xFFFF
        link_len = link_len & 0xFFFF
        offset += ENTRY_FIXED

        needed = user_len + group_len + fn_len + link_len
        if offset + needed > len(data):
            return {
                "cmd": "ls",
                "error": f"truncated at entry data (offset {offset})",
                "entries": entries,
            }

        user = data[offset : offset + user_len].decode("utf-8", errors="replace")
        offset += user_len
        group = data[offset : offset + group_len].decode("utf-8", errors="replace")
        offset += group_len
        filename = data[offset : offset + fn_len].decode("utf-8", errors="replace")
        offset += fn_len
        link = (
            data[offset : offset + link_len].decode("utf-8", errors="replace")
            if link_len
            else None
        )
        offset += link_len

        entries.append(
            {
                "permissions": permissions,
                "inode": inode,
                "link_count": link_count,
                "user": user,
                "group": group,
                "size": size,
                "mtime": mtime,
                "ctime": ctime,
                "filename": filename,
                "link": link,
            }
        )

    return {"cmd": "ls", "entries": entries}


def _decode_checkin(data: bytes) -> dict:
    """
    Check-in wire format (shell.rs perform_checkin):
      u32  total_length
      u8   user_len    + [user bytes]
      u8   hostname_len+ [hostname bytes]
      u8   arch_len    + [arch bytes]
      u8   version_len + [version bytes]
      u32  pid
    """
    if len(data) < 4:
        return {"error": "truncated checkin"}

    offset = 4  # skip total_length
    fields = {}
    for key in ("user", "hostname", "arch", "version"):
        if offset >= len(data):
            return {"error": f"truncated at field '{key}'", **fields}
        length = data[offset]
        offset += 1
        if offset + length > len(data):
            return {"error": f"truncated reading field '{key}'", **fields}
        fields[key] = (
            data[offset : offset + length].decode("utf-8", errors="replace").strip()
        )
        offset += length

    if offset + 4 <= len(data):
        fields["pid"] = struct.unpack_from(">I", data, offset)[0]
    else:
        return {"error": "truncated at pid field", **fields}

    return {"cmd": "checkin", **fields}


# ---------------------------------------------------------------------------
# Decoder registry
# opcode -1 = unsolicited (check-in)
# ---------------------------------------------------------------------------
DECODERS: dict[int, dict[int, Callable[[bytes], dict]]] = {
    SHELL: {
        -1: _decode_checkin,
        SHELL_OP_PS: _decode_ps,
        SHELL_OP_NETSTAT: _decode_netstat,
        SHELL_OP_LIST: _decode_ls,
    },
}


def decode_response(module_identity: int, opcode: int, data: bytes) -> dict:
    """
    Decode a fully-reassembled response payload into a typed dict.
    Returns an error dict if no decoder is registered or decoding raises.
    """
    module_decoders = DECODERS.get(module_identity, {})
    decoder = module_decoders.get(opcode)
    if decoder:
        try:
            return decoder(data)
        except Exception as e:
            return {"error": f"decode exception: {e}", "raw": data[:256].hex()}
    return {
        "error": f"no decoder for module {hex(module_identity)} opcode {opcode}",
        "raw": data[:256].hex(),
    }
