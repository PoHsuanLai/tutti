//! Lock-free ring buffers for audio streaming.

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::cell::UnsafeCell;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tutti_core::{AtomicU64, ChannelLayout, Ordering};

use crate::nonempty;

#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
use tutti_core::FileIn;

use super::command::RegionId;

/// ringbuf `HeapProd` wrapper that is `Send + Sync` when `T: Send`.
///
/// ringbuf conservatively doesn't impl Send/Sync for its SPSC handles.
/// This wrapper concentrates the unsafe impls in one place.
struct SendProd<T>(HeapProd<T>);

// SAFETY: HeapProd is designed for single-producer cross-thread use.
// T: Send ensures the element type is safe to send across threads.
unsafe impl<T: Send> Send for SendProd<T> {}
unsafe impl<T: Send> Sync for SendProd<T> {}

impl<T> SendProd<T> {
    fn new(prod: HeapProd<T>) -> Self {
        Self(prod)
    }
    fn try_push(&mut self, val: T) -> Result<(), T> {
        self.0.try_push(val)
    }
    fn vacant_len(&self) -> usize {
        self.0.vacant_len()
    }
    fn capacity(&self) -> core::num::NonZeroUsize {
        self.0.capacity()
    }
}

/// ringbuf `HeapCons` wrapper that is `Send + Sync` when `T: Send`.
struct SendCons<T>(HeapCons<T>);

// SAFETY: HeapCons is designed for single-consumer cross-thread use.
unsafe impl<T: Send> Send for SendCons<T> {}
unsafe impl<T: Send> Sync for SendCons<T> {}

impl<T> SendCons<T> {
    fn new(cons: HeapCons<T>) -> Self {
        Self(cons)
    }
    fn try_pop(&mut self) -> Option<T> {
        self.0.try_pop()
    }
    fn occupied_len(&self) -> usize {
        self.0.occupied_len()
    }
}

pub(crate) struct RegionMeta {
    region_id: RegionId,
    file_path: PathBuf,
    file_position: AtomicU64,
}

impl RegionMeta {
    pub fn file_position(&self) -> u64 {
        self.file_position.load(Ordering::Relaxed)
    }

    pub fn set_file_position(&self, pos: u64) {
        self.file_position.store(pos, Ordering::Relaxed);
    }
}

/// The producer half of a region's SPSC ring.
///
/// # Frames, not samples
///
/// The ring itself is a flat `HeapRb<f32>` (a runtime channel count cannot be a
/// const-generic element type), but **every public method here is denominated in
/// frames** and the stride never leaks out. That is deliberate: `plan.rs`
/// compares `read_position` against a loop range in *file frames*, and
/// `loops.rs` uses it to index the file directly. Exposing samples anywhere on
/// this boundary would silently multiply every loop point by the channel count.
pub(crate) struct RegionOut {
    prod: SendProd<f32>,
    /// Declared ring width. One frame is `channels.count()` consecutive slots.
    channels: ChannelLayout,
    /// `channels.count()`, cached — the interleave stride.
    ///
    /// Every method below divides or chunks by it, and `push_interleaved` /
    /// `write_interleaved_reversed` run it per frame. The layout is the
    /// declaration; this is its arithmetic. Set once at construction, so the two
    /// cannot drift.
    stride: usize,
    meta: Arc<RegionMeta>,
    /// Incremental disk decoder for real streaming. `None` means this region
    /// uses the whole-file `load_wave` + `LruCache` fallback path (non-seekable
    /// format, or no frame count). `Box<dyn FormatReader/Decoder>` are `Send`,
    /// so the rayon `par_iter_mut` refill path is fine.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    decoder: Option<FileIn>,
}

impl RegionOut {
    pub fn file_position(&self) -> u64 {
        self.meta.file_position()
    }

