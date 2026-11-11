//! The slab's shared memory: the file, the mapping, and the only copies in and
//! out of it.
//!
//! The mapping is held as a raw base pointer and its length, and is reached
//! only through [`SharedMapping::header`] (a shared reference to the atomics at
//! offset 0), [`SharedMapping::read`] and [`MappingWriter::write`]. No reference
//! to the data region is ever formed: another process writes it while we hold
//! the mapping, so a `&[u8]` or `&mut [u8]` over it would assert an exclusivity
//! or immutability that no Rust type can see, let alone keep.
//!
//! A mapping exists only together with its one [`MappingWriter`] (not `Clone`,
//! `write` takes `&mut self`), so within this process every store into the data
//! region is ordered by the borrow checker. That, and the file being made here
//! (exclusively, at 0600) and never resized, is what lets every call in this
//! module be safe.

use crate::SlabError;
use crate::header::{SLAB_HEADER_BYTES, SlabHeader};
use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;

/// A mapped slab: a [`SlabHeader`] at offset 0, then the data region.
///
/// Shared (in an `Arc`) by everything that reads the slab; the one writer is
/// the [`MappingWriter`] it was made with.
#[derive(Debug)]
pub struct SharedMapping {
    /// Owns the mapping and unmaps it on drop. Never touched after `new`:
    /// every access goes through `base`, so no borrow of it exists that the
    /// raw copies could alias.
    _map: MmapMut,
    /// The mapping's first byte. Page-aligned.
    base: NonNull<u8>,
    /// The mapping's length in bytes, at least [`SLAB_HEADER_BYTES`].
    len: usize,
}

// SAFETY: `SharedMapping` is a pointer to memory it owns (through `_map`) for as
// long as it exists, so sending it to another thread moves nothing a thread
// could still be using. The pointer is the only reason the auto trait is
// missing; `MmapMut` itself is `Send`.
unsafe impl Send for SharedMapping {}

// SAFETY: sharing `&SharedMapping` between threads gives each of them `header`
// and `read`. `header` hands out `&SlabHeader`, whose fields are all atomics, so
// concurrent use is what they are for. `read` copies through raw pointers and
// never forms a reference to the data region, so no Rust aliasing rule is in
// play for those bytes; what remains is the data-race question:
//
// - *Within this process*, the only stores into the data region are
//   `MappingWriter::write`, and a mapping has exactly one writer (made with it
//   in `create` or `open`, not `Clone`, `write` takes `&mut self`).
// - *Across processes*, the other side writes the other direction's region
//   (disjoint offsets, decided by `tutti-plugin-ipc`'s layout), and a reader of
//   a region the peer writes copies only between two `Acquire` loads of the
//   slot's sequence that both name the block it wants. A copy that overlapped
//   the peer's rewrite of the slot is discarded, never used.
//
// What is NOT claimed: that a misbehaving peer cannot scribble on the region,
// or that the copy of a slot the peer is rewriting is well-defined in the
// abstract machine (it is the seqlock pattern, which Rust's model does not yet
// express). The bytes copied are plain bytes into our own buffer, so a corrupt
// or racing peer yields wrong audio, which the recheck then discards, never an
// invalid value on this side.
unsafe impl Sync for SharedMapping {}

/// The one writer of a [`SharedMapping`]'s data region in this process.
///
/// Made only with its mapping ([`create`](Self::create), [`open`](Self::open)),
/// and not `Clone`:
///
/// ```compile_fail
/// fn second(w: &tutti_plugin_ipc_ffi::MappingWriter) -> tutti_plugin_ipc_ffi::MappingWriter {
///     w.clone()
/// }
/// ```
#[derive(Debug)]
pub struct MappingWriter {
    mapping: Arc<SharedMapping>,
}

/// Whether [`open_file`] creates the backing file or opens the peer's.
enum Open {
    Create,
    Existing,
}

impl MappingWriter {
    /// Creates the slab named `name`, `size` bytes long, maps it and stamps its
    /// header, before any peer can know the name.
    ///
    /// The file is created exclusively, at 0600: a name that already exists (a
    /// symlink included) is an error, never a file to truncate, since another
    /// process could hold that file mapped, or have planted it. The creator
    /// removes it with [`remove`].
    ///
    /// # Errors
    ///
    /// [`SlabError::File`] if the file exists or cannot be created, sized or
    /// mapped.
    ///
    /// # Panics
    ///
    /// If `size` is smaller than [`SLAB_HEADER_BYTES`]: the caller sizes the
    /// slab from a layout that always includes the header.
    pub fn create(name: &str, size: usize) -> Result<Self, SlabError> {
        let writer = Self::map(open_file(name, size, Open::Create)?)?;
        // Stamp before anyone can open it: the peer is told the slab's name only
        // after this returns, and `initialize` releases the magic last so an
        // opener that sees the magic sees a fully written header.
        writer.mapping.header().initialize();
        Ok(writer)
    }

