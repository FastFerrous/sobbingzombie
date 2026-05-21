use crate::{Module, ModuleIdentity, BusMessage};
use std::{sync::Arc};
use tokio::sync::Notify;
use tokio::task::spawn_blocking;
use std::{ffi::c_void, slice};
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

unsafe extern "C" fn send_message(context: *mut c_void, local_identity: u32, remote_identity: *const u32, msg: *const u8, len: usize) -> u8 {
    if context.is_null() || msg.is_null() {
        return false as u8;
    }

    let msg_slice = unsafe { slice::from_raw_parts(msg, len) };
    let mut msg: Vec<u8> = Vec::new();
    if msg.try_reserve(msg_slice.len()).is_err(){
        return false as u8;
    }

    msg.extend_from_slice(msg_slice);

    let cxt = unsafe { &*(context as *const PluginContext) };
    let remote = unsafe { remote_identity.as_ref().copied() }.map(ModuleIdentity);

    cxt.tx.try_send(BusMessage {
        identity: ModuleIdentity(local_identity),
        remote,
        msg,
    }).is_ok() as u8
}

unsafe extern "C" fn poll_objects(context: *mut c_void) -> u8 {
    if context.is_null() {
        return PollStatus::Cancelled as u8;
    }

    let context = unsafe { &*(context as *const PluginContext) };
    let rt_handle = tokio::runtime::Handle::current();

    rt_handle.block_on(async {
        tokio::select! {
            _ = context.token.cancelled() => PollStatus::Cancelled,
            _ = context.notify.notified() => PollStatus::InboundMessage
        }.as_u8()
    })
}

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum PollStatus {
    Cancelled,
    InboundMessage
}

impl PollStatus {
    fn as_u8(self) -> u8 { self as u8 }
}

struct PluginContext {
    tx: Sender<BusMessage>,
    token: CancellationToken,
    notify : Arc<Notify>
}

#[repr(C)]
pub struct HostVTable {
    pub context: *mut c_void,
    pub send_bus_message: unsafe extern "C" fn(*mut c_void, u32, *const u32, *const u8, usize) -> u8,
    pub poll_objects: unsafe extern "C" fn(*mut c_void) -> u8,
}

#[repr(C)]
pub struct ModuleVTable {
    pub init: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(instance: *mut c_void),
    pub run: unsafe extern "C" fn(instance: *mut c_void, host_vtable: *const HostVTable),
    pub enqueue: unsafe extern "C" fn(instance: *mut c_void, msg: *const u8, len: usize) -> u8,
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
    notify : Arc<Notify>
}

impl PluginModule {
    pub fn new(mem_addr: *mut c_void, vtable: *const ModuleVTable, instance: *mut c_void) -> PluginModule {
        PluginModule {
            identity: ModuleIdentity::get_custom(),
            handle: ModuleHandle { vtable, state: instance },
            mem_addr,
            notify: Arc::new(Notify::new())
        }
    }
}

unsafe impl Send for PluginModule {}
unsafe impl Sync for PluginModule {}

#[async_trait]
impl Module for PluginModule{
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken ) {
        /*
         * plugin context
         * stores all required fields that are used during callback operations to signal messages back to plugin during runtime
        */
        let cxt = ThreadSafeCVoid(Box::into_raw(Box::new(PluginContext {
            tx: bus_channel,
            token,
            notify: self.notify.clone(),
        })) as *mut c_void);

        /* assign callback functions and store plugin context */
        let host_vtable = ThreadSafeHostVTable(Box::into_raw(Box::new(HostVTable {
            context: cxt.0,
            send_bus_message: send_message,
            poll_objects,
        })) as *const HostVTable);

        let state = ThreadSafeCVoid(self.handle.state);
        let module_vtable = ThreadSafeVTable(self.handle.vtable);

        let _ = spawn_blocking(move || {
            let module_vtable = &module_vtable;
            let state = &state;
            let host_vtable = &host_vtable;
            let context = &cxt;
            unsafe { ((*module_vtable.0).run)(state.0, host_vtable.0) }

            unsafe {
                drop(Box::from_raw(host_vtable.0 as *mut HostVTable));
                drop(Box::from_raw(context.0 as *mut PluginContext));
            }
        }).await;
    }

    fn enqueue(&self, msg: BusMessage) -> bool {
        let result = unsafe { ((*self.handle.vtable).enqueue)(self.handle.state, msg.msg.as_ptr(), msg.msg.len()) };
        if result == 1 {
            self.notify.notify_one();
            return true
        }
        false
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
