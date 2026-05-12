use super::module::{Module, ModuleIdentity};
use crate::sozo_debug;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct BusMessage {
    pub identity: ModuleIdentity, /* local routing destination -- module bus utilizes this to route internal to process */
    pub remote: Option<ModuleIdentity>, /* optional module identity to specify the remote or end `target` for the destination; Used via Comms to specify the over the wire module that should be routed this data */
    pub msg: Vec<u8>,
}

pub struct Bus {
    modules: HashMap<ModuleIdentity, Arc<dyn Module>>,
    handles: Vec<JoinHandle<()>>,
    rx: Receiver<BusMessage>,
    tx: Sender<BusMessage>,
    token: CancellationToken,
}

impl Bus {
    pub fn new() -> Bus {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        Bus {
            modules: HashMap::new(),
            handles: Vec::new(),
            rx,
            tx,
            token: CancellationToken::new(),
        }
    }

    pub async fn shutdown(&mut self) {
        self.token.cancel();
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
    }

    pub fn register(&mut self, module: Box<dyn Module>) -> bool {
        let identity = module.get_identity();

        if self.modules.try_reserve(1).is_err() {
            return false;
        }

        self.modules.insert(identity, Arc::from(module));

        true
    }

    pub fn deregister(&mut self, identity: ModuleIdentity) -> bool {
        self.modules.remove_entry(&identity).is_some()
    }

    pub fn start_modules(&mut self) -> bool {
        if self.handles.try_reserve(self.modules.len()).is_err() {
            return false;
        }

        for module in self.modules.values() {
            let token = self.token.clone();
            let msg_channel = self.tx.clone();
            let arc_module = module.clone();

            let handle = tokio::spawn(async move {
                arc_module.run(msg_channel, token).await;
            });

            self.handles.push(handle);
        }

        true
    }

    pub async fn dispatch(&mut self) {
        while let Some(msg) = self.rx.recv().await {
            match self.modules.get_mut(&msg.identity) {
                Some(module) => {
                    module.enqueue(msg);
                }
                None => {
                    sozo_debug!(
                        "bus_dispatch",
                        "supplied identity is not a valid registration"
                    );
                    return;
                }
            }
        }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        /*
         * `drop` performs synchronously so unable to await tokio threads
         * due to this case, we are calling abort as this should not occur unless something has gone wrong
         */
        self.token.cancel();
        for handle in self.handles.drain(..) {
            let _ = handle.abort();
        }
    }
}

/* TODO: Create start module as well so once things come in dynamically */
/* TODO: Enforce msg lengths across the module bus -- ie network traffic is limited to a max size of 8192; Header + Msg + Padding should not exceed. Current implementation of the bus should ensure that messages do not exceed 4096 byte chunk lengths */
/* TODO: Create foreign module ABI for runtime loading */
/* TODO: Once we start talking loader, we will need runtime control of the bus, will need extra channels or a BusCommand message type with watch */