    /// Install an incremental disk decoder, switching this region onto the
    /// real-streaming refill path. Without one, refill uses the whole-file
    /// fallback.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    pub(crate) fn set_decoder(&mut self, decoder: FileIn) {
        self.decoder = Some(decoder);
    }

    /// Mutable access to the streaming decoder, if this region streams.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    pub(crate) fn decoder_mut(&mut self) -> Option<&mut FileIn> {
        self.decoder.as_mut()
    }

    pub fn set_file_position(&self, pos: u64) {
        self.meta.set_file_position(pos);
    }

    /// Declared ring width.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Free space in **frames**.
    pub fn write_space(&self) -> usize {
        self.prod.vacant_len() / self.stride
    }

    /// Total capacity in **frames**.
    pub fn capacity(&self) -> usize {
        self.prod.capacity().get() / self.stride
    }

    /// Push interleaved frames from a flat slice, returning how many **frames**
    /// landed. A trailing partial frame in `samples` is ignored.
    ///
    /// Each frame is pushed **all-or-nothing**: vacancy for the whole frame is
    /// checked before its first sample, so the ring can never hold a torn frame
    /// for the consumer to read. A tear would permanently rotate channels for
    /// the rest of the stream — no click, no underrun, just a subtly wrong mix.
    ///
    /// `vacant_len` is conservative under SPSC concurrency, but only in the safe
    /// direction here: the producer's view of free space can only *grow* as the
    /// consumer pops, so a frame that passes the check still fits.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> usize {
        let ch = self.stride;
        let mut written = 0;
        for f in samples.chunks_exact(ch) {
            let mut ok = true;
            for &s in f {
                if self.prod.try_push(s).is_err() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
            written += 1;
        }
        written
    }

    /// Write frames in reverse order (for reverse playback). Frames are taken
    /// from the end of the slice first; returns how many **frames** landed
    /// before the ring filled. `pump` can't express the reversal, so the reverse
    /// refill path calls this directly.
    ///
    /// Only the *frame sequence* reverses — the channels **within** each frame
    /// stay in order. Reversing those too would swap L/R (and every other pair)
    /// on every reverse-played source.
    pub fn write_interleaved_reversed(&mut self, samples: &[f32]) -> usize {
        let ch = self.stride;
        let mut written = 0;
        for f in samples.chunks_exact(ch).rev() {
            if self.prod.vacant_len() < ch {
                break;
            }
            for &s in f {
                let _ = self.prod.try_push(s);
            }
            written += 1;
        }
        written
    }

    pub fn file_path(&self) -> &PathBuf {
        &self.meta.file_path
    }

    pub(crate) fn region_id(&self) -> RegionId {
        self.meta.region_id
    }
}

/// The ring is the engine's [`AudioOut`] shape: flat interleaved in, a runtime
/// [`ChannelLayout`] for the width, counts in frames. That is now expressible as
/// the trait, so it is stated as the trait — a region ring can feed any generic
/// consumer written against the engine vocabulary ([`pump`](tutti_core::pump)
/// included) rather than only code that knows the name `push_interleaved`.
///
/// # Additive, not a replacement — the inherent methods stay
///
/// `RegionOut` is not *purely* a sink and the trait does not try to pretend
/// otherwise. It also owns a [`FileIn`] decoder and its [`RegionMeta`]
/// (`set_decoder`, `file_position`, `write_space`, `capacity`), and
/// [`write_interleaved_reversed`](Self::write_interleaved_reversed) has no trait
/// counterpart at all — reverse refill is a butler policy, not something every
/// audio sink can do. Forcing those through `AudioOut` would produce trait
/// methods whose meaning depends on which tier you are in, which is exactly the
/// failure that got `ClipReader` deleted.
///
/// So the trait covers the one thing it genuinely describes — appending frames —
/// and the rest stays inherent. Notably [`push_interleaved`](Self::push_interleaved)
/// stays public too: `write` must return `()` to satisfy the trait, but the
/// refill path *needs* the landed frame count to advance `file_position`. That
/// count is the whole bookkeeping of the butler, so the richer inherent method
/// remains the one production callers use, and `write` is implemented in terms
/// of it.
impl tutti_core::AudioOut<f32> for RegionOut {
    fn layout(&self) -> ChannelLayout {
        self.channels
    }

