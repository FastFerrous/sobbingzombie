"""
bridge.py — Sozo all-in-one bridge
====================================
Runs three servers in one asyncio event loop:

  1. HTTP  http://127.0.0.1:<http-port>/    serves web/sozo_web.html
  2. WS    ws://127.0.0.1:<ws-port>/        browser ↔ bridge protocol
  3. QUIC  0.0.0.0:<quic-port>              remote agent connections

Tokens
------
Both operator and observer tokens are auto-generated (8 random chars) at
startup and printed to stdout.  Pass --operator-token / --observer-token
to override with fixed values (useful for scripted environments).

Cert directory
--------------
Pass --certs-dir (default ./certs).  The bridge expects:
  <certs-dir>/server_chain.crt
  <certs-dir>/server.key
  <certs-dir>/ca.crt

Concurrent recv routing
-----------------------
Each in-flight command is tracked as (conn_identity → seq) so that when a
recv arrives from a specific QUIC connection we can tag it with the correct
seq and the UI can route it to the right card — even when many commands are
simultaneously in-flight across multiple connections.

Usage
-----
  pip install websockets aioquic aiohttp
  python bridge.py [--http-port 8080] [--ws-port 8765] [--quic-port 4443]
                   [--certs-dir ./certs]
                   [--operator-token TOKEN] [--observer-token TOKEN]
"""

import argparse
import asyncio
import json
import logging
import os
import random
import signal
import string
import sys
from pathlib import Path
from typing import Optional
from urllib.parse import parse_qs, urlparse

try:
    import websockets
except ImportError:
    print("ERROR: pip install websockets")
    sys.exit(1)

try:
    from aiohttp import web as aiohttp_web
except ImportError:
    print("ERROR: pip install aiohttp")
    sys.exit(1)

import server as quic_server

logging.basicConfig(level=logging.INFO, format="[BRIDGE] %(message)s")
log = logging.getLogger("bridge")

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
_HERE = Path(__file__).parent
UI_FILE = _HERE / "web" / "sozo_web.html"  # served by HTTP

# Command timeout in seconds — server-side busy lock force-released if no recv.
# Match or exceed the browser-side CMD_TIMEOUT (15s).
CMD_TIMEOUT_S = 15

# ---------------------------------------------------------------------------
# Runtime config
# ---------------------------------------------------------------------------
cfg: dict = {
    "operator_token": None,
    "observer_token": None,
    "http_port": 8080,
    "ws_port": 8765,
}

# ---------------------------------------------------------------------------
# Shared state
# ---------------------------------------------------------------------------
event_queue: asyncio.Queue = asyncio.Queue(maxsize=4096)
event_history: list[dict] = []  # full replay log

# ws connection → {"role": str}
browser_clients: dict = {}

# Concurrent recv routing:
#   conn_identity (int) → seq (int)
# When we send a command to connection X with seq N, we record X→N.
# When a recv arrives from X we tag it with N and clear the mapping.
# This correctly handles multiple simultaneous in-flight commands across
# different connections without the broken "len == 1" heuristic.
inflight: dict[int, int] = {}  # conn_identity → seq

# Busy lock — Shell is a sync single-stream module. Only one command can be
# in-flight per connection at a time. Set True on send, cleared on recv or disc.
conn_busy: dict[int, bool] = {}  # conn_identity → busy

# Stores the original seq for a command that triggered a module load.
# Used to re-tag the recv after auto-retry so the browser card gets its output.
pending_cmd_seqs: dict[int, int] = {}  # conn_identity → seq

# ---------------------------------------------------------------------------
# Token generation
# ---------------------------------------------------------------------------
_CHARS = string.ascii_letters + string.digits


def _gen_token(n: int = 8) -> str:
    return "".join(random.SystemRandom().choice(_CHARS) for _ in range(n))


# ---------------------------------------------------------------------------
# Auth
# ---------------------------------------------------------------------------


def _role_for_token(token: Optional[str]) -> Optional[str]:
    op = cfg["operator_token"]
    obs = cfg["observer_token"]
    if op is None and obs is None:
        return "operator"  # no auth configured
    if op and token == op:
        return "operator"
    if obs and token == obs:
        return "observer"
    return None


# ---------------------------------------------------------------------------
# Broadcast / send
# ---------------------------------------------------------------------------


async def broadcast(msg: dict, *, store: bool = True) -> None:
    if store:
        event_history.append(msg)
    if not browser_clients:
        return
    payload = json.dumps(msg)
    await asyncio.gather(
        *[ws.send(payload) for ws in list(browser_clients)],
        return_exceptions=True,
    )


