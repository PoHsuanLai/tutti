//! Named, cross-process audio storage. `channels × samples_per_channel`
//! of `f32`/`f64` samples in shared memory.
//!
//! # The process edge, in the engine's I/O vocabulary
//!
//! This slab is where audio genuinely crosses the subprocess boundary, so it
//! is the concrete realisation of [`tutti_types::io`]'s two roles at that edge:
//! [`write_channel`](AudioSlab::write_channel) is the
//! [`AudioOut`](tutti_types::io::AudioOut) side (push a block of samples into
//! the shared region) and [`read_channel_into`](AudioSlab::read_channel_into)
//! is the [`AudioIn`](tutti_types::io::AudioIn) side (poll a block back out).
//! The slab does not `impl` those traits directly: they move *interleaved*
//! `[S; CH]` frames, whereas the slab is *mono-planar* — one fixed contiguous
//! region per channel, addressed by index — because the plugin ABIs it feeds
//! are deinterleaved and the memcpy must stay a straight per-channel copy with
//! no transpose. The traits name what each direction *is*; this module is the
//! planar, fixed-region edge that carries it across the process boundary.
//!
//! # Why shared memory
//!
//! Each plugin runs in its own subprocess, so the host and the plugin live
//! in separate address spaces and share no memory by default. Audio has to
//! cross that boundary every block (~thousands of times a second), on the
//! real-time audio thread, which must never block, allocate, or wait on the
//! kernel. Sending it over the control socket would mean serialize + two
//! syscalls + kernel copies *per block* — far too expensive.
//!
//! Instead both processes `mmap` the *same* named, RAM-backed file
//! (`/dev/shm` on Linux, a temp file elsewhere), so the kernel maps the
//! same physical pages into both. A write on one side is instantly visible
//! to the other: transfer becomes a plain `memcpy` with no syscall in the
//! hot path. See [`AudioSlab::write_channel`] / [`read_channel_into`].
//!
//! [`read_channel_into`]: AudioSlab::read_channel_into
//!
//! # No locking
//!
//! There is deliberately **no synchronization in this module** — no header, no
//! generation counter, no data-ready flag. A mutex on the audio thread would
//! defeat the purpose. The single-writer-per-channel invariant is upheld
//! entirely by the request/reply handshake one layer up: the host writes its
//! inputs, sends `ProcessAudio`, and does not touch the slab again until the
//! matching `AudioProcessed` comes back; the server only touches it while
//! serving that request.
//!
//! **The handshake is what makes a read meaningful, and it is not optional.**
//! [`read_channel_into`](AudioSlab::read_channel_into) reports a full-length
//! success whether or not the other side ever wrote the region — "the server
//! never wrote this" is indistinguishable from "the server wrote this". So a
//! caller that reads without having matched the reply to its own request reads
//! stale bytes and cannot tell. That is exactly how the out-of-process bypass
//! bug happened: with `output_base == 0` the host's own input write shares the
//! region, so an unmatched read returned the host's input at unity gain — a
//! silent bypass. `BridgeMessage::AudioProcessed` therefore echoes the
//! request's `buffer_id`, and `AudioBridge::process` reads the slab only after
//! matching it.
//!
//! # Sample payload stays raw
//!
//! The `f32`/`f64` samples here are deliberately NOT wrapped in the engine's
//! unit newtypes. This is a C-ABI / IPC boundary: the bytes are `memcpy`'d
//! into a region the plugin subprocess reads with its own ABI expectations,
//! where the layout must be exactly the primitive's. The unit vocabulary stops
//! at this edge by design.
//!
//! # Lifecycle
//!
//! One side [`create`s](AudioSlab::create) the slab (the [`Owner`], which
//! unlinks the backing file on drop); the other [`open`s](AudioSlab::open)
//! it (a [`View`]) using a matching [`SlabLayout`]. Both then
//! [`write_channel`](AudioSlab::write_channel) /
//! [`read_channel_into`](AudioSlab::read_channel_into) against the shared
//! region; [`Clone`] reopens it as a fresh view so a cloned audio node
//! points at the same pages.
//!
//! [`Owner`]: Ownership::Owner
//! [`View`]: Ownership::View