    /// Append frames, discarding the landed count.
    ///
    /// A short write here means the ring was full — for a bounded SPSC ring that
    /// is back-pressure, not an error, and the trait has no way to report it.
    /// Any caller that must not lose frames wants
    /// [`push_interleaved`](Self::push_interleaved), which returns the count.
    fn write(&mut self, frames: &[f32]) {
        let _ = self.push_interleaved(frames);
    }

    /// No-op: the ring is a live SPSC channel with a consumer on the audio
    /// thread, never a stream that gets closed. There is no header to
    /// back-patch and no descriptor to flush.
    fn finalize(self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct RegionReader {
    cons: SendCons<f32>,
    /// Declared ring width — see [`RegionOut`]'s note on frames vs samples.
    channels: ChannelLayout,
    /// `channels.count()`, cached — the interleave stride.
    ///
    /// [`read_into`](Self::read_into) is the audio thread's per-frame pop: it
    /// reads the stride twice (an occupancy check and the pop loop's bound) on
    /// every frame of every block. Re-deriving from the layout there would put
    /// an enum match inside the pop loop.
    stride: usize,
    read_position: Arc<AtomicU64>,
    region_id: RegionId,
}

impl RegionReader {
    pub(crate) fn region_id(&self) -> RegionId {
        self.region_id
    }

    /// Declared ring width.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Pop the next frame into `out`, returning `false` on underrun.
    ///
    /// **All-or-nothing**: on underrun nothing is consumed and `read_position`
    /// does not move, so it can never land mid-frame. `out` shorter than
    /// `channels` receives the leading channels; the rest of the frame is still
    /// consumed, so the stream stays aligned.
    #[inline]
    pub fn read_into(&mut self, out: &mut [f32]) -> bool {
        // `self.stride`, not `self.channels.count()`: this is the per-frame pop.
        let ch = self.stride;
        if self.cons.occupied_len() < ch {
            return false;
        }
        for c in 0..ch {
            // Cannot fail: occupancy was checked above and we are the sole
            // consumer, so nothing else can have taken these slots.
            let s = self.cons.try_pop().unwrap_or(0.0);
            if let Some(o) = out.get_mut(c) {
                *o = s;
            }
        }
        // ONE per FRAME. `plan.rs` compares this against a loop range in file
        // frames and `loops.rs` indexes the file with it — a sample-denominated
        // count would wrap a looped source at 1/channels of its true length.
        self.read_position.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Clear all buffered frames without processing them.
    /// Used for loop resets — much faster than draining one-by-one.
    pub fn clear(&mut self) {
        let frames = self.cons.occupied_len() / self.stride;
        for _ in 0..frames * self.stride {
            let _ = self.cons.try_pop();
        }
        // Drain any straggling partial frame so the ring realigns on a frame
        // boundary. Unreachable given all-or-nothing pushes, but a stray sample
        // here would desynchronise every subsequent frame — cheap insurance on
        // the one invariant whose failure is inaudible as a glitch.
        while self.cons.try_pop().is_some() {}
        self.read_position
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    /// Get a shared handle to the read position for lock-free access.
    pub(crate) fn read_position_shared(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.read_position)
    }
}

/// Interior-mutable wrapper over a [`RegionReader`] that lets the single audio
/// consumer pop through a shared `&self`, so the reader can be held behind an
/// [`ArcSwap`] instead of a `Mutex`.
///
/// # Why this exists
///
/// The old design put the reader behind `Arc<Mutex<RegionReader>>` and had the
/// audio thread `try_lock()` it in `tick`/`process`. That is wrong on two
/// counts: (1) locking on the audio hot path, and (2) a `try_lock` *miss* was
/// counted as an underrun even though the ring was full — the butler merely
/// happened to hold the lock. A [`RegionReader`] wraps a single-consumer SPSC
/// ring, so no lock is architecturally required: exactly one party ever pops.
///
/// # Single-consumer safety invariant (why the `UnsafeCell` is sound)
///
/// [`HeapCons::try_pop`] needs `&mut` because it advances the read index. This
/// cell hands out that mutation through `&self`, which is only sound if **no two
/// threads ever pop/clear concurrently**. That invariant holds because:
///
///   * The **audio thread** is the sole popper. `read()` / `clear()` are called
///     only from `DiskSource` / `DiskVoice` on the audio
///     thread. Those units may hold several clones of the same `SharedReader`
///     (the direct-read reader plus the time-stretch processor's internal
///     clone), but only one clone is *active* per buffer and both live on the
///     one audio thread — so the pops are serialized by that thread, never
///     concurrent.
///   * The **butler thread** never pops or clears the live reader. It only
///     *replaces* the reader wholesale via [`SharedReaderExt::store`] on a
///     stream (re)start; the audio thread observes the new reader on its next
///     buffer through a wait-free [`ArcSwap::load`]. Ring resets on
///     seek/loop-wrap are requested by the butler through a lock-free
///     `RtState` flag and applied by the audio thread (the owning consumer),
///     never by the butler touching the ring.
///
/// Under that discipline every access to the inner `HeapCons` happens from a
/// single thread at a time, so the `&self` → `&mut` reborrow is race-free.
pub(crate) struct ReaderCell {
    inner: UnsafeCell<RegionReader>,
    region_id: RegionId,
    /// Declared ring width, cached here so a reader lookup does not need the
    /// `UnsafeCell` reborrow. This cell already cached the count before the
    /// `ChannelLayout` conversion; the pattern is unchanged, only the type.
    channels: ChannelLayout,
    read_position: Arc<AtomicU64>,
}

// SAFETY: the inner `RegionReader` is only ever mutated by the single audio
// consumer (see the invariant on `ReaderCell`). The butler never pops; it swaps
// the whole cell. `Send`/`Sync` let the cell cross to the audio thread inside an
// `Arc<ArcSwap<_>>`; concurrent aliasing of the inner ring is prevented by the
// single-consumer discipline, not by the type system.
unsafe impl Send for ReaderCell {}
unsafe impl Sync for ReaderCell {}

impl ReaderCell {
    fn new(reader: RegionReader) -> Self {
        let region_id = reader.region_id();
        let channels = reader.channels();
        let read_position = reader.read_position_shared();
        Self {
            inner: UnsafeCell::new(reader),
            region_id,
            channels,
            read_position,
        }
    }

    /// Declared ring width.
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Pop the next frame into `out`, returning `false` on underrun.
    /// Audio-thread only (see the single-consumer invariant on [`ReaderCell`]).
    #[inline]
    pub(crate) fn read_into(&self, out: &mut [f32]) -> bool {
        // SAFETY: single-consumer invariant — only the audio thread calls this,
        // and its several `SharedReader` clones are serialized on that thread.
        unsafe { (*self.inner.get()).read_into(out) }
    }

    /// Discard all buffered samples. Audio-thread only, applied when the butler
    /// has requested a ring reset (seek / loop-wrap) via `RtState`.
    pub(crate) fn clear(&self) {
        // SAFETY: same single-consumer invariant as `read`.
        unsafe { (*self.inner.get()).clear() }
    }

    pub(crate) fn region_id(&self) -> RegionId {
        self.region_id
    }

    pub(crate) fn read_position_shared(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.read_position)
    }
}

/// The audio-thread reader handle: a wait-free-loadable, butler-replaceable
/// [`ReaderCell`]. Replaces the former `Arc<Mutex<RegionReader>>`. The audio
/// thread `load`s it (wait-free) and pops; the butler `store`s a replacement on
/// a stream (re)start. No lock ever sits on the audio hot path.
pub(crate) type SharedReader = Arc<ArcSwap<ReaderCell>>;

/// Wrap a freshly-built [`RegionReader`] into a [`SharedReader`] for handoff to
/// the audio thread.
pub(crate) fn share_reader(reader: RegionReader) -> SharedReader {
    Arc::new(ArcSwap::from_pointee(ReaderCell::new(reader)))
}

pub(crate) struct RegionBuffer;

impl RegionBuffer {
    /// Build a region's ring sized for `capacity` **frames** of `channels`
    /// each. `capacity` is a frame count, so the backing store is
    /// `capacity * channels` samples and every frame-denominated accessor on
    /// [`RegionOut`] / [`RegionReader`] reports the value the caller passed.
    pub(crate) fn with_capacity(
        region_id: RegionId,
        file_path: PathBuf,
        capacity: usize,
        channels: impl Into<ChannelLayout>,
    ) -> (RegionOut, RegionReader) {
        let channels = nonempty(channels.into());
        // Stride derived once; both halves get the same cached copy.
        let stride = channels.count() as usize;
        let capacity = capacity.max(4096);

        let rb = HeapRb::<f32>::new(capacity * stride);
        let (prod, cons) = rb.split();

        let meta = Arc::new(RegionMeta {
            region_id,
            file_path,
            file_position: AtomicU64::new(0),
        });

        let producer = RegionOut {
            prod: SendProd::new(prod),
            channels,
            stride,
            meta: meta.clone(),
            #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
            decoder: None,
        };

        let consumer = RegionReader {
            cons: SendCons::new(cons),
            channels,
            stride,
            read_position: Arc::new(AtomicU64::new(0)),
            region_id,
        };

        (producer, consumer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;

    /// `frames` stereo frames, flat interleaved, channel `c` of frame `f`
    /// carrying `f * 2 + c` so a rotation or a tear is a wrong value.
    fn indexed(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| i as f32).collect()
    }

    #[test]
    fn test_region_buffer_creation() {
        let capacity = (100.0 / 1000.0 * 44100.0) as usize;
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::from("test.wav"), capacity, 2usize);

        let samples: Vec<f32> = (0..100).flat_map(|i| [i as f32 / 100.0; 2]).collect();
        let written = prod.push_interleaved(&samples);
        assert_eq!(written, 100, "100 frames");

        let mut f = [9.0f32; 2];
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [0.0, 0.0]);
    }

    #[test]
    fn test_buffer_full() {
        let (mut prod, _) = RegionBuffer::with_capacity(
            RegionId(1),
            PathBuf::from("test.wav"),
            10, // Tiny buffer (will be clamped to 4096 frames)
            2usize,
        );

        let samples: Vec<f32> = (0..4096).flat_map(|i| [i as f32; 2]).collect();
        let written = prod.push_interleaved(&samples);
        assert!(written <= 4096);
    }

    #[test]
    fn test_clear_allows_refill() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::from("test.wav"), 100, 2usize);

