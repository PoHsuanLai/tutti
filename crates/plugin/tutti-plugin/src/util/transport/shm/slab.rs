//! Named, cross-process audio storage: a header plus two independent rings of
//! `channels × samples_per_channel` samples, one per direction.
//!
//! Layout is **mono-planar** — one contiguous region per channel, addressed by
//! index — not interleaved, because the plugin ABIs it feeds are deinterleaved
//! and the memcpy must stay a straight per-channel copy with no transpose. That
//! is why it does not `impl` [`tutti_types::io`]'s `AudioIn`/`AudioOut`, which
//! move flat *interleaved* samples. The mismatch is layout, not width — those
//! traits carry a runtime `ChannelLayout`, not a const frame width.
//!
//! # Why shared memory
//!
//! Host and plugin are separate processes, so they share no memory by default,
//! and audio crosses the boundary every block on the audio thread — which must
//! never block, allocate, or enter the kernel. Over the control socket that
//! would be serialize + two syscalls + kernel copies per block.
//!
//! Instead both processes `mmap` the *same* named, RAM-backed file (`/dev/shm`
//! on Linux, a temp file elsewhere), so the kernel maps the same physical pages
//! into both and transfer is a plain `memcpy`.
//!
//! # Synchronization lives here, not above
//!
//! Delegating the single-writer invariant to a handshake "one layer up" fails
//! invisibly, because this layer cannot report the gap: a read of an untouched
//! region returns full length and plausible bytes. With both directions aliased
//! onto one offset, an unmatched read hands the host its own input back — a
//! silent bypass that measures as working audio.
//!
//! Validity is therefore answered *in the slab*, by `SlabHeader`'s per-slot
//! sequence numbers, and the two directions occupy disjoint regions:
//!
//! - A writer fills every channel of a slot, then calls `publish_*` **once**.
//! - A reader calls `*_sequence` first and only copies if it matches the block
//!   it wants; otherwise it substitutes silence.
//!
//! `read_*_into` still returns a copy count, but that count means only "how many
//! samples I copied" — **it is not evidence that anyone wrote them**. The
//! sequence number is. See `header.rs` for the Release/Acquire argument.
//!
//! Samples stay raw `f32`/`f64`, not unit newtypes: this is a C-ABI / IPC
//! boundary where the layout must be exactly the primitive's.
//!
//! # Lifecycle
//!
//! One side [`create`s](AudioSlab::create) the slab (the `Owner`, which
//! unlinks the backing file on drop) and stamps its header; the other
//! [`open`s](AudioSlab::open) it (a `View`) using a matching [`SlabLayout`]
//! and validates that header before trusting a byte. [`Clone`] reopens the same
//! backing file as a fresh view so a cloned audio node points at the same pages.
//!

use crate::error::{BridgeError, Result};
use crate::protocol::audio::Sample;
use crate::protocol::SlabLayout;
use memmap2::MmapMut;
use std::fs::OpenOptions;

use super::header::{slot_for, Direction, RING_SLOTS, SLAB_HEADER_BYTES};
use super::mmap::{as_bytes, as_bytes_mut, MmapCell};

/// Who is responsible for the backing file's lifetime.
enum Ownership {
    /// Created the slab; unlinks the backing file on drop.
    Owner,
    /// Opened an existing slab; detaches on drop without deleting it.
    View,
}

/// Named mmap region: a `SlabHeader`, then a ring per direction.
///
/// Create on one side, open on the other with a matching [`SlabLayout`].
/// The creator owns the backing file and unlinks it on drop; the opener
/// is a view.
pub struct AudioSlab {
    mmap: MmapCell,
    name: String,
    layout: SlabLayout,
    ownership: Ownership,
}

impl AudioSlab {
    // ---- Construction: one side creates, the other opens ----

