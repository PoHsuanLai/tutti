//! `clap.posix-fd-support` is POSIX's: where there are no descriptors the plugin
//! offers no extension and registers none.

use std::ffi::{CStr, c_void};

use clap_sys::host::clap_host;

pub(crate) fn get_extension(_id: &CStr) -> *const c_void {
    std::ptr::null()
}

pub(crate) unsafe fn register(_host: *const clap_host) {}

pub(crate) unsafe fn unregister(_host: *const clap_host) {}

pub(crate) fn snapshot() -> (bool, u32, u32) {
    (false, 0, 0)
}

pub(crate) fn reset() {}

/// Nothing is registered.
#[unsafe(no_mangle)]
pub extern "C" fn tutti_test_plugin_fd_poke() -> bool {
    false
}