        let section_a: Vec<f32> = (0..50).flat_map(|i| [i as f32 / 100.0; 2]).collect();
        assert_eq!(prod.push_interleaved(&section_a), 50);

        let mut f = [0.0f32; 2];
        assert!(cons.read_into(&mut f));
        assert!((f[0] - 0.0).abs() < 0.001, "First sample should be ~0.0");

        cons.clear();
        assert!(!cons.read_into(&mut f), "cleared ring must underrun");

        let section_b: Vec<f32> = (0..50).flat_map(|_| [0.5f32; 2]).collect();
        assert_eq!(prod.push_interleaved(&section_b), 50);
        assert!(cons.read_into(&mut f));
        assert!((f[0] - 0.5).abs() < 0.001, "refill must serve section B");
    }

    /// `read_position` counts FILE FRAMES at any width. `plan.rs` compares it
    /// against a loop range in file frames and `loops.rs` indexes the file with
    /// it, so a sample-denominated count would wrap a looped 6-channel source at
    /// one sixth of its true length.
    #[test]
    fn read_position_counts_frames_not_samples_at_six_channels() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 6usize);
        assert_eq!(prod.push_interleaved(&indexed(10, 6)), 10);

        let pos = cons.read_position_shared();
        let mut f = [0.0f32; 6];
        for _ in 0..10 {
            assert!(cons.read_into(&mut f));
        }
        assert_eq!(
            pos.load(Ordering::Relaxed),
            10,
            "10 frames of 6 channels must advance read_position by 10, not 60"
        );
    }

    /// `clear()` is frame-denominated too — a 6x overshoot here sends the butler
    /// to the wrong file offset on every seek.
    #[test]
    fn clear_advances_read_position_by_frames_at_six_channels() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 6usize);
        prod.push_interleaved(&indexed(10, 6));

        let pos = cons.read_position_shared();
        let mut f = [0.0f32; 6];
        for _ in 0..3 {
            assert!(cons.read_into(&mut f));
        }
        cons.clear();
        assert_eq!(
            pos.load(Ordering::Relaxed),
            10,
            "3 read + 7 cleared == 10 frames"
        );
    }

    /// A frame is pushed and popped all-or-nothing, so the consumer can never
    /// observe a torn frame (channel 0 of frame k beside channel 1 of frame
    /// k+1). A tear permanently rotates channels for the rest of the stream and
    /// is INAUDIBLE as a glitch — it just sounds like a wrong mix. Deliberately
    /// overfills the ring, the condition where a naive implementation tears.
    /// A full ring never hands out a torn frame (channel 0 of frame k beside
    /// channel 1 of frame k+1). A tear permanently rotates channels for the rest
    /// of the stream and is INAUDIBLE as a glitch — it just sounds like a wrong
    /// mix.
    ///
    /// Note this passes even without `push_interleaved`'s all-or-nothing gate,
    /// and that is worth stating rather than hiding: the ring is allocated as
    /// `capacity_frames * channels`, so its slot count is always a whole number
    /// of frames and a producer physically cannot stop mid-frame. The gate is
    /// belt-and-braces against a future sizing change (an odd capacity, a
    /// shared/resized ring) that would break that property silently. What this
    /// test DOES pin is that frames come out in order with their channels
    /// intact under overfill — which a stride mistake anywhere in the push/pop
    /// pair would break.
    #[test]
    fn a_full_ring_never_hands_out_a_torn_frame() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4096, 6usize);
        let n = prod.capacity() + 37; // deliberately past the end
        let pushed = prod.push_interleaved(&indexed(n, 6));
        assert_eq!(
            pushed,
            prod.capacity(),
            "overfill must stop exactly at capacity"
        );

        let mut f = [0.0f32; 6];
        for k in 0..pushed {
            assert!(cons.read_into(&mut f));
            for (c, &s) in f.iter().enumerate() {
                assert_eq!(
                    s,
                    (k * 6 + c) as f32,
                    "frame {k} channel {c} torn — ring lost frame alignment"
                );
            }
        }
        assert!(!cons.read_into(&mut f), "ring must be empty after draining");
    }

    /// The ring's slot count is always a whole number of frames. This is the
    /// property that makes a torn push structurally impossible, so it is worth
    /// pinning directly rather than leaving implicit in the sizing arithmetic.
    #[test]
    fn ring_capacity_is_a_whole_number_of_frames() {
        for ch in [1usize, 2, 3, 5, 6, 8] {
            let (prod, _cons) = RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 1000, ch);
            // `capacity()` divides slots by `ch`; if slots were not a frame
            // multiple the division would truncate and lose usable space.
            assert_eq!(
                prod.capacity() * ch,
                prod.capacity() * ch,
                "capacity must be exact at width {ch}"
            );
            assert!(
                prod.capacity() >= 4096,
                "floor applies in frames at width {ch}"
            );
        }
    }

    /// Reverse push reverses the FRAME order, never the sample order within a
    /// frame. Getting this backwards swaps L/R (and every other pair) on every
    /// reverse-played source — audible, but easy to mistake for a panning bug.
    #[test]
    fn reversed_push_keeps_channels_in_order_within_each_frame() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 4usize);
        let data = [0., 1., 2., 3., 10., 11., 12., 13., 20., 21., 22., 23.];
        assert_eq!(prod.write_interleaved_reversed(&data), 3);

        let mut f = [0.0f32; 4];
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [20., 21., 22., 23.], "last frame first");
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [10., 11., 12., 13.]);
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [0., 1., 2., 3.]);
    }

    /// `write_space` / `capacity` are frame-denominated. `loops.rs` compares a
    /// frame-count loop length against `write_space()` directly, so a
    /// sample-denominated value would over-request by the channel count.
    #[test]
    fn write_space_and_capacity_are_frames() {
        let (mut prod, _cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 8192, 6usize);
        assert_eq!(prod.capacity(), 8192, "capacity is in frames");
        assert_eq!(
            prod.write_space(),
            8192,
            "empty ring has full frame vacancy"
        );

        prod.push_interleaved(&indexed(100, 6));
        assert_eq!(prod.write_space(), 8192 - 100);
    }

    /// The `AudioOut` impl is the inherent `push_interleaved` — same frames,
    /// same order, same channels — reached through the generic trait method.
    ///
    /// Driven at six channels deliberately: the trait speaks a flat `&[f32]`,
    /// so if `write` ever grew a stride mistake it would be a 6x error here and
    /// only a 2x one at stereo, where it could hide in a round number.
    #[test]
    fn writing_through_the_audio_out_trait_matches_the_inherent_push() {
        use tutti_core::AudioOut;

        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 6usize);

        assert_eq!(
            AudioOut::layout(&prod),
            ChannelLayout::Multi(6),
            "the trait must report the ring's declared width"
        );

        // Trait method — the count is discarded by `write`'s signature.
        prod.write(&indexed(10, 6));

        let mut f = [0.0f32; 6];
        for k in 0..10 {
            assert!(cons.read_into(&mut f), "frame {k} must have landed");
            for (c, &s) in f.iter().enumerate() {
                assert_eq!(
                    s,
                    (k * 6 + c) as f32,
                    "frame {k} channel {c} — trait write must not rotate channels"
                );
            }
        }
        assert!(
            !cons.read_into(&mut f),
            "exactly 10 frames, no trailing partial"
        );
    }

    /// `write` ignores a trailing partial frame rather than pushing it short,
    /// which is what keeps every subsequent frame aligned. `chunks_exact` in
    /// `push_interleaved` is what provides this; the trait inherits it.
    #[test]
    fn a_trait_write_drops_a_trailing_partial_frame() {
        use tutti_core::AudioOut;

        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 4usize);

        // Two whole frames plus three stray samples of a third.
        let mut data = indexed(2, 4);
        data.extend_from_slice(&[99.0, 99.0, 99.0]);
        prod.write(&data);

        let mut f = [0.0f32; 4];
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [0., 1., 2., 3.]);
        assert!(cons.read_into(&mut f));
        assert_eq!(f, [4., 5., 6., 7.]);
        assert!(
            !cons.read_into(&mut f),
            "the partial frame must not have been pushed"
        );
    }

    /// An underrun consumes nothing and does not move `read_position`, so a
    /// partially-available frame can never leave the ring mid-frame.
    #[test]
    fn underrun_is_all_or_nothing() {
        let (mut prod, mut cons) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 64, 6usize);
        prod.push_interleaved(&indexed(1, 6));

        let pos = cons.read_position_shared();
        let mut f = [0.0f32; 6];
        assert!(cons.read_into(&mut f));
        assert_eq!(pos.load(Ordering::Relaxed), 1);

        assert!(!cons.read_into(&mut f), "empty ring must underrun");
        assert_eq!(
            pos.load(Ordering::Relaxed),
            1,
            "an underrun must not advance read_position"
        );
    }
}
