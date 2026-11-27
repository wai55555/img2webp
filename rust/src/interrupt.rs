//! Cooperative interruption.
//!
//! A console control handler / signal sets a flag; workers and the dispatcher
//! poll it at job boundaries, frame boundaries and 1 MiB decompression units
//! (design spec §3.6, decision D-SIG). The second request skips the final
//! commit.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Interrupt {
    state: Arc<AtomicU8>,
}

pub const NONE: u8 = 0;
pub const REQUESTED: u8 = 1;
pub const SECOND: u8 = 2;

impl Interrupt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a request; returns true when this was the second request.
    pub fn request(&self) -> bool {
        match self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(match current {
                    NONE => REQUESTED,
                    _ => SECOND,
                })
            }) {
            Ok(NONE) => false,
            Ok(_) => true,
            Err(_) => true,
        }
    }

    pub fn requested(&self) -> bool {
        self.state.load(Ordering::SeqCst) != NONE
    }

    pub fn is_second(&self) -> bool {
        self.state.load(Ordering::SeqCst) == SECOND
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), Interrupted> {
        if self.requested() {
            Err(Interrupted)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

#[cfg(windows)]
pub fn install_handler(interrupt: &Interrupt) -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<Interrupt> = OnceLock::new();
    let _ = FLAG.set(interrupt.clone());
    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        if let Some(flag) = FLAG.get() {
            // Only set the flag; never allocate or lock inside the handler.
            flag.request();
            return 1;
        }
        0
    }
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handler), 1) != 0 }
}

#[cfg(unix)]
pub fn install_handler(interrupt: &Interrupt) -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<Interrupt> = OnceLock::new();
    let _ = FLAG.set(interrupt.clone());
    unsafe extern "C" fn handler(_signal: libc::c_int) {
        if let Some(flag) = FLAG.get() {
            flag.request();
        }
    }
    unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) != libc::SIG_ERR }
}
