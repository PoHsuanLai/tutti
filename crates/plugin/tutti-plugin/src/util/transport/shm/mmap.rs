//! The single `unsafe` concentrator for the shared-memory transport.
//!
//! Gives `&self` methods a `&mut [u8]` view into an mmap region. Caller
//! must ensure single-writer access; the IPC handshake enforces that.

use memmap2::MmapMut;
use std::cell::UnsafeCell;

/// View a `&[T]` as `&[u8]` for `Copy` types.
#[inline]
pub(super) fn as_bytes<T: Copy>(slice: &[T]) -> &[u8] {
    // SAFETY: T is Copy (no drop glue), pointer is valid, length is correct.
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, std::mem::size_of_val(slice)) }
}

/// View a `&mut [T]` as `&mut [u8]` for `Copy` types.
#[inline]
pub(super) fn as_bytes_mut<T: Copy>(slice: &mut [T]) -> &mut [u8] {
    // SAFETY: T is Copy (no drop glue), pointer is valid, length is correct.
    unsafe {
        std::slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u8, std::mem::size_of_val(slice))
    }
}

/// Interior-mutable mmap region. Single-writer invariant is upheld by the
/// IPC handshake in the layer above.
pub(super) struct MmapCell(UnsafeCell<MmapMut>);

// SAFETY: cross-process shared memory; single-writer enforced by IPC protocol.
unsafe impl Sync for MmapCell {}

impl MmapCell {
    pub(super) fn new(mmap: MmapMut) -> Self {
        Self(UnsafeCell::new(mmap))
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(super) fn as_mut_slice(&self) -> &mut [u8] {
        unsafe { &mut *self.0.get() }
    }

    #[inline]
    pub(super) fn as_slice(&self) -> &[u8] {
        unsafe { &*self.0.get() }
    }
}
