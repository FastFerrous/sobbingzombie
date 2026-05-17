use sozo_api::{sozo_debug, ModuleVTable, BusMessage};
use std::ffi::c_void;
use tokio::sync::mpsc::{Sender, Receiver};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct FileOperations {
    rx: Mutex<Receiver<BusMessage>>,
    tx: Sender<BusMessage>,
}

impl FileOperations {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        Self {
            rx: Mutex::new(rx),
            tx,
        }
    }
}

// use the structure above for whats required

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init() -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_destroy(instance: *mut c_void) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_run(instance: *mut c_void, channel: *mut c_void, token: *mut c_void) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_enqueue(instance: *mut c_void, msg: *const u8, len: usize) -> u8 {
    0u8
}

static VTABLE: ModuleVTable = ModuleVTable {
    init: plugin_init,
    destroy: plugin_destroy,
    run: plugin_run,
    enqueue: plugin_enqueue,
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn module_entry() -> *const ModuleVTable {
    &VTABLE
}

/*
 * unsafe extern "C" fn run(state: *mut c_void, sender: *mut c_void, token: *mut c_void) {
     let sender = unsafe { *Box::from_raw(sender as *mut Sender<BusMessage>) };
     let token = unsafe { *Box::from_raw(token as *mut CancellationToken) };
     let me = unsafe { &*(state as *const MyPluginState) };

     let handle = tokio::runtime::Handle::current();
     handle.block_on(async move {
         me.run_impl(sender, token).await;
     });
 }
 *
 */
