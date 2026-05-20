use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use sozo_api::{Module, ModuleIdentity, BusMessage, sozo_debug};
use sozo_api::plugin::PluginModule;
use tokio::sync::Mutex;

#[derive(Debug)]
enum LoadError {
    InvalidLength,
    Critical
}

enum LoadStatus {
    Error(LoadError),
    Waiting,
    Complete(Vec<u8>)
}

enum LoadState {
    Idle,
    Loading {
        so_size: usize,
        so_data: Vec<u8>
    }
}

pub struct LibraryLoader {
    identity: ModuleIdentity,
    tx: Sender<BusMessage>,
    rx: Mutex<Receiver<BusMessage>>,
}

impl LibraryLoader {
    pub fn new() -> LibraryLoader {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = tokio::sync::mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        LibraryLoader {
            identity: ModuleIdentity::LOADER,
            tx,
            rx : Mutex::new(rx)
        }
    }

    fn handle_inbound_msg(&self, state: &mut LoadState, msg: &[u8]) -> LoadStatus {
        const MAX_SO_SIZE : usize = 1024 * 1024;

        match state {
            LoadState::Idle => {
                if msg.len() != size_of::<u32>() {
                    return LoadStatus::Error(LoadError::InvalidLength);
                }

                let so_size = u32::from_be_bytes(msg.try_into().unwrap()) as usize;
                if so_size > MAX_SO_SIZE {
                    return LoadStatus::Error(LoadError::InvalidLength);
                }

                let mut so_data: Vec<u8> = Vec::new();
                if so_data.try_reserve(so_size).is_err() {
                    return LoadStatus::Error(LoadError::Critical);
                }

                *state = LoadState::Loading {
                    so_size,
                    so_data
                };

                LoadStatus::Waiting
            },
            LoadState::Loading {so_size, so_data} => {
                let remaining_size = *so_size - so_data.len();
                if msg.len() > remaining_size {
                    return LoadStatus::Error(LoadError::InvalidLength);
                }

                so_data.extend_from_slice(msg);

                if so_data.len() == *so_size {
                    let so = std::mem::take(so_data);
                    *state = LoadState::Idle;
                    return LoadStatus::Complete(so);
                }

                LoadStatus::Waiting
            }
        }
    }
}

#[async_trait::async_trait]
impl Module for LibraryLoader {
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken) {
        sozo_debug!("LibraryLoader::run", "starting");

        let mut rx = self.rx.lock().await;
        let mut state = LoadState::Idle;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    sozo_debug!("LibraryLoader::run", "cancelled");
                    break;
                }
                msg = rx.recv() => {
                    if let Some(msg) = msg {
                        if msg.msg.is_empty() {
                            break;
                        }

                        let _ = match self.handle_inbound_msg(&mut state, &msg.msg) {
                            LoadStatus::Waiting => println!("okay, waiting"),
                            _ => println!("other")
                        };

                        // perform result checks here -- perform follow on action.
                        // go into loading

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


    // once its loaded and we have a vtable, we can then wrap into a plugin module

    // get inbound message which should be a entire module -- need to grab all the bytes at once and then perform
    // write to mem_fd and write the bytes
    // build out the string path and dl open
    // close out fd since we dont need it now
    // search for module_entry
    // get vtable
    // wrap into plugin module
    // register to bus

    // need to build some kind of message system that permits adding to bus at runtime -- ie sep control channel?
    // most likey a control channel that sends messages of callback functions thatpermit registeration and staring, etc.