    /// Opens the peer's slab named `name`, checking that it is at least `size`
    /// bytes and a tutti slab of this build's header.
    ///
    /// On unix a symlink at the name is refused (`O_NOFOLLOW`).
    ///
    /// # Errors
    ///
    /// [`SlabError::File`] if it cannot be opened or mapped,
    /// [`SlabError::Short`] if it is shorter than `size`, and
    /// [`SlabError::Header`] if it is not a tutti slab of this header version
    /// and ring depth.
    ///
    /// # Panics
    ///
    /// As for [`create`](Self::create).
    pub fn open(name: &str, size: usize) -> Result<Self, SlabError> {
        let file = open_file(name, size, Open::Existing)?;
        let actual = file
            .metadata()
            .map_err(|e| SlabError::File(format!("failed to read shared memory size: {e}")))?
            .len();
        // Length first: `header()` asserts on a short mapping, and an assert is
        // the wrong failure mode for "the peer created a slab of another shape".
        if actual < size as u64 {
            return Err(SlabError::Short {
                actual,
                expected: size,
            });
        }
        let writer = Self::map(file)?;
        writer.mapping.header().validate()?;
        Ok(writer)
    }

    fn map(file: File) -> Result<Self, SlabError> {
        // SAFETY: `memmap2`'s contract is that the file is not truncated or
        // resized by anyone while mapped. `open_file` is the only code that makes
        // or opens a slab file: it creates one exclusively at 0600 and sizes it
        // before this map, and opens the peer's without `truncate` and never
        // sizes it. A process with write access to the file could still shrink
        // it; that is a fault of a trusted peer, as is scribbling on it (the
        // `Sync` argument above).
        let map = unsafe { MmapMut::map_mut(&file) }
            .map_err(|e| SlabError::File(format!("failed to map shared memory: {e}")))?;
        Ok(Self {
            mapping: Arc::new(SharedMapping::new(map)),
        })
    }

    /// The mapping, for the readers that do not write.
    #[must_use]
    pub fn mapping(&self) -> &Arc<SharedMapping> {
        &self.mapping
    }

    /// Copies `src` into the mapping at byte `offset`.
    ///
    /// # Panics
    ///
    /// If the range reaches into the header or past the mapping. The caller's
    /// offsets come from its layout, so this is the last line of defence, not
    /// the check callers rely on.
    #[inline]
    pub fn write(&mut self, offset: usize, src: &[u8]) {
        let m = &self.mapping;
        let end = m.check_range(offset, src.len());
        debug_assert!(end <= m.len);
        // SAFETY: `check_range` proves `offset..end` lies in the mapping and
        // after the header. `src` is our own buffer, never the mapping, so the
        // two ranges cannot overlap. No reference to these bytes exists (the
        // module's rule), and `&mut self` on the mapping's one writer orders
        // every store this process makes (the `Sync` argument).
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), m.base.as_ptr().add(offset), src.len());
        }
    }
}

impl SharedMapping {
    /// Takes ownership of `map`.
    ///
    /// # Panics
    ///
    /// If the mapping is smaller than the header.
    fn new(mut map: MmapMut) -> Self {
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

    /// The mapping's length in bytes, header included.
    #[must_use]
    #[allow(
        clippy::len_without_is_empty,
        reason = "a mapping always holds its header"
    )]
    pub fn len(&self) -> usize {
        self.len
    }

    /// The header at offset 0 of the mapping.
    ///
    /// A shared reference even though callers mutate through it: every field is
    /// an atomic. `write` refuses an offset inside the header, so no copy
    /// ever stores through these bytes while this reference lives.
    #[inline]
    #[must_use]
    pub fn header(&self) -> &SlabHeader {
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

    /// Copies `dst.len()` bytes out of the mapping from byte `offset`.
    ///
    /// The bytes may be mid-rewrite by the peer: a caller decides whether they
    /// are a published block by the slot's sequence, before and after.
    ///
    /// # Panics
    ///
    /// As for [`MappingWriter::write`].
    #[inline]
    pub fn read(&self, offset: usize, dst: &mut [u8]) {
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

/// Removes the backing file of the slab named `name`, as its creator does when
/// it is done with it. A peer that still has it mapped keeps its mapping.
pub fn remove(name: &str) {
    let _ = std::fs::remove_file(path(name));
}

fn open_file(name: &str, size: usize, mode: Open) -> Result<File, SlabError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match mode {
        Open::Create => {
            // Exclusive (`O_CREAT | O_EXCL`): an existing name, a symlink
            // included, is refused. Truncating one would shrink a file that
            // another process may have mapped, which faults it (SIGBUS), and a
            // planted file or link would put our audio where its owner chose.
            opts.create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
        }
        Open::Existing => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NOFOLLOW);
            }
        }
    }
    let file = opts
        .open(path(name))
        .map_err(|e| SlabError::File(format!("failed to open shared memory file: {e}")))?;
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|e| SlabError::File(format!("failed to set file size: {e}")))?;
    }
    Ok(file)
}

