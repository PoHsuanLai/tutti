//! The single `unsafe` concentrator for the shared-memory transport.
//!
//! Gives `&self` methods a `&mut [u8]` view into an mmap region, plus a typed
//! view of the [`SlabHeader`] at offset 0. Every raw-pointer operation in the
//! transport lives here so the layers above are ordinary safe Rust.

use super::header::SlabHeader;
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

/// Interior-mutable mmap region, prefixed by a [`SlabHeader`].
pub(super) struct MmapCell(UnsafeCell<MmapMut>);

// SAFETY: this is cross-process shared memory, so `Sync` cannot mean what it
// usually does — the other writer is in a different address space and no Rust
// type can see it. What makes concurrent access sound is the header's
// Release/Acquire discipline, and it is worth being precise about what that
// buys, because the previous justification here ("single-writer enforced by IPC
// protocol") named a guarantee that did not exist and a bypass shipped under it.
//
// Two claims, separately:
//
// 1. *No torn reads of published audio.* A reader only copies a region after an
//    `Acquire` load of that slot's sequence returns the block it wants, and the
//    writer only stores that sequence with `Release` after the last sample is in
//    place. So a matching sequence proves every sample write happens-before the
//    read. A non-matching one means the reader substitutes silence and touches
//    nothing. See `header.rs`.
//
// 2. *No two writers to one byte.* Each direction has its own region and its own
//    sequence array — the host writes only inputs, the server only outputs — so
//    the two processes never write the same address. This is structural now, not
//    conventional: the aliased single-bus layout that let the host's input write
//    land in the output region has been removed.
//
// What is NOT claimed: that a *misbehaving or crashed* peer cannot scribble on
// the region. Nothing short of a separate mapping per direction could give that,
// and a plugin subprocess is trusted code by construction. A corrupt peer
// produces wrong audio, not memory unsafety on this side, because every read is
// a `memcpy` of `Copy` scalars into a bounds-checked slice.
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

    /// The header at offset 0 of the mapping.
    ///
    /// Returns a shared reference even though callers mutate through it: every
    /// field is an atomic, so there is no `&mut` aliasing question and no reason
    /// to hand out exclusive access that two processes could not honour anyway.
    ///
    /// # Panics
    ///
    /// If the mapping is smaller than the header. That is a programming error in
    /// slab construction, not a runtime condition — both sides size the mapping
    /// from the same `SlabLayout::byte_size`, which always includes the header.
    #[inline]
    pub(super) fn header(&self) -> &SlabHeader {
        let bytes = self.as_slice();
        assert!(
            bytes.len() >= std::mem::size_of::<SlabHeader>(),
            "slab mapping is smaller than its own header"
        );
        // SAFETY: three conditions, each independently established.
        //
        // *Alignment*: an mmap base address is page-aligned (4 KiB or more),
        // which exceeds `SlabHeader`'s alignment — it is `CachePadded`, so at
        // most 128 bytes. The header sits at offset 0, so the pointer inherits
        // that alignment.
        //
        // *Size*: asserted above.
        //
        // *Validity*: `SlabHeader` is `repr(C)` and contains only atomic
        // integers, which have no invalid bit patterns and no niches. Every byte
        // sequence of the right length is therefore a valid instance —
        // including the all-zeros of a freshly created mapping, which is exactly
        // the state `initialize` expects to find. `validate` is what decides
        // whether the contents are *meaningful*; this cast only needs them to be
        // well-formed.
        unsafe { &*(bytes.as_ptr() as *const SlabHeader) }
    }
}
