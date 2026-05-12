import asyncio
import ssl
from aioquic.asyncio import serve
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import QuicEvent, StreamDataReceived, HandshakeCompleted, ConnectionTerminated
import struct 
import os

HEADER_SIZE = 20  # 8 + 4 + 4 + 4

def craft_packet(identity: int, data: bytes) -> bytes:
    data_len = len(data)
    min_pad = max(int(data_len * 0.05), 1)
    max_pad = max(int(data_len * 0.15), min_pad + 1)
    pad_len = os.urandom(1)[0] % (max_pad - min_pad + 1) + min_pad
    padding = os.urandom(pad_len)
    
    total_size = HEADER_SIZE + data_len + pad_len
    
    header = struct.pack(">QIII", total_size, identity, data_len, pad_len)
    return header + data + padding


class SozoServerProtocol(QuicConnectionProtocol):
    def quic_event_received(self, event: QuicEvent):
        if isinstance(event, HandshakeCompleted):
            alpn = self._quic.tls._alpn_protocols
            print(f"[SERVER] client connected, ALPN={alpn}")

        elif isinstance(event, StreamDataReceived):
            data = event.data
            print(f"[SERVER] recv stream={event.stream_id}: {data.hex()}")

            if len(data) < HEADER_SIZE:
                return

            total_size, identity, data_len, pad_len = struct.unpack_from(">QIII", data)
            msg = data[HEADER_SIZE:HEADER_SIZE + data_len]
            print(f"[SERVER] parsed: identity=0x{identity:08X} data={msg!r}")

            # echo back using the same identity so rust routes it correctly
            response = craft_packet(identity, msg)
            # self._quic.send_stream_data(
            #     stream_id=event.stream_id,
            #     data=response,
            # )
            # self.transmit()

        elif isinstance(event, ConnectionTerminated):
            print(f"[SERVER] client disconnected: {event.reason_phrase}")


async def main():
    config = QuicConfiguration(is_client=False)
    config.alpn_protocols = ["sozo"]
    config.idle_timeout = 300 # max seconds before considered connection as timed out with no traffic being recvd
    config.load_cert_chain("./certs/server_chain.crt", "./certs/server.key")

    # mutual
    config.verify_mode = ssl.CERT_REQUIRED
    config.load_verify_locations("./certs/ca.crt") 

    server = await serve(
        host="0.0.0.0",
        port=4443,
        configuration=config,
        create_protocol=SozoServerProtocol,
    )

    print("[SERVER] listening on 0.0.0.0:4443")

    await asyncio.Future()  # run forever


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("[SERVER] shutting down")



