use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use sozo_api::{Module, ModuleIdentity, BusMessage, sozo_debug};


pub struct LibraryLoader {
    identity: ModuleIdentity,
    tx: Sender<BusMessage>,
    rx: Receiver<BusMessage>,
}

impl LibraryLoader {
    pub fn new() -> LibraryLoader {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = tokio::sync::mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        LibraryLoader {
            identity: ModuleIdentity::SHELL,
            tx,
            rx,
        }
    }
}

#[async_trait::async_trait]
impl Module for LibraryLoader {
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken) {
        println!("Inside run");
    }

    fn enqueue(&self, msg: BusMessage) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

// standard module that is registered as part of core ( ie core is shell, quic, loader and all of them register to the module bus )
// performs runtime loading and registration of modules -- ie sends control messages to perform registration with the module bus and during registration the module is executed
// structure that is loaded is part of the foreign module layout -- this will contain the file descriptor of the module to avoid dropping and vtable
