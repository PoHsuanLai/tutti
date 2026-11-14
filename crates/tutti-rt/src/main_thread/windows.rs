//! Windows: the loader runs a CRT initializer on the process's primary thread
//! before `main`, and it records that thread's id (the method winit uses).
//! Windows has no "is the main thread" call; the id recorded at startup is the
//! primary thread's for the life of the process, even once it has exited.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::System::Threading::GetCurrentThreadId;

/// The primary thread's id, written once by [`init`] (0 until it has run; no
/// thread has id 0).
static PRIMARY: AtomicU32 = AtomicU32::new(0);

/// Runs on the primary thread, from the `.CRT$XCU` table, before `main`.
extern "C" fn init() {
    // SAFETY: `GetCurrentThreadId` takes no arguments and cannot fail.
    PRIMARY.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
}

// SAFETY (link section): `.CRT$XCU` is the table of C initializers the MSVC
// runtime calls at startup; an `extern "C" fn()` entry is what it expects, and
// `#[used]` keeps the linker from dropping it.
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static INIT: extern "C" fn() = init;

pub(super) fn is_main_thread() -> bool {
    let primary = PRIMARY.load(Ordering::Relaxed);
    // SAFETY: as above.
    primary != 0 && primary == unsafe { GetCurrentThreadId() }
}