async def send_ws(ws, msg: dict) -> None:
    try:
        await ws.send(json.dumps(msg))
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Event queue consumer
# ---------------------------------------------------------------------------


async def event_consumer() -> None:
    while True:
        event = await event_queue.get()
        try:
            if event.get("type") == "recv":
                try:
                    conn_id = int(event.get("id", "0x0"), 16)
                except (ValueError, TypeError):
                    conn_id = 0
                seq = inflight.pop(conn_id, None)
                if seq is not None:
                    event = {**event, "seq": seq}
                # Release busy lock — connection is ready for next command
                conn_busy.pop(conn_id, None)

            elif event.get("type") == "load_result":
                try:
                    conn_id = int(event.get("id", "0x0"), 16)
                except (ValueError, TypeError):
                    conn_id = 0

                # Release busy lock — load counts as a completed operation
                inflight.pop(conn_id, None)
                conn_busy.pop(conn_id, None)

                if event.get("success") and event.get("pending_cmd"):
                    pending_cmd = event["pending_cmd"]
                    proto = quic_server.active_connections.get(conn_id)
                    retry_seq = pending_cmd_seqs.pop(conn_id, None)

                    if proto and retry_seq is not None:
                        ok = proto.send_command(pending_cmd)
                        if ok:
                            inflight[conn_id] = retry_seq
                            conn_busy[conn_id] = True
                            log.info(
                                f"Auto-retry '{pending_cmd}' seq={retry_seq} after load"
                            )
                        else:
                            log.error(
                                f"Auto-retry send_command failed for '{pending_cmd}'"
                            )
                    else:
                        log.warning(
                            f"Auto-retry skipped — proto={proto} retry_seq={retry_seq}"
                        )
                else:
                    # Load failed — clear pending seq
                    pending_cmd_seqs.pop(conn_id, None)
            if event.get("type") == "disc":
                try:
                    disc_id = int(event.get("id", "0x0"), 16)
                except (ValueError, TypeError):
                    disc_id = 0
                inflight.pop(disc_id, None)
                conn_busy.pop(disc_id, None)
                pending_cmd_seqs.pop(disc_id, None)

            await broadcast(event)
        except Exception as e:
            log.warning(f"broadcast error: {e}")


# ---------------------------------------------------------------------------
# HTTP — serves sozo_web.html
# ---------------------------------------------------------------------------


async def handle_http(request: aiohttp_web.Request) -> aiohttp_web.Response:
    if not UI_FILE.exists():
        return aiohttp_web.Response(status=404, text=f"UI not found: {UI_FILE}")
    return aiohttp_web.Response(
        body=UI_FILE.read_bytes(),
        content_type="text/html",
        headers={"Cache-Control": "no-cache"},
    )


async def start_http_server(port: int) -> None:
    app = aiohttp_web.Application()
    app.router.add_get("/", handle_http)
    runner = aiohttp_web.AppRunner(app)
    await runner.setup()
    await aiohttp_web.TCPSite(runner, "127.0.0.1", port).start()
    log.info(f"HTTP UI → http://127.0.0.1:{port}/")
    await asyncio.Future()


# ---------------------------------------------------------------------------
# WebSocket handler
# ---------------------------------------------------------------------------


