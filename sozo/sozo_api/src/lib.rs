pub mod debug;

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct BusMessage {
    pub identity: ModuleIdentity, /* local routing destination -- module bus utilizes this to route internal to process */
    pub remote: Option<ModuleIdentity>, /* optional module identity to specify the remote or end `target` for the destination; Used via Comms to specify the over the wire module that should be routed this data */
    pub msg: Vec<u8>,
}

#[async_trait]
pub trait Module: Send + Sync {
    fn get_identity(&self) -> ModuleIdentity;
    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken);
    fn enqueue(&self, msg: BusMessage) -> bool;
}

#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
pub struct ModuleIdentity(pub u32);

impl ModuleIdentity {
    pub const COMMS: ModuleIdentity = ModuleIdentity(0xFF3A7C12);
    pub const SHELL: ModuleIdentity = ModuleIdentity(0xFF8B2E45);

    pub fn new(id: u32) -> ModuleIdentity {
        ModuleIdentity(id)
    }

    pub fn get_custom() -> ModuleIdentity {
        const CUSTOM_IDENTITY_RANGE: u32 = 0x00FFFFFF;
        ModuleIdentity(rand::random::<u32>() & CUSTOM_IDENTITY_RANGE)
    }
}
