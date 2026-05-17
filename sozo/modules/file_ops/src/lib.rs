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
        const MAX_MSG_BUFFER: usize = 1024;

        let (tx, rx) = tokio::sync::mpsc::channel(MAX_MSG_BUFFER);
        Self {
            rx: Mutex::new(rx),
            tx,
        }
    }
}

unsafe extern "C" fn plugin_init() -> *mut c_void {
    Box::into_raw(Box::new(FileOperations::new())) as *mut c_void
}

unsafe extern "C" fn plugin_destroy(instance: *mut c_void) {
    drop(unsafe { Box::from_raw(instance as *mut FileOperations) });
}

unsafe extern "C" fn plugin_run(instance: *mut c_void, channel: *mut c_void, token: *mut c_void) {}

unsafe extern "C" fn plugin_enqueue(instance: *mut c_void, msg: *const u8, len: usize) -> u8 {
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

// finish up the main extern functions and then work on internals parsing of certain things. just do simple prints for now.
// once built, just make the loader and see if we can do that

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
