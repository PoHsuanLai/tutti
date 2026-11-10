//! The single `unsafe` concentrator for the shared-memory transport.
//!
//! The mapping is held as a raw base pointer and its length, and is reached
//! only through [`MmapCell::header`] (a shared reference to the atomics at
//! offset 0) and the two copies [`MmapCell::write`] and [`MmapCell::read`].
//! No reference to the data region is ever formed: another process writes it
//! while we hold the mapping, so a `&[u8]` or `&mut [u8]` over it would assert
//! an exclusivity or immutability that no Rust type can see, let alone keep.
//! Every raw-pointer operation in the transport lives here so the layers above
//! are ordinary safe Rust.

use super::header::{SLAB_HEADER_BYTES, SlabHeader};
use memmap2::MmapMut;
use std::ptr::NonNull;

/// A mapped slab: a [`SlabHeader`] at offset 0, then the data region.
#[derive(Debug)]
pub(super) struct MmapCell {
    /// Owns the mapping and unmaps it on drop. Never touched after `new`:
    /// every access goes through `base`, so no borrow of it exists that the
    /// raw copies could alias.
    _map: MmapMut,
    /// The mapping's first byte. Page-aligned.
    base: NonNull<u8>,
    /// The mapping's length in bytes, at least [`SLAB_HEADER_BYTES`].
    len: usize,
}

// SAFETY: `MmapCell` is a pointer to memory it owns (through `_map`) for as
// long as it exists, so sending it to another thread moves nothing a thread
// could still be using. The pointer is the only reason the auto trait is
// missing; `MmapMut` itself is `Send`.
unsafe impl Send for MmapCell {}

// SAFETY: sharing `&MmapCell` between threads gives each of them `header`,
// `read` and `write`. `header` hands out `&SlabHeader`, whose fields are all
// atomics, so concurrent use is what they are for. `read` and `write` copy
// through raw pointers and never form a reference to the data region, so no
// Rust aliasing rule is in play for those bytes; what remains is the data-race
// question, and it is answered one layer up, not here:
//
// - *Within this process*, at most one writer exists per direction. A
//   direction's writes go only through its `SlabWriter`, which is not `Clone`
//   and takes `&mut self`; `AudioSlab` (shared, in an `Arc`) has no writer
//   method. `write` is `pub(super)` so nothing outside the module can reach it
//   around that rule.
// - *Across processes*, the other side writes the other direction's region
//   (disjoint offsets, `slab.rs`), and a reader of a region the peer writes
//   copies only between two `Acquire` loads of the slot's sequence that both
//   name the block it wants (`AudioSlab::read_region`). A copy that overlapped
//   the peer's rewrite of the slot is discarded, never used.
//
// What is NOT claimed: that a misbehaving peer cannot scribble on the region,
// or that the copy of a slot the peer is rewriting is well-defined in the
// abstract machine (it is the seqlock pattern, which Rust's model does not yet
// express). The bytes copied are `Pod` scalars into our own buffer, so a
// corrupt or racing peer yields wrong audio, which the recheck then discards,
// never an invalid value on this side.
unsafe impl Sync for MmapCell {}

impl MmapCell {
    /// Takes ownership of `map`.
    ///
    /// # Panics
    ///
    /// If the mapping is smaller than the header. That is a programming error
    /// in slab construction, not a runtime condition: both sides size the
    /// mapping from the same `SlabLayout::byte_size_with_header`, which always
    /// includes the header.
    pub(super) fn new(mut map: MmapMut) -> Self {
        let len = map.len();
        assert!(
            len >= SLAB_HEADER_BYTES,
            "slab mapping is smaller than its own header"
        );
        let base = NonNull::new(map.as_mut_ptr()).expect("a non-empty mapping has a base");
        Self {
            _map: map,
            base,
            len,
        }
    }

    /// The header at offset 0 of the mapping.
    ///
    /// A shared reference even though callers mutate through it: every field is
    /// an atomic. `write` refuses an offset inside the header, so no copy
    /// ever stores through these bytes while this reference lives.
    #[inline]
    pub(super) fn header(&self) -> &SlabHeader {
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "page-aligned mmap base; the alignment argument is below"
        )]
        let header = self.base.as_ptr().cast::<SlabHeader>();
        // SAFETY:
        // - *Alignment*: the header sits at offset 0 of a page-aligned mmap base,
        //   which exceeds `SlabHeader`'s alignment (`CachePadded`, so <= 128).
        // - *Size*: `new` asserts the mapping holds at least the header.
        // - *Validity*: `repr(C)` holding only atomic integers, with no invalid
        //   bit patterns and no niches, so any bytes of the right length are a
        //   valid instance, including a fresh mapping's zeros. `validate`
        //   decides whether the contents are *meaningful*; this cast only needs
        //   them well-formed.
        // - *Aliasing*: the header's bytes are only ever stored through its
        //   atomics (here or in the peer); `write` refuses them.
        // - *Lifetime*: tied to `&self`, and the mapping lives as long as `self`.
        unsafe { &*header }
    }

    /// Copies `src` into the mapping at byte `offset`.
    ///
    /// The caller is the one writer of the region (see the `Sync` argument
    /// above): `SlabWriter`, through `&mut self`.
    ///
    /// # Panics
    ///
    /// If the range reaches into the header or past the mapping. Offsets come
    /// from `AudioSlab::offset_of`, which checks them against the layout, so
    /// this is the last line of defence, not the check callers rely on.
    #[inline]
    pub(super) fn write(&self, offset: usize, src: &[u8]) {
        let end = self.check_range(offset, src.len());
        debug_assert!(end <= self.len);
        // SAFETY: `check_range` proves `offset..end` lies in the mapping and
        // after the header. `src` is our own buffer, never the mapping, so the
        // two ranges cannot overlap. No reference to these bytes exists (the
        // module's rule), and this process has one writer for them (`Sync`
        // argument above).
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.base.as_ptr().add(offset), src.len());
        }
    }

    /// Copies `dst.len()` bytes out of the mapping from byte `offset`.
    ///
    /// The bytes may be mid-rewrite by the peer: a caller decides whether they
    /// are a published block by the slot's sequence, before and after.
    ///
    /// # Panics
    ///
    /// As for [`write`](Self::write).
    #[inline]
    pub(super) fn read(&self, offset: usize, dst: &mut [u8]) {
        let end = self.check_range(offset, dst.len());
        debug_assert!(end <= self.len);
        // SAFETY: `check_range` proves `offset..end` lies in the mapping and
        // after the header. `dst` is our own buffer, never the mapping, so the
        // ranges cannot overlap. Any byte value is valid for `u8`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(offset),
                dst.as_mut_ptr(),
                dst.len(),
            );
        }
    }

    /// Every byte of the mapping, header included, for a test that checks the
    /// layout by address rather than through the code that computed it.
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; self.len];
        // SAFETY: `base..base + len` is the mapping, which `self` keeps alive.
        // A test calls this with no writer running, so the header's atomics
        // are not being stored to while their bytes are copied.
        unsafe { std::ptr::copy_nonoverlapping(self.base.as_ptr(), bytes.as_mut_ptr(), self.len) };
        bytes
    }

    /// `offset + len`, after checking the range is data, not header, and
    /// inside the mapping.
    fn check_range(&self, offset: usize, len: usize) -> usize {
        let end = offset.checked_add(len).expect("slab range overflows usize");
        assert!(
            offset >= SLAB_HEADER_BYTES && end <= self.len,
            "slab range {offset}..{end} is outside the data region {SLAB_HEADER_BYTES}..{}",
            self.len
        );
        end
    }
}