use crate::error::{BridgeError, Result};
use crate::protocol::audio::Sample;
use crate::protocol::{SampleFormat, SlabLayout};
use memmap2::MmapMut;
use std::fs::OpenOptions;

use super::mmap::{as_bytes, as_bytes_mut, MmapCell};

fn sample_size(format: SampleFormat) -> usize {
    match format {
        SampleFormat::Float32 => std::mem::size_of::<f32>(),
        SampleFormat::Float64 => std::mem::size_of::<f64>(),
    }
}

fn channel_offset(layout: &SlabLayout, channel: usize) -> usize {
    channel * layout.samples_per_channel * sample_size(layout.format)
}

/// Who is responsible for the backing file's lifetime.
enum Ownership {
    /// Created the slab; unlinks the backing file on drop.
    Owner,
    /// Opened an existing slab; detaches on drop without deleting it.
    View,
}

/// Named mmap region of `channels × samples_per_channel` audio samples.
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

    /// Create the slab: allocate the named backing file, size it to
    /// `layout.byte_size()`, and map it. This side is the [`Owner`] and
    /// unlinks the file on drop. Exactly one side calls this; the other
    /// calls [`open`](Self::open) with a matching `layout`.
    ///
    /// [`Owner`]: Ownership::Owner
    pub fn create(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        let name = name.into();
        let mmap = open_mmap(&name, layout.byte_size(), Open::Create)?;
        Ok(Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::Owner,
        })
    }

    /// Open an existing slab as a [`View`]. The `layout` must match the one
    /// the [`Owner`] created it with — both sides agree on the shape out of
    /// band (over the control channel) before mapping. Detaches on drop
    /// without deleting the backing file.
    ///
    /// [`View`]: Ownership::View
    /// [`Owner`]: Ownership::Owner
    pub fn open(name: impl Into<String>, layout: SlabLayout) -> Result<Self> {
        let name = name.into();
        let mmap = open_mmap(&name, layout.byte_size(), Open::Existing)?;
        Ok(Self {
            mmap: MmapCell::new(mmap),
            name,
            layout,
            ownership: Ownership::View,
        })
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
    // Both are a single bounds check + `memcpy` into/out of the mapped
    // region — no syscall, no allocation. Safe to call on the audio thread.
    // The single-writer-per-channel invariant is the caller's responsibility
    // (upheld by the control-channel handshake; see the module docs).

    /// Copy `data` into `channel`'s region of the shared buffer. The per-channel
    /// [`AudioOut`](tutti_types::io::AudioOut) push at the process edge: a
    /// multichannel writer calls this once per channel to place one block.
    ///
    /// Caller must ensure single-writer access per channel — there is no
    /// internal locking. Errors if `channel` is out of range or `data` is
    /// longer than `samples_per_channel`.
    pub fn write_channel<T: Sample>(&self, channel: usize, data: &[T]) -> Result<()> {
        self.check_channel(channel)?;
        if data.len() > self.layout.samples_per_channel {
            return Err(oob("data length exceeds buffer capacity"));
        }
        let offset = channel_offset(&self.layout, channel);
        let bytes = as_bytes(data);
        self.mmap.as_mut_slice()[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Copy `channel`'s region out into `output`. The per-channel
    /// [`AudioIn`](tutti_types::io::AudioIn) poll at the process edge: a
    /// multichannel reader calls this once per channel to pull one block, and
    /// the returned count mirrors `AudioIn::poll_into`'s "how many frames I
    /// actually produced" contract. Reads `min(samples_per_channel,
    /// output.len())` samples and returns that count. Errors if `channel` is
    /// out of range.
    ///
    /// The count says how many samples were copied, **not** whether anyone
    /// wrote them: an untouched region reads back at full length just like a
    /// freshly written one. Only call this once the control-channel handshake
    /// has confirmed the other side wrote this block (see the module docs).
    pub fn read_channel_into<T: Sample>(&self, channel: usize, output: &mut [T]) -> Result<usize> {
        self.check_channel(channel)?;
        let copy_samples = self.layout.samples_per_channel.min(output.len());
        let copy_bytes = copy_samples * std::mem::size_of::<T>();
        let offset = channel_offset(&self.layout, channel);
        let src = &self.mmap.as_slice()[offset..offset + copy_bytes];
        as_bytes_mut(&mut output[..copy_samples]).copy_from_slice(src);
        Ok(copy_samples)
    }

    // ---- Internal helpers ----

    /// Reject channel indices past the slab's channel count.
    fn check_channel(&self, channel: usize) -> Result<()> {
        if channel >= self.layout.channels.count() as usize {
            Err(oob("channel index out of bounds"))
        } else {
            Ok(())
        }
    }
}

impl Clone for AudioSlab {
    /// Reopen the same backing file as a fresh [`View`], so a cloned audio
    /// node still points at the same physical pages. The clone never owns
    /// the file, regardless of this side's [`Ownership`].
    ///
    /// [`View`]: Ownership::View
    fn clone(&self) -> Self {
        Self::open(self.name.clone(), self.layout.clone())
            .expect("failed to reopen shared-memory slab for clone")
    }
}

impl Drop for AudioSlab {
    /// Only the [`Owner`] unlinks the backing file; [`View`]s just detach.
    ///
    /// [`Owner`]: Ownership::Owner
    /// [`View`]: Ownership::View
    fn drop(&mut self) {
        if matches!(self.ownership, Ownership::Owner) {
            let _ = std::fs::remove_file(shm_path(&self.name));
        }
    }
}

fn oob(msg: &'static str) -> BridgeError {
    BridgeError::SharedMemoryError(msg.into())
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

    fn layout(channels: usize, samples: usize, format: SampleFormat) -> SlabLayout {
        SlabLayout {
            channels: crate::protocol::ChannelLayout::from(channels),
            samples_per_channel: samples,
            format,
            inputs: Default::default(),
            outputs: Default::default(),
        }
    }

    #[test]
    fn roundtrip_f32() {
        let name = format!("slab_f32_{}", std::process::id());
        let layout = layout(2, 128, SampleFormat::Float32);
        let writer = AudioSlab::create(name.clone(), layout.clone()).unwrap();

        let data: Vec<f32> = (0..128).map(|i| i as f32 * 0.1).collect();
        writer.write_channel(0, &data).unwrap();

        let reader = AudioSlab::open(name, layout).unwrap();
        let mut out = vec![0.0f32; 128];
        let n = reader.read_channel_into(0, &mut out).unwrap();
        assert_eq!(n, 128);
        assert_eq!(data, out);
    }

    #[test]
    fn roundtrip_f64() {
        let name = format!("slab_f64_{}", std::process::id());
        let layout = layout(1, 64, SampleFormat::Float64);
        let writer = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let data: Vec<f64> = (0..64).map(|i| (i as f64).sin()).collect();
        writer.write_channel(0, &data).unwrap();

        let reader = AudioSlab::open(name, layout).unwrap();
        let mut out = vec![0.0f64; 64];
        reader.read_channel_into(0, &mut out).unwrap();
        assert_eq!(data, out);
    }

    #[test]
    fn clone_preserves_layout() {
        let name = format!("slab_clone_{}", std::process::id());
        let layout = layout(2, 32, SampleFormat::Float64);
        let original = AudioSlab::create(name, layout.clone()).unwrap();
        let cloned = original.clone();
        assert_eq!(cloned.layout(), layout);
    }

    #[test]
    fn channel_out_of_bounds() {
        let name = format!("slab_oob_{}", std::process::id());
        let slab = AudioSlab::create(name, layout(2, 64, SampleFormat::Float32)).unwrap();
        let data = vec![0.0f32; 64];
        assert!(slab.write_channel(2, &data).is_err());
        let mut out = vec![0.0f32; 64];
        assert!(slab.read_channel_into(5, &mut out).is_err());
    }

    #[test]
    fn data_exceeds_capacity() {
        let name = format!("slab_exceed_{}", std::process::id());
        let slab = AudioSlab::create(name, layout(1, 32, SampleFormat::Float32)).unwrap();
        let data = vec![0.0f32; 64];
        assert!(slab.write_channel(0, &data).is_err());
    }

    #[test]
    fn getters() {
        let name = format!("slab_getters_{}", std::process::id());
        let l = layout(4, 256, SampleFormat::Float32);
        let slab = AudioSlab::create(name.clone(), l.clone()).unwrap();
        assert_eq!(slab.name(), name);
        assert_eq!(slab.layout(), l);
    }
}