async def ws_handler(ws) -> None:
    # ── Auth ────────────────────────────────────────────────────────────────
    try:
        raw_path = ws.request.path  # websockets ≥14
    except AttributeError:
        raw_path = getattr(ws, "path", "/")  # websockets <14

    qs = parse_qs(urlparse(raw_path).query)
    token = (qs.get("token") or [None])[0]
    role = _role_for_token(token)

    if role is None:
        await send_ws(ws, {"type": "error", "msg": "Unauthorized"})
        await ws.close(code=4001, reason="Unauthorized")
        log.warning(f"Rejected {ws.remote_address} (bad token)")
        return

    browser_clients[ws] = {"role": role}
    log.info(f"Browser [{role}] connected from {ws.remote_address}")
    await send_ws(ws, {"type": "hello", "role": role})

    # ── Replay history ───────────────────────────────────────────────────────
    if event_history:
        await send_ws(ws, {"type": "replay_start", "count": len(event_history)})
        for ev in event_history:
            await send_ws(ws, ev)
        await send_ws(ws, {"type": "replay_end"})

    # ── Message loop ─────────────────────────────────────────────────────────
    try:
        async for raw in ws:
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                await send_ws(ws, {"type": "error", "msg": "Invalid JSON"})
                continue

            mtype = msg.get("type")
            info = browser_clients.get(ws, {})

            # ── CMD ──────────────────────────────────────────────────────────
            if mtype == "cmd":
                if info.get("role") != "operator":
                    await send_ws(
                        ws,
                        {
                            "type": "error",
                            "msg": "Read-only session",
                            "seq": msg.get("seq"),
                        },
                    )
                    continue

                cmd_text = msg.get("cmd", "").strip()
                target = msg.get("target", "all")
                seq = msg.get("seq", 0)

                if not cmd_text:
                    await send_ws(
                        ws, {"type": "error", "msg": "Empty command", "seq": seq}
                    )
                    continue

                if target == "all":
                    conns = list(quic_server.active_connections.items())
                else:
                    try:
                        tid = int(target, 16)
                    except ValueError:
                        await send_ws(
                            ws,
                            {
                                "type": "error",
                                "msg": f"Bad target id: {target}",
                                "seq": seq,
                            },
                        )
                        continue
                    proto = quic_server.active_connections.get(tid)
                    conns = [(tid, proto)] if proto else []

                if not conns:
                    await send_ws(
                        ws,
                        {
                            "type": "error",
                            "msg": "No matching connected clients",
                            "seq": seq,
                        },
                    )
                    continue

                targets_hit = []
                for conn_id, proto in conns:
                    cid = proto.conn_identity
                    # Shell is sync — one command in-flight per connection at a time
                    if conn_busy.get(cid):
                        await send_ws(
                            ws,
                            {
                                "type": "error",
                                "msg": f"Connection {hex(cid)} busy — wait for current command to finish",
                                "seq": seq,
                            },
                        )
                        continue
                    # Hard gate: resolve before anything touches the wire.
                    # None = unknown, ("load_required", name) = needs load,
                    # (identity, payload) = ready to send.
                    import modules as _mod

                    resolve = _mod.resolve_command(cmd_text)

                    if resolve is None:
                        await send_ws(
                            ws,
                            {
                                "type": "error",
                                "msg": f"Unknown command '{cmd_text.split()[0]}' — type ? for help",
                                "seq": seq,
                            },
                        )
                        continue

                    if isinstance(resolve, tuple) and resolve[0] == "load_required":
                        module_name = resolve[1]
                        await broadcast(
                            {
                                "type": "load_start",
                                "id": hex(cid),
                                "module_name": module_name,
                                "seq": seq,
                                "cmd": cmd_text,
                            }
                        )
                        try:
                            ok = proto.send_load_sequence(module_name, cmd_text)
                        except Exception as e:
                            log.error(f"send_load_sequence failed: {e}")
                            ok = False
                        if not ok:
                            await send_ws(
                                ws,
                                {
                                    "type": "error",
                                    "msg": f"Module '{module_name}.so' not found in ./modules/",
                                    "seq": seq,
                                },
                            )
                            continue
                        targets_hit.append(hex(conn_id))
                        inflight[cid] = seq
                        conn_busy[cid] = True
                        pending_cmd_seqs[cid] = seq
                        continue

                    # Command is fully resolved — send it
                    try:
                        ok = proto.send_command(cmd_text)
                        if not ok:
                            await send_ws(
                                ws,
                                {
                                    "type": "error",
                                    "msg": "Stream busy — previous response still assembling",
                                    "seq": seq,
                                },
                            )
                            continue
                        targets_hit.append(hex(conn_id))
                        inflight[cid] = seq
                        conn_busy[cid] = True
                    except Exception as e:
                        log.error(f"send_command failed {hex(conn_id)}: {e}")
                        await send_ws(
                            ws,
                            {"type": "error", "msg": f"Send failed: {e}", "seq": seq},
                        )

                if targets_hit:
                    ack = {
                        "type": "ack",
                        "seq": seq,
                        "targets": targets_hit,
                        "cmd": cmd_text,
                    }
                    await broadcast(ack)
                    log.info(f"CMD seq={seq} '{cmd_text}' → {targets_hit}")

                    # Server-side timeout — if no recv arrives within CMD_TIMEOUT_S,
                    # clear the busy lock and broadcast an error so the browser card
                    # is marked failed and the input is unlocked.
                    # The browser has its own JS timeout too, but that doesn't clear
                    # the server-side conn_busy/inflight state.
                    async def _cmd_timeout(cids: list, s: int, txt: str) -> None:
                        await asyncio.sleep(CMD_TIMEOUT_S)
                        for c in cids:
                            if conn_busy.get(c) and inflight.get(c) == s:
                                log.warning(
                                    f"CMD timeout seq={s} '{txt}' on {hex(c)} — releasing busy lock"
                                )
                                inflight.pop(c, None)
                                conn_busy.pop(c, None)
                                pending_cmd_seqs.pop(c, None)
                                await broadcast(
                                    {
                                        "type": "recv",
                                        "id": hex(c),
                                        "seq": s,
                                        "error": f"timeout — no response after {CMD_TIMEOUT_S}s",
                                        "data": None,
                                    }
                                )

                    asyncio.create_task(
                        _cmd_timeout(
                            [
                                proto.conn_identity
                                for _, proto in conns
                                if hex(proto.conn_identity) in targets_hit
                            ],
                            seq,
                            cmd_text,
                        )
                    )

            # ── RENAME ───────────────────────────────────────────────────────
            elif mtype == "rename":
                if info.get("role") != "operator":
                    await send_ws(ws, {"type": "error", "msg": "Read-only session"})
                    continue
                conn_id = msg.get("id", "")
                alias = msg.get("alias", "").strip()
                try:
                    tid = int(conn_id, 16)
                except ValueError:
                    await send_ws(ws, {"type": "error", "msg": f"Bad id: {conn_id}"})
                    continue
                proto = quic_server.active_connections.get(tid)
                if not proto:
                    await send_ws(
                        ws, {"type": "error", "msg": f"Unknown connection: {conn_id}"}
                    )
                    continue
                proto.set_alias(alias)
                log.info(f"Renamed {conn_id} → '{alias}'")

            # ── PING ─────────────────────────────────────────────────────────
            elif mtype == "ping":
                await send_ws(ws, {"type": "pong"})

            else:
                await send_ws(ws, {"type": "error", "msg": f"Unknown type: {mtype!r}"})

    except websockets.exceptions.ConnectionClosedOK:
        pass
    except websockets.exceptions.ConnectionClosedError as e:
        log.warning(f"Unclean close from {ws.remote_address}: {e}")
    finally:
        browser_clients.pop(ws, None)
        log.info(f"Browser [{role}] disconnected from {ws.remote_address}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


async def main(args: argparse.Namespace) -> None:
    certs = Path(args.certs_dir)
    cert_chain = certs / "server_chain.crt"
    key_file = certs / "server.key"
    ca_cert = certs / "ca.crt"

    for p in (cert_chain, key_file, ca_cert):
        if not p.exists():
            log.error(f"Missing cert file: {p}")
            sys.exit(1)

    # Tokens — use provided or auto-generate
    cfg["operator_token"] = args.operator_token or _gen_token()
    cfg["observer_token"] = args.observer_token or _gen_token()
    cfg["http_port"] = args.http_port
    cfg["ws_port"] = args.ws_port

    quic_server.event_queue = event_queue

    http_port = args.http_port
    ws_port = args.ws_port
    quic_port = args.quic_port

    # ── Print startup banner ─────────────────────────────────────────────────
    sep = "─" * 56
    print(f"\n{sep}")
    print(f"  sozo bridge")
    print(sep)
    print(f"  HTTP UI   http://127.0.0.1:{http_port}/")
    print(f"  WS        ws://127.0.0.1:{ws_port}/")
    print(f"  QUIC      0.0.0.0:{quic_port}  (mTLS, ALPN: sozo)")
    print(f"  Certs     {certs.resolve()}")
    print(sep)
    print(f"  OPERATOR  http://127.0.0.1:{http_port}/?token={cfg['operator_token']}")
    print(f"  OBSERVER  http://127.0.0.1:{http_port}/?token={cfg['observer_token']}")
    print(f"{sep}\n")

    ws_server = await websockets.serve(ws_handler, "127.0.0.1", ws_port)

    await asyncio.gather(
        ws_server.wait_closed(),
        start_http_server(http_port),
        quic_server.start_quic_server(
            host="0.0.0.0",
            port=quic_port,
            cert_chain=str(cert_chain),
            key_file=str(key_file),
            ca_cert=str(ca_cert),
        ),
        event_consumer(),
    )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Sozo bridge — HTTP + WebSocket + QUIC in one process",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--http-port", type=int, default=8080, help="HTTP port for the UI"
    )
    parser.add_argument("--ws-port", type=int, default=8765, help="WebSocket port")
    parser.add_argument("--quic-port", type=int, default=4443, help="QUIC listen port")
    parser.add_argument(
        "--certs-dir",
        default="./certs",
        help="Directory containing server_chain.crt, server.key, ca.crt",
    )
    parser.add_argument(
        "--operator-token", default=None, help="Override auto-generated operator token"
    )
    parser.add_argument(
        "--observer-token", default=None, help="Override auto-generated observer token"
    )
    args = parser.parse_args()

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    def _shutdown():
        log.info("Shutting down…")
        for task in asyncio.all_tasks(loop):
            task.cancel()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _shutdown)
        except NotImplementedError:
            pass  # Windows

    try:
        loop.run_until_complete(main(args))
    except (KeyboardInterrupt, asyncio.CancelledError):
        log.info("Bridge stopped.")
