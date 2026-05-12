#[macro_export]
macro_rules! sozo_debug {
    ($tag:expr, $($arg:tt)*) => {
        #[cfg(debug_assertions)]
        eprintln!("[SOZO::{}] {}", $tag, format_args!($($arg)*));
    };
}