#[cfg(unix)]
fn path(name: &str) -> PathBuf {
    #[cfg(target_os = "linux")]
    let base = PathBuf::from("/dev/shm");
    #[cfg(target_os = "macos")]
    let base = std::env::temp_dir();
    base.join(format!("tutti_{name}"))
}

#[cfg(windows)]
fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::SLAB_MAGIC;

    fn name(tag: &str) -> String {
        format!("ffi_{tag}_{}", std::process::id())
    }

    /// The mapping's copies refuse the header's bytes: the header is only ever
    /// stored through its atomics, while `header()` hands out a shared
    /// reference to them.
    #[test]
    #[should_panic(expected = "outside the data region")]
    fn a_copy_into_the_header_panics() {
        let n = name("hdr");
        let mut w = MappingWriter::create(&n, SLAB_HEADER_BYTES + 64).unwrap();
        remove(&n);
        w.write(0, &[0u8; 8]);
    }

    /// The magic is the first thing in the mapping: the opening side keys on it
    /// before trusting a single byte, so `repr(C)` and its field order are
    /// load-bearing. Read from the file, not through the header type.
    #[test]
    fn the_magic_sits_at_offset_zero() {
        let n = name("magic");
        let _w = MappingWriter::create(&n, SLAB_HEADER_BYTES + 64).unwrap();
        let bytes = std::fs::read(path(&n)).unwrap();
        remove(&n);
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            SLAB_MAGIC
        );
    }

    /// An opener reads back what the creator wrote, and refuses a file shorter
    /// than the size it asked for.
    #[test]
    fn open_sees_the_creators_bytes_and_refuses_a_short_file() {
        let n = name("open");
        let size = SLAB_HEADER_BYTES + 16;
        let mut w = MappingWriter::create(&n, size).unwrap();
        w.write(SLAB_HEADER_BYTES, &[7u8; 16]);
        let peer = MappingWriter::open(&n, size).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [7u8; 16]);
        let err = MappingWriter::open(&n, size + 1).expect_err("a short slab");
        remove(&n);
        assert!(matches!(err, SlabError::Short { .. }), "got {err}");
    }

    /// A symlink planted at the name is refused by both ends: `create` does
    /// not follow it into the file it names, and `open` (`O_NOFOLLOW`) does not
    /// map whatever it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_name_is_refused() {
        let size = SLAB_HEADER_BYTES + 64;
        let real = name("real");
        let _owner = MappingWriter::create(&real, size).unwrap();
        let link = name("link");
        std::os::unix::fs::symlink(path(&real), path(&link)).unwrap();
        let opened = MappingWriter::open(&link, size);
        let created = MappingWriter::create(&link, size);
        remove(&link);
        remove(&real);
        assert!(opened.is_err(), "open followed a link");
        assert!(created.is_err(), "create followed a link");
    }

    /// Opening a file that is not a slab must fail on the magic, not produce a
    /// mapping full of someone else's bytes read as audio.
    #[test]
    fn opening_a_foreign_file_is_refused() {
        let size = SLAB_HEADER_BYTES + 64;
        let n = name("foreign");
        std::fs::write(path(&n), vec![0xABu8; size]).unwrap();
        let err = MappingWriter::open(&n, size).expect_err("a foreign file must not open");
        remove(&n);
        assert!(
            err.to_string().contains("not a tutti audio slab"),
            "expected a magic failure, got: {err}"
        );
    }
}
