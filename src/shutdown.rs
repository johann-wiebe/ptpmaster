//! Ctrl+C / Konsolen-Schließen → Stopp-Flag.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Installiert einen Handler für Ctrl+C (und unter Windows Ctrl+Break/Fenster schließen)
/// und liefert das Flag, das dann gesetzt wird.
pub fn install_handler() -> Arc<AtomicBool> {
    let flag = FLAG
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    platform::install();
    flag
}

fn trigger() {
    if let Some(f) = FLAG.get() {
        f.store(true, Ordering::SeqCst);
    }
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::{FALSE, TRUE};
    use windows::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };
    use windows::core::BOOL;

    unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
        if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT || ctrl == CTRL_CLOSE_EVENT {
            super::trigger();
            TRUE
        } else {
            FALSE
        }
    }

    pub fn install() {
        // SAFETY: Handler ist eine statische Funktion ohne Zustand.
        let _ = unsafe { SetConsoleCtrlHandler(Some(handler), true) };
    }
}

#[cfg(unix)]
mod platform {
    extern "C" fn handler(_: libc::c_int) {
        super::trigger();
    }

    pub fn install() {
        // SAFETY: Handler setzt nur ein atomares Flag (async-signal-sicher).
        unsafe {
            libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        }
    }
}
