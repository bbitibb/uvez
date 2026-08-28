use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

struct DebugState {
    enabled: bool,
    sink: Option<File>,
}

static DEBUG_STATE: OnceLock<DebugState> = OnceLock::new();

fn state() -> &'static DebugState {
    DEBUG_STATE.get_or_init(|| {
        let enabled = cfg!(debug_assertions)
            || std::env::var_os("UVEZ_DEBUG").is_some_and(|value| value != OsStr::new("0"));
        let sink = if enabled && !cfg!(debug_assertions) {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(log_path())
                .map_err(|error| eprintln!("Could not open the Uvez debug log: {error}"))
                .ok()
        } else {
            None
        };

        DebugState { enabled, sink }
    })
}

fn log_path() -> PathBuf {
    std::env::temp_dir().join("uvez-debug.log")
}

pub(crate) fn debug_enabled() -> bool {
    state().enabled
}

pub(crate) fn write_debug_line(message: &str) {
    let state = state();

    match &state.sink {
        Some(file) => {
            let mut handle = file;
            let _ = writeln!(handle, "{message}");
        }
        None => eprintln!("{message}"),
    }
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::logging::debug_enabled() {
            $crate::logging::write_debug_line(&format!($($arg)*));
        }
    };
}