    /// Create the slab: allocate the named backing file, size it to hold the
    /// header and both rings, map it, and stamp the header. This side is the
    /// `Owner` and unlinks the file on drop. Exactly one side calls this; the
    /// other calls [`open`](Self::open) with a matching `layout`.
    ///
    pub fn create(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        check_layout(&layout)?;
        let name = name.into();
        let mmap = open_mmap(&name, byte_size(&layout), Open::Create)?;
        let slab = Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::Owner,
        };
        // Stamp before anyone can open it: the peer is told the slab's name only
        // after this returns, and `initialize` releases the magic last so an
        // opener that sees the magic sees a fully written header.
        slab.mmap.header().initialize();
        Ok(slab)
    }

    /// Open an existing slab as a `View`, validating its header.
    ///
    /// The `layout` must match the one the `Owner` created it with — both
    /// sides agree on the shape out of band (over the control channel) before
    /// mapping. Unlike the previous version, which validated *nothing*, this
    /// rejects a file that is too short, is not a tutti slab, or was written by
    /// a build with a different header shape. Detaches on drop without deleting
    /// the backing file.
    ///
    pub fn open(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        check_layout(&layout)?;
        let name = name.into();
        let expected = byte_size(&layout);
        let mmap = open_mmap(&name, expected, Open::Existing)?;
        // Length first: `header()` asserts on a short mapping, and an assert is
        // the wrong failure mode for "the peer created a slab of another shape".
        if mmap.len() < expected {
            return Err(oob_owned(format!(
                "slab is {} bytes, layout needs {expected}",
                mmap.len()
            )));
        }
        let slab = Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::View,
        };
        slab.mmap
            .header()
            .validate()
            .map_err(BridgeError::SharedMemoryError)?;
        Ok(slab)
    }

    // ---- Accessors ----

    /// The slab's name, used by the other side to [`open`](Self::open) it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Clone the layout. For the allocation-free audio path, prefer
    /// [`layout_ref`](Self::layout_ref).
    pub fn layout(&self) -> SlabLayout {
        self.layout.clone()
    }

    /// Borrow the layout without cloning its (heap-backed) bus list — for the
    /// allocation-free audio path. [`layout`](Self::layout) (which clones) is
    /// for callers that need to own a copy.
    pub fn layout_ref(&self) -> &SlabLayout {
        &self.layout
    }

    // ---- Per-block transfer: the hot path ----
    //
    // Each is a bounds check plus a `memcpy` into/out of the mapped region — no
    // syscall, no allocation, safe on the audio thread. The `write_*` /
    // `publish_*` split is not a convenience: publishing must happen exactly
    // once, after the last channel, or a reader can observe a slot marked valid
    // while later channels are still being copied.

    /// Copy `data` into one channel of the input ring's slot for block `seq`.
    ///
    /// Host side. Call once per channel, then
    /// [`publish_input`](Self::publish_input) once.
    pub fn write_input<T: Sample>(&self, seq: u64, channel: usize, data: &[T]) -> Result<()> {
        self.write_region(Direction::Input, seq, channel, data)
    }

    /// Copy `data` into one channel of the output ring's slot for block `seq`.
    ///
    /// Server side. Call once per channel, then
    /// [`publish_output`](Self::publish_output) once.
    pub fn write_output<T: Sample>(&self, seq: u64, channel: usize, data: &[T]) -> Result<()> {
        self.write_region(Direction::Output, seq, channel, data)
    }

    /// Copy one channel of the input ring's slot for block `seq` into `output`.
    ///
    /// Server side. Returns how many samples were copied — **not** whether they
    /// are this block's. Check [`input_sequence`](Self::input_sequence) first.
    pub fn read_input_into<T: Sample>(
        &self,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize> {
        self.read_region(Direction::Input, seq, channel, output)
    }

    /// Copy one channel of the output ring's slot for block `seq` into `output`.
    ///
    /// Host side. Returns how many samples were copied — **not** whether they
    /// are this block's. Check [`output_sequence`](Self::output_sequence) first.
    pub fn read_output_into<T: Sample>(
        &self,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize> {
        self.read_region(Direction::Output, seq, channel, output)
    }

    /// Announce that block `seq`'s inputs are complete. Host side, once per
    /// block, after the last [`write_input`](Self::write_input).
    #[inline]
    pub fn publish_input(&self, seq: u64) {
        self.mmap
            .header()
            .publish(Direction::Input, slot_for(seq), seq);
    }

    /// Announce that block `seq`'s outputs are complete. Server side, once per
    /// block, after the last [`write_output`](Self::write_output).
    #[inline]
    pub fn publish_output(&self, seq: u64) {
        self.mmap
            .header()
            .publish(Direction::Output, slot_for(seq), seq);
    }

    /// Which block currently occupies the input slot that `seq` maps to.
    ///
    /// Equal to `seq` means "block `seq`'s inputs are there and complete".
    /// Anything else means the slot holds a different block — read nothing.
    #[inline]
    pub fn input_sequence(&self, seq: u64) -> u64 {
        self.mmap.header().sequence(Direction::Input, slot_for(seq))
    }

    /// Which block currently occupies the output slot that `seq` maps to.
    #[inline]
    pub fn output_sequence(&self, seq: u64) -> u64 {
        self.mmap
            .header()
            .sequence(Direction::Output, slot_for(seq))
    }

    /// True when the output ring holds block `seq` — the single check the host
    /// makes before reading a block back.
    #[inline]
    pub fn has_output(&self, seq: u64) -> bool {
        self.output_sequence(seq) == seq
    }

    /// True when the input ring holds block `seq` — the server's check before
    /// feeding the plugin.
    #[inline]
    pub fn has_input(&self, seq: u64) -> bool {
        self.input_sequence(seq) == seq
    }

    // ---- Internal helpers ----

    fn write_region<T: Sample>(
        &self,
        direction: Direction,
        seq: u64,
        channel: usize,
        data: &[T],
    ) -> Result<()> {
        let offset = self.offset_of(direction, seq, channel)?;
        if data.len() > self.layout.samples_per_channel {
            return Err(oob("data length exceeds buffer capacity"));
        }
        let bytes = as_bytes(data);
        self.mmap.as_mut_slice()[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn read_region<T: Sample>(
        &self,
        direction: Direction,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize> {
        let offset = self.offset_of(direction, seq, channel)?;
        let copy_samples = self.layout.samples_per_channel.min(output.len());
        let copy_bytes = copy_samples * std::mem::size_of::<T>();
        let src = &self.mmap.as_slice()[offset..offset + copy_bytes];
        as_bytes_mut(&mut output[..copy_samples]).copy_from_slice(src);
        Ok(copy_samples)
    }

    /// Byte offset of one channel of one slot of one direction's ring.
    ///
    /// Slot-major within each region, so a whole block's write stays
    /// sequential — one slot's channels are contiguous rather than strided
    /// across the ring.
    fn offset_of(&self, direction: Direction, seq: u64, channel: usize) -> Result<usize> {
        let channels = match direction {
            Direction::Input => self.layout.input_channels(),
            Direction::Output => self.layout.output_channels(),
        };
        if channel >= channels {
            return Err(oob("channel index out of bounds"));
        }
        let base = match direction {
            Direction::Input => SLAB_HEADER_BYTES,
            Direction::Output => SLAB_HEADER_BYTES + self.layout.input_ring_bytes(),
        };
        let stride = self.layout.samples_per_channel * self.layout.sample_size();
        Ok(base + (slot_for(seq) * channels + channel) * stride)
    }
}

/// Total mapping size for `layout`, header included.
fn byte_size(layout: &SlabLayout) -> usize {
    layout.byte_size_with_header(SLAB_HEADER_BYTES)
}

/// Reject a layout this build cannot address before it becomes a mapping.
///
/// Both conditions would otherwise be silently representable, and both produce
/// wrong audio rather than an error: an empty bus list reads as "single flat
/// range shared in place" (the aliasing bypass), and a mismatched slot count
/// indexes the wrong ring slot.
fn check_layout(layout: &SlabLayout) -> Result<()> {
    if layout.inputs.is_empty() || layout.outputs.is_empty() {
        return Err(oob(
            "slab layout must name at least one bus per direction; \
             an empty list would read as 'share one region in place', which is the bypass",
        ));
    }
    if layout.slots as usize != RING_SLOTS {
        return Err(oob_owned(format!(
            "slab layout asks for {} ring slots, this build addresses {RING_SLOTS}",
            layout.slots
        )));
    }
    Ok(())
}

impl Clone for AudioSlab {
    /// Reopen the same backing file as a fresh `View`, so a cloned audio
    /// node still points at the same physical pages. The clone never owns
    /// the file, regardless of this side's ownership.
    ///
    fn clone(&self) -> Self {
        Self::open(self.name.clone(), self.layout.clone())
            .expect("failed to reopen shared-memory slab for clone")
    }
}

impl Drop for AudioSlab {
    /// Only the `Owner` unlinks the backing file; `View`s just detach.
    ///
    fn drop(&mut self) {
        if matches!(self.ownership, Ownership::Owner) {
            let _ = std::fs::remove_file(shm_path(&self.name));
        }
    }
}

fn oob(msg: &'static str) -> BridgeError {
    BridgeError::SharedMemoryError(msg.into())
}

fn oob_owned(msg: String) -> BridgeError {
    BridgeError::SharedMemoryError(msg)
}

enum Open {
    Create,
    Existing,
}

fn open_mmap(name: &str, size: usize, mode: Open) -> Result<MmapMut> {
    let path = shm_path(name);
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match mode {
        Open::Create => {
            opts.create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
        }
        Open::Existing => {}
    }
    let file = opts.open(&path).map_err(|e| {
        BridgeError::SharedMemoryError(format!("failed to open shared memory file: {e}"))
    })?;
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|e| BridgeError::SharedMemoryError(format!("failed to set file size: {e}")))?;
    }
    unsafe { MmapMut::map_mut(&file) }
        .map_err(|e| BridgeError::SharedMemoryError(format!("failed to map shared memory: {e}")))
}

#[cfg(unix)]
fn shm_path(name: &str) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    let base = std::path::PathBuf::from("/dev/shm");
    #[cfg(target_os = "macos")]
    let base = std::env::temp_dir();
    base.join(format!("tutti_{name}"))
}

#[cfg(windows)]
fn shm_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChannelLayout, SampleFormat};
    use smallvec::SmallVec;

    /// A real two-bus-per-direction layout. Deliberately *asymmetric* (3 in, 2
    /// out): built at base 0 with equal widths, an offset error that swapped the
    /// directions or dropped the slot term still produces matching bytes. With
    /// different widths and a non-zero output base, those mistakes cannot
    /// round-trip.
    fn layout(samples: usize, format: SampleFormat) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: SmallVec::from_slice(&[ChannelLayout::STEREO, ChannelLayout::MONO]),
            outputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
        }
    }

    fn stereo_layout(samples: usize, format: SampleFormat) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
            outputs: SmallVec::from_slice(&[ChannelLayout::STEREO]),
        }
    }

    fn name(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "slab_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn roundtrip_f32() {
        let l = layout(128, SampleFormat::Float32);
        let n = name("f32");
        let writer = AudioSlab::create(n.clone(), l.clone()).unwrap();

        let data: Vec<f32> = (0..128).map(|i| i as f32 * 0.1).collect();
        writer.write_input(1, 0, &data).unwrap();
        writer.publish_input(1);

        let reader = AudioSlab::open(n, l).unwrap();
        assert!(reader.has_input(1));
        let mut out = vec![0.0f32; 128];
        let got = reader.read_input_into(1, 0, &mut out).unwrap();
        assert_eq!(got, 128);
        assert_eq!(data, out);
    }

    #[test]
    fn roundtrip_f64() {
        let l = layout(64, SampleFormat::Float64);
        let n = name("f64");
        let writer = AudioSlab::create(n.clone(), l.clone()).unwrap();
        let data: Vec<f64> = (0..64).map(|i| (i as f64).sin()).collect();
        writer.write_output(1, 0, &data).unwrap();
        writer.publish_output(1);

        let reader = AudioSlab::open(n, l).unwrap();
        let mut out = vec![0.0f64; 64];
        reader.read_output_into(1, 0, &mut out).unwrap();
        assert_eq!(data, out);
    }

    /// **The direct regression guard for the shipped bypass.** The host writes
    /// its input and the server never answers; the output region must not hand
    /// that input back. Before the reshape these two addresses were the same for
    /// any plugin with one bus per direction — the common case.
    #[test]
    fn the_output_region_never_aliases_the_input_region() {
        let l = stereo_layout(64, SampleFormat::Float32);
        let n = name("alias");
        let slab = AudioSlab::create(n, l).unwrap();

        let input: Vec<f32> = (0..64).map(|i| i as f32 + 1.0).collect();
        for ch in 0..2 {
            slab.write_input(1, ch, &input).unwrap();
        }
        slab.publish_input(1);

        // Nobody published an output for block 1.
        assert!(!slab.has_output(1), "no output was published");

        let mut out = vec![0.0f32; 64];
        for ch in 0..2 {
            slab.read_output_into(1, ch, &mut out).unwrap();
            assert!(
                out.iter().all(|&s| s == 0.0),
                "ch {ch}: reading the output region returned the host's own input — \
                 this is the bypass bug"
            );
        }
    }

    /// Reading a slot that holds a *different* block must be detectable. The
    /// sequence is the only evidence; the bytes themselves are as plausible as
    /// any other block's.
    #[test]
    fn a_stale_slot_is_detectable_by_sequence_alone() {
        let l = stereo_layout(64, SampleFormat::Float32);
        let n = name("stale");
        let slab = AudioSlab::create(n, l).unwrap();

        let old: Vec<f32> = (0..64).map(|i| i as f32).collect();
        slab.write_output(1, 0, &old).unwrap();
        slab.publish_output(1);

        // Block 3 maps to the same slot as block 1 (depth 2), and the bytes
        // there are real audio — just the wrong block's.
        assert_eq!(slot_for(3), slot_for(1));
        assert!(!slab.has_output(3), "slot holds block 1, not block 3");
        assert!(slab.has_output(1));
    }

    /// Consecutive blocks occupy different slots, which is what lets one be read
    /// while the next is written.
    #[test]
    fn consecutive_blocks_do_not_share_a_slot() {
        let l = stereo_layout(64, SampleFormat::Float32);
        let n = name("ring");
        let slab = AudioSlab::create(n, l).unwrap();

        let a = vec![1.0f32; 64];
        let b = vec![2.0f32; 64];
        slab.write_output(1, 0, &a).unwrap();
        slab.publish_output(1);
        slab.write_output(2, 0, &b).unwrap();
        slab.publish_output(2);

        assert!(slab.has_output(1), "block 1 survived block 2's write");
        assert!(slab.has_output(2));

        let mut out = vec![0.0f32; 64];
        slab.read_output_into(1, 0, &mut out).unwrap();
        assert_eq!(out, a);
        slab.read_output_into(2, 0, &mut out).unwrap();
        assert_eq!(out, b);
    }

    /// Every (direction, slot, channel) triple must be a distinct region.
    /// Written as an exhaustive fill-and-verify rather than an offset
    /// calculation, so it catches a wrong base, a dropped slot term, and a
    /// channel/slot transposition alike.
    #[test]
    fn every_region_is_distinct() {
        let l = layout(16, SampleFormat::Float32);
        let n = name("distinct");
        let slab = AudioSlab::create(n, l.clone()).unwrap();

        // Stamp each region with a unique constant.
        let mut tag = 0.0f32;
        for seq in 1..=RING_SLOTS as u64 {
            for ch in 0..l.input_channels() {
                tag += 1.0;
                slab.write_input(seq, ch, &[tag; 16]).unwrap();
            }
            for ch in 0..l.output_channels() {
                tag += 1.0;
                slab.write_output(seq, ch, &[tag; 16]).unwrap();
            }
        }

        // Read every region back; each must still hold its own stamp.
        let mut expect = 0.0f32;
        let mut out = vec![0.0f32; 16];
        for seq in 1..=RING_SLOTS as u64 {
            for ch in 0..l.input_channels() {
                expect += 1.0;
                slab.read_input_into(seq, ch, &mut out).unwrap();
                assert!(
                    out.iter().all(|&s| s == expect),
                    "input seq={seq} ch={ch} was overwritten by another region"
                );
            }
            for ch in 0..l.output_channels() {
                expect += 1.0;
                slab.read_output_into(seq, ch, &mut out).unwrap();
                assert!(
                    out.iter().all(|&s| s == expect),
                    "output seq={seq} ch={ch} was overwritten by another region"
                );
            }
        }
    }

    #[test]
    fn clone_preserves_layout() {
        let l = layout(32, SampleFormat::Float64);
        let original = AudioSlab::create(name("clone"), l.clone()).unwrap();
        let cloned = original.clone();
        assert_eq!(cloned.layout(), l);
    }

    /// A clone points at the same pages, sequences included — so a node cloned
    /// on graph commit still sees what the original published.
    #[test]
    fn clone_shares_the_header() {
        let l = stereo_layout(64, SampleFormat::Float32);
        let original = AudioSlab::create(name("clone_hdr"), l).unwrap();
        original.publish_output(5);
        let cloned = original.clone();
        assert!(cloned.has_output(5));
    }

    #[test]
    fn channel_out_of_bounds() {
        let l = layout(64, SampleFormat::Float32);
        let slab = AudioSlab::create(name("oob"), l).unwrap();
        let data = vec![0.0f32; 64];
        // 3 input channels, 2 output channels — the asymmetry matters: an index
        // valid for one direction may not be valid for the other.
        assert!(slab.write_input(1, 3, &data).is_err());
        assert!(slab.write_output(1, 2, &data).is_err());
        assert!(
            slab.write_input(1, 2, &data).is_ok(),
            "sidechain is in range"
        );
        let mut out = vec![0.0f32; 64];
        assert!(slab.read_output_into(1, 5, &mut out).is_err());
    }

    #[test]
    fn data_exceeds_capacity() {
        let l = stereo_layout(32, SampleFormat::Float32);
        let slab = AudioSlab::create(name("exceed"), l).unwrap();
        let data = vec![0.0f32; 64];
        assert!(slab.write_input(1, 0, &data).is_err());
    }

    #[test]
    fn getters() {
        let l = layout(256, SampleFormat::Float32);
        let n = name("getters");
        let slab = AudioSlab::create(n.clone(), l.clone()).unwrap();
        assert_eq!(slab.name(), n);
        assert_eq!(slab.layout(), l);
    }

    /// An empty bus list is the shape that meant "share one region in place".
    /// It must now be rejected outright rather than reinterpreted.
    #[test]
    fn an_empty_bus_list_is_refused() {
        let mut l = stereo_layout(64, SampleFormat::Float32);
        l.inputs = SmallVec::new();
        assert!(AudioSlab::create(name("empty"), l).is_err());
    }

    /// A peer asking for a ring depth this build cannot address is refused,
    /// rather than silently indexing the wrong slot.
    #[test]
    fn a_foreign_ring_depth_is_refused() {
        let mut l = stereo_layout(64, SampleFormat::Float32);
        l.slots = RING_SLOTS as u32 + 1;
        assert!(AudioSlab::create(name("depth"), l).is_err());
    }

    /// Opening a file that is not a slab must fail on the magic, not produce a
    /// mapping full of someone else's bytes read as audio.
    #[test]
    fn opening_a_foreign_file_is_refused() {
        let l = stereo_layout(64, SampleFormat::Float32);
        let n = name("foreign");
        let path = shm_path(&n);
        std::fs::write(&path, vec![0xABu8; byte_size(&l)]).unwrap();

        let err = AudioSlab::open(n, l)
            .err()
            .expect("a foreign file must not open");
        assert!(
            format!("{err}").contains("not a tutti audio slab"),
            "expected a magic failure, got: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A slab created for a smaller layout must not be openable with a larger
    /// one — the mapping would end mid-region and every read past it would be
    /// out of bounds.
    #[test]
    fn opening_with_an_oversized_layout_is_refused() {
        let small = stereo_layout(64, SampleFormat::Float32);
        let n = name("short");
        let _owner = AudioSlab::create(n.clone(), small).unwrap();

        let big = stereo_layout(256, SampleFormat::Float32);
        let err = AudioSlab::open(n, big)
            .err()
            .expect("an oversized layout must not open");
        assert!(format!("{err}").contains("bytes"), "got: {err}");
    }

    /// Read the mapping's raw bytes and check the layout *without* going through
    /// the offset helper that produced it.
    ///
    /// Every other test here writes with `write_input` and reads with
    /// `read_input_into`, so both sides share `offset_of`. A consistent
    /// error in it — a dropped slot term, a swapped channel/slot factor, the
    /// wrong region base — cancels out and round-trips perfectly. This test
    /// recomputes each address from the layout arithmetic independently, so such
    /// an error shows up as a value in the wrong place rather than not at all.
    ///
    /// The `#[repr(C)]` header is checked here too: `magic` sits at offset 0 of
    /// the mapping, which is what the opening side keys on before trusting a
    /// single audio byte.
    ///
    /// This is the by-hand `hexdump` from the pipelining plan, kept as a test
    /// because nothing about it needed a human eye — only bytes at addresses.
    #[test]
    fn raw_bytes_land_where_the_layout_says_they_should() {
        const SAMPLES: usize = 8;
        // 3 in / 2 out, so a swapped direction or channel count cannot alias.
        let l = layout(SAMPLES, SampleFormat::Float32);
        let in_channels = l.input_channels();
        let out_channels = l.output_channels();
        assert_eq!(
            (in_channels, out_channels),
            (3, 2),
            "this test's arithmetic assumes the asymmetric helper layout"
        );

        let slab = AudioSlab::create(name("hexdump"), l.clone()).unwrap();

        // A distinct constant per (direction, slot, channel). Every sample in a
        // channel carries the same value, so a misaddressed *channel* is
        // visible; the values are spread far enough apart that a misaddressed
        // *slot* or *region* is too.
        let tag = |dir: u8, slot: usize, ch: usize| -> f32 {
            (dir as f32) * 1000.0 + (slot as f32) * 100.0 + (ch as f32) + 1.0
        };

        for slot in 0..RING_SLOTS {
            // `seq` must be a sequence whose ring slot is `slot`; at depth 2 the
            // sequence value and the slot coincide for 0..RING_SLOTS.
            let seq = slot as u64;
            for ch in 0..in_channels {
                slab.write_input(seq, ch, &[tag(0, slot, ch); SAMPLES])
                    .unwrap();
            }
            for ch in 0..out_channels {
                slab.write_output(seq, ch, &[tag(1, slot, ch); SAMPLES])
                    .unwrap();
            }
        }

        let bytes = slab.mmap.as_slice();
        assert_eq!(
            bytes.len(),
            byte_size(&l),
            "mapping is not the sized length"
        );

        // 1. The magic is the first thing in the mapping.
        let magic = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        assert_eq!(
            magic,
            crate::util::transport::shm::header::SLAB_MAGIC,
            "magic must sit at offset 0 — the opening side reads it before anything else"
        );

        // 2. Audio starts after the header, and the two rings are disjoint.
        let stride = SAMPLES * size_of::<f32>();
        let input_base = SLAB_HEADER_BYTES;
        let output_base = SLAB_HEADER_BYTES + l.input_ring_bytes();
        assert_eq!(
            l.input_ring_bytes(),
            RING_SLOTS * in_channels * stride,
            "input ring must cover every slot x channel"
        );
        assert!(
            output_base >= input_base + l.input_ring_bytes(),
            "the output ring starts inside the input ring: bases {input_base} / {output_base}"
        );

        // 3. Every channel of every slot, addressed from the layout rather than
        //    from the code under test.
        let read_at = |offset: usize| -> f32 {
            f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
        };
        for slot in 0..RING_SLOTS {
            for ch in 0..in_channels {
                let at = input_base + (slot * in_channels + ch) * stride;
                assert_eq!(
                    read_at(at),
                    tag(0, slot, ch),
                    "input slot {slot} ch {ch} is not at byte {at}"
                );
            }
            for ch in 0..out_channels {
                let at = output_base + (slot * out_channels + ch) * stride;
                assert_eq!(
                    read_at(at),
                    tag(1, slot, ch),
                    "output slot {slot} ch {ch} is not at byte {at}"
                );
            }
        }

        // 4. The directions really do hold different data. The shipped bypass
        //    was precisely the case where reading "output" returned the input.
        assert_ne!(
            read_at(input_base),
            read_at(output_base),
            "input and output regions hold identical bytes — the aliasing bug"
        );
    }
}
