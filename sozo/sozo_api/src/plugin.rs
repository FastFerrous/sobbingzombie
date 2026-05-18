use crate::{Module, ModuleIdentity, BusMessage};

use tokio::task::spawn_blocking;
use std::ffi::c_void;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

extern "C" fn send_message(context: *mut c_void, local_identity: u32, remote_identity: *const u32, msg: *const u8, len: usize) -> u8 {
    0u8
}

extern "C" fn poll_token(context: *mut c_void) -> u8 {
    0u8
}

#[repr(C)]
pub struct PluginContext {
    tx: Sender<BusMessage>,
    token: CancellationToken,
}

#[repr(C)]
pub struct HostVTable {
    pub send_message: unsafe extern "C" fn(*mut c_void, u32, *const u32, *const u8, usize) -> u8,
    pub poll_token: unsafe extern "C" fn(*mut c_void) -> u8,
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
struct ThreadSafeHostVTable(*const HostVTable);

unsafe impl Send for ThreadSafeCVoid {}
unsafe impl Send for ThreadSafeVTable {}
unsafe impl Send for ThreadSafeHostVTable {}
