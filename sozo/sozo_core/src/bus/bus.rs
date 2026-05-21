use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use sozo_api::{Module, ModuleIdentity, BusMessage, sozo_debug};

enum BusCommand {
    Register(Box<dyn Module>)
}

pub struct BusController {
    tx: Sender<BusCommand>
}

pub struct Bus {
    modules: HashMap<ModuleIdentity, Arc<dyn Module>>,
    handles: Vec<JoinHandle<()>>,
    rx: Receiver<BusMessage>,
    tx: Sender<BusMessage>,
    token: CancellationToken,
    ctrl_rx: Receiver<BusCommand>,
    ctrl_tx: Sender<BusCommand>
}

impl Bus {
    pub fn new() -> Bus {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<BusCommand>(MAX_MSG_BUFFER);

        Bus {
            modules: HashMap::new(),
            handles: Vec::new(),
            rx,
            tx,
            token: CancellationToken::new(),
            ctrl_rx,
            ctrl_tx
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
        loop {
            tokio::select! {
                _ = self.token.cancelled() => break,
                msg = self.rx.recv() => {
                    let Some(msg) = msg else {
                        break
                    };

                    match self.modules.get(&msg.identity) {
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
                ctrl_msg = self.ctrl_rx.recv() =>  {
                    let Some(ctrl_msg) = ctrl_msg else {
                        break
                    };

                    match ctrl_msg {
                        BusCommand::Register(module) => {
                            if !self.register(module) {
                                println!("failed to register the bus, send response back to loader");
                            }

                            // start it
                            // return result
                        }
                    }
                }
            }
        }

        self.token.cancel();
    }

    pub fn get_bus_controller(&self) -> BusController {
        BusController { tx: self.ctrl_tx.clone() }
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

/* TODO: Enforce msg lengths across the module bus -- ie network traffic is limited to a max size of 8192; Header + Msg + Padding should not exceed. Current implementation of the bus should ensure that messages do not exceed 4096 byte chunk lengths; currently just pulling from quic packet design */
/* TODO: Currently not checking for collisions due to sheer size as of now */
/* TODO: Build out a response via oneshot channel or something so loader knows actual response -- pass in a bool by reference or something -- dispatch calls register and then starts, sends result, etc. */
