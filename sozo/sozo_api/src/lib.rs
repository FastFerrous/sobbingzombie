pub mod debug;

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use std::ffi::c_void;

pub struct BusMessage {
    pub identity: ModuleIdentity,       /* local routing destination -- module bus utilizes this to route internal to process */
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
    pub const LOADER: ModuleIdentity = ModuleIdentity(0xFF2E81A7);

    pub fn new(id: u32) -> ModuleIdentity {
        ModuleIdentity(id)
    }

    pub fn get_custom() -> ModuleIdentity {
        const CUSTOM_IDENTITY_RANGE: u32 = 0x00FFFFFF;
        ModuleIdentity(rand::random::<u32>() & CUSTOM_IDENTITY_RANGE)
    }
}

#[repr(C)]
pub struct ModuleVTable {
    pub init: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(state: *mut c_void),
    pub run: unsafe extern "C" fn(state: *mut c_void, channel: *mut c_void, token: *mut c_void),
    pub enqueue: unsafe extern "C" fn(state: *mut c_void, msg: *const u8, len: usize) -> u8,
}

#[repr(C)]
pub struct ModuleHandle {
    pub vtable: *const ModuleVTable,    /* static function table declared from foreign module -- this is the returned value from module_init() */
    pub state: *mut c_void,             /* once we have a handle to the vtable, we initialize the instance with the init() function to obtain a running instance of `self` */
}

pub struct PluginModule {
    identity: ModuleIdentity,
    handle : ModuleHandle,             /* virtual function address table along with `self` instance */
    mem_addr : *mut c_void,            /* address from dl_open */
}

unsafe impl Send for PluginModule {}
unsafe impl Sync for PluginModule {}

#[async_trait]
impl Module for PluginModule{
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken ) {
        let channel_ptr = Box::into_raw(Box::new(bus_channel)) as *mut c_void;
        let token_ptr = Box::into_raw(Box::new(token)) as *mut c_void;

        let channel = ThreadSafeCVoid(channel_ptr);
        let token = ThreadSafeCVoid(token_ptr);
        let state = ThreadSafeCVoid(self.handle.state);
        let vtable = ThreadSafeVTable(self.handle.vtable);

        let _ = spawn_blocking(move || {
            let vtable = &vtable;
            let state = &state;
            let channel = &channel;
            let token = &token;
            unsafe { ((*vtable.0).run)(state.0, channel.0, token.0) }
        }).await;
    }

    fn enqueue(&self, msg: BusMessage) -> bool {
        let boolean = unsafe { ((*self.handle.vtable).enqueue)(self.handle.state, msg.msg.as_ptr(), msg.msg.len()) };
        boolean == true as u8
    }
}

impl Drop for PluginModule{
    fn drop(&mut self) {
        unsafe {
            ((*self.handle.vtable).destroy)(self.handle.state);
            libc::dlclose(self.mem_addr);
        }
    }
}

/* Required for compiler to safely move addresses into spawned thread -- raw addresses by default impl Copy, but not Send */
struct ThreadSafeCVoid(*mut c_void);
struct ThreadSafeVTable(*const ModuleVTable);

unsafe impl Send for ThreadSafeCVoid {}
unsafe impl Send for ThreadSafeVTable {}

// consider a host side vtable that is supplied into the plugin run function rather than the token and sender.
// callback functions can be used that get performed over here where the runtime exists and just primitive errors returned to plugin
// removes tokio dependency

// so essentially we createa host side vtable that performs the actual actions, so we pass in three things
/*
 * {
 * opaque structure containing the sender, token and identity?
 * then a vtable containing that structure plus two function addresses which is host send a mesage to the bus and host is cancelled
 * }
 *
 * we would need those functions now here as extern c functions that are called -- ctx is used to determine who cals who, etc.
 *
 *
 *
 * Add HostVTable, HostCtx, host_send_message, host_is_cancelled to sozo_api.
 Change ModuleVTable::run to take *const HostVTable instead of two c_void pointers.
 Update PluginModule::run to construct the host vtable and pass it through.
 Rewrite FileOperations::plugin_run as a sync polling loop using the host vtable.
 Drop Tokio from the plugin's Cargo.toml. Replace tokio::sync::Mutex with std::sync::Mutex, the channel with a VecDeque.
 Test with the harness. Verify the plugin size dropped significantly. Verify cancellation and message flow work.
 *
 */
