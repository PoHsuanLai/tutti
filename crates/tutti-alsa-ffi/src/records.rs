//! alsa-lib's opaque info records, each owned and freed on drop, so no call
//! site frees by hand or leaks on an early return.

use std::ptr::NonNull;

use crate::error::{Result, check};
use crate::sys;

/// A `snd_seq_port_info_t`.
pub(crate) struct PortRecord(NonNull<sys::snd_seq_port_info_t>);

impl PortRecord {
    pub(crate) fn new() -> Result<Self> {
        let mut ptr: *mut sys::snd_seq_port_info_t = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let rc = unsafe { sys::snd_seq_port_info_malloc(&mut ptr) };
        check(rc, "snd_seq_port_info_malloc")?;
        NonNull::new(ptr).map(Self).ok_or(crate::Error {
            operation: "snd_seq_port_info_malloc",
            code: -12,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::snd_seq_port_info_t {
        self.0.as_ptr()
    }
}

impl Drop for PortRecord {
    fn drop(&mut self) {
        // SAFETY: allocated by `new` and freed here, once.
        unsafe { sys::snd_seq_port_info_free(self.0.as_ptr()) };
    }
}

/// A `snd_seq_client_info_t`.
pub(crate) struct ClientRecord(NonNull<sys::snd_seq_client_info_t>);

impl ClientRecord {
    pub(crate) fn new() -> Result<Self> {
        let mut ptr: *mut sys::snd_seq_client_info_t = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let rc = unsafe { sys::snd_seq_client_info_malloc(&mut ptr) };
        check(rc, "snd_seq_client_info_malloc")?;
        NonNull::new(ptr).map(Self).ok_or(crate::Error {
            operation: "snd_seq_client_info_malloc",
            code: -12,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::snd_seq_client_info_t {
        self.0.as_ptr()
    }
}

impl Drop for ClientRecord {
    fn drop(&mut self) {
        // SAFETY: allocated by `new` and freed here, once.
        unsafe { sys::snd_seq_client_info_free(self.0.as_ptr()) };
    }
}

/// A `snd_ump_block_info_t`.
pub(crate) struct BlockRecord(NonNull<sys::snd_ump_block_info_t>);

impl BlockRecord {
    pub(crate) fn new() -> Result<Self> {
        let mut ptr: *mut sys::snd_ump_block_info_t = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let rc = unsafe { sys::snd_ump_block_info_malloc(&mut ptr) };
        check(rc, "snd_ump_block_info_malloc")?;
        NonNull::new(ptr).map(Self).ok_or(crate::Error {
            operation: "snd_ump_block_info_malloc",
            code: -12,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::snd_ump_block_info_t {
        self.0.as_ptr()
    }
}

impl Drop for BlockRecord {
    fn drop(&mut self) {
        // SAFETY: allocated by `new` and freed here, once.
        unsafe { sys::snd_ump_block_info_free(self.0.as_ptr()) };
    }
}

/// A `snd_ump_endpoint_info_t`.
pub(crate) struct EndpointRecord(NonNull<sys::snd_ump_endpoint_info_t>);

impl EndpointRecord {
    pub(crate) fn new() -> Result<Self> {
        let mut ptr: *mut sys::snd_ump_endpoint_info_t = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let rc = unsafe { sys::snd_ump_endpoint_info_malloc(&mut ptr) };
        check(rc, "snd_ump_endpoint_info_malloc")?;
        NonNull::new(ptr).map(Self).ok_or(crate::Error {
            operation: "snd_ump_endpoint_info_malloc",
            code: -12,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::snd_ump_endpoint_info_t {
        self.0.as_ptr()
    }
}

impl Drop for EndpointRecord {
    fn drop(&mut self) {
        // SAFETY: allocated by `new` and freed here, once.
        unsafe { sys::snd_ump_endpoint_info_free(self.0.as_ptr()) };
    }
}

/// A `snd_seq_queue_status_t`.
pub(crate) struct StatusRecord(NonNull<sys::snd_seq_queue_status_t>);

// SAFETY: the record is plain memory alsa-lib fills on a call; it is owned by
// this value alone and used through `&mut self` or `&self` of its one owner.
unsafe impl Send for StatusRecord {}

impl std::fmt::Debug for StatusRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusRecord").finish_non_exhaustive()
    }
}

impl StatusRecord {
    pub(crate) fn new() -> Result<Self> {
        let mut ptr: *mut sys::snd_seq_queue_status_t = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let rc = unsafe { sys::snd_seq_queue_status_malloc(&mut ptr) };
        check(rc, "snd_seq_queue_status_malloc")?;
        NonNull::new(ptr).map(Self).ok_or(crate::Error {
            operation: "snd_seq_queue_status_malloc",
            code: -12,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::snd_seq_queue_status_t {
        self.0.as_ptr()
    }
}

impl Drop for StatusRecord {
    fn drop(&mut self) {
        // SAFETY: allocated by `new` and freed here, once.
        unsafe { sys::snd_seq_queue_status_free(self.0.as_ptr()) };
    }
}
