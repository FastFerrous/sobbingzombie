use sozo_api::plugin::{HostVTable, ModuleVTable};
use sozo_api::sozo_debug;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

struct FileOperations {
    rx: Mutex<Receiver<Vec<u8>>>,
    tx: SyncSender<Vec<u8>>,
}

impl FileOperations {
    fn new() -> FileOperations {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = sync_channel::<Vec<u8>>(MAX_MSG_BUFFER);

        FileOperations {
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

unsafe extern "C" fn plugin_run(instance: *mut c_void, host_vtable: *const HostVTable) {
    if instance.is_null() || host_vtable.is_null() {
        return;
    }

    let instance = unsafe { &*(instance as *const FileOperations) };
    let host_vtable = unsafe { &*host_vtable };

    let rx = match instance.rx.try_lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };

    sozo_debug!(
        "FileOperations::plugin_run",
        "currently sitting within plugin_run"
    );

    loop {
        sozo_debug!("FileOperations::plugin_run", "inside loop");
        let result = unsafe { (host_vtable.poll_objects)(host_vtable.context) };
        sozo_debug!("FileOperations::plugin_run", "result was {}", result);
        if result == 0 {
            sozo_debug!("FileOperations::plugin_run", "token was cancelled");
            break;
        }
    }
}

unsafe extern "C" fn plugin_enqueue(instance: *mut c_void, msg: *const u8, len: usize) -> u8 {
    if instance.is_null() || msg.is_null() {
        return false as u8;
    }

    let slice_msg = unsafe { std::slice::from_raw_parts(msg, len) };

    let mut msg: Vec<u8> = Vec::new();
    if msg.try_reserve(slice_msg.len()).is_err() {
        return false as u8;
    }

    msg.extend_from_slice(slice_msg);

    unsafe {
        (&*(instance as *const FileOperations))
            .tx
            .try_send(msg)
            .is_ok() as u8
    }
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
