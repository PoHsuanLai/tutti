//! `clap.posix-fd-support`, the plugin side (unix): a pipe the plugin owns,
//! whose read end it registers with the host on a latched command, and which a
//! test makes readable with [`tutti_test_plugin_fd_poke`]. `on_fd` drains it
//! (so a level-triggered descriptor stops being ready) and counts the call, so
//! "the descriptor became readable and the host called `on_fd`" is checkable
//! without a clock.

use std::ffi::{CStr, c_int, c_void};
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ, clap_host_posix_fd_support,
    clap_plugin_posix_fd_support, clap_posix_fd_flags,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

use crate::threading::host_ext;

static PIPE: Mutex<Option<(PipeReader, PipeWriter)>> = Mutex::new(None);
static REGISTERED: AtomicBool = AtomicBool::new(false);
static FIRES: AtomicU32 = AtomicU32::new(0);
static LAST_EVENTS: AtomicU32 = AtomicU32::new(0);

static POSIX_FD_SUPPORT: clap_plugin_posix_fd_support = clap_plugin_posix_fd_support {
    on_fd: Some(plugin_on_fd),
};

/// The extension for `id`, null for anything else.
pub(crate) fn get_extension(id: &CStr) -> *const c_void {
    if id == CLAP_EXT_POSIX_FD_SUPPORT {
        return &POSIX_FD_SUPPORT as *const _ as *const c_void;
    }
    std::ptr::null()
}

unsafe extern "C" fn plugin_on_fd(
    _plugin: *const clap_plugin,
    _fd: c_int,
    flags: clap_posix_fd_flags,
) {
    LAST_EVENTS.store(flags, Ordering::Release);
    if let Some((reader, _)) = PIPE.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        let mut sink = [0u8; 64];
        // Readable by contract of the call, so this does not block.
        let _ = reader.read(&mut sink);
    }
    FIRES.fetch_add(1, Ordering::AcqRel);
}

/// Makes a pipe and registers its read end for reading, with the host.
pub(crate) unsafe fn register(host: *const clap_host) {
    // SAFETY: `host` is the live `clap_host` the plugin was given.
    let Some(ext) =
        (unsafe { host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT) })
    else {
        return;
    };
    let Some(register) = ext.register_fd else {
        return;
    };
    let Ok((reader, writer)) = std::io::pipe() else {
        return;
    };
    let fd = reader.as_raw_fd();
    *PIPE.lock().unwrap_or_else(|p| p.into_inner()) = Some((reader, writer));
    // SAFETY: the host's own entry, with the host pointer it gave us.
    let ok = unsafe { register(host, fd, CLAP_POSIX_FD_READ) };
    REGISTERED.store(ok, Ordering::Release);
}

/// Unregisters the pipe's read end.
pub(crate) unsafe fn unregister(host: *const clap_host) {
    // SAFETY: `host` is the live `clap_host` the plugin was given.
    let Some(ext) =
        (unsafe { host_ext::<clap_host_posix_fd_support>(host, CLAP_EXT_POSIX_FD_SUPPORT) })
    else {
        return;
    };
    let Some(unregister) = ext.unregister_fd else {
        return;
    };
    let fd = PIPE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|(reader, _)| reader.as_raw_fd());
    if let Some(fd) = fd {
        // SAFETY: the host's own entry.
        let ok = unsafe { unregister(host, fd) };
        REGISTERED.store(!ok, Ordering::Release);
    }
}

/// Whether the host accepted the registration, how many times `on_fd` ran, and
/// the events it was last told.
pub(crate) fn snapshot() -> (bool, u32, u32) {
    (
        REGISTERED.load(Ordering::Acquire),
        FIRES.load(Ordering::Acquire),
        LAST_EVENTS.load(Ordering::Acquire),
    )
}

/// Clears the counters. The pipe and its registration stay.
pub(crate) fn reset() {
    FIRES.store(0, Ordering::Release);
    LAST_EVENTS.store(0, Ordering::Release);
}

/// Writes a byte to the registered pipe, making its read end readable. `false`
/// when nothing is registered.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_test_plugin_fd_poke() -> bool {
    PIPE.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_mut()
        .is_some_and(|(_, writer)| writer.write_all(&[1]).is_ok())
}
