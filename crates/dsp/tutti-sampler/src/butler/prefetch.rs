//! A stream's ring: file frames the butler has read ahead, indexed by
//! position.
//!
//! One ring per streaming region. The butler writes frames into it
//! ([`RegionOut`]); the audio thread reads them ([`Ring`], through a
//! [`SharedReader`]). Nothing on the reading side locks, allocates or waits.
//!
//! # Indexed by straight position, not consumed in order
//!
//! Slot `s mod N` holds what straight position `s` holds under the stream's
//! mapping (`loops`' module docs): the file counted straight on, a loop
//! placing it. The ring publishes the window of positions it holds,
//! `[from, to)`, as one packed atomic word, and the reader looks the four taps
//! a position needs up by position. So the reader seats on the clock exactly
//! as the memory tier does and reads what the memory tier reads, at any rate;
//! a jump inside the window costs nothing; and a jump outside it is the butler
//! moving the window to where the reader now plays ([`Ring::play`]), with no
//! flush, no stale frames to discard and no read head to guess.
//!
//! This replaced an SPSC FIFO the reader popped: the butler had to flush it to
//! move it, which dropped what it had refilled since, left the reader's head
//! unknowable while the flush was pending, and cost the reader its history —
//! doc 013, "The live disk reposition (after #48)".
//!
//! # Why a write never tears a read
//!
//! A write of position `s` reuses the slot of `s - N`. Before it, the butler
//! raises the window's start past `s - N` (so no later block reads it) and
//! then loads the ranges the reader's block in flight may read, which the
//! reader stores *before* it loads the window (all `SeqCst`). Either the
//! reader's load saw the raised start, or its ranges were stored before the
//! butler's load, which then sees them: the butler writes no slot those ranges
//! reach ([`RegionOut::push_interleaved`]). The samples are `AtomicU32`s all
//! the same, so a protocol slip is a wrong sample, never undefined behaviour.
//!
//! # Frames, not samples
//!
//! Every count crossing this module is in **frames**; the stride never leaks
//! out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{ChannelLayout, RtPublish, RtRef, Samples};
use tutti_io::Wave;

#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
use tutti_io::FileIn;

use super::command::RegionId;
use super::loops::{Arrangement, Content, Mapping, RingMap};
use crate::nonempty;

/// Bits of the packed window word that hold its length; the rest hold its end.
const LEN_BITS: u32 = 24;

/// Frames a ring keeps behind where its reader plays: the taps a position
/// reads behind itself, and a little slack.
pub(crate) const HISTORY_FRAMES: u64 = 4;

/// The most frames a ring holds: its window's length must fit [`LEN_BITS`].
pub(crate) const MAX_RING_FRAMES: usize = (1 << LEN_BITS) - 1;

/// A window of straight positions `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) from: u64,
    pub(crate) to: u64,
}

impl Window {
    fn pack(self) -> u64 {
        (self.to << LEN_BITS) | (self.to - self.from)
    }

    fn unpack(word: u64) -> Self {
        let to = word >> LEN_BITS;
        Self {
            from: to - (word & ((1 << LEN_BITS) - 1)),
            to,
        }
    }

    /// Whether position `s` is held.
    #[inline]
    pub(crate) fn holds(&self, s: u64) -> bool {
        s >= self.from && s < self.to
    }
}

/// A region's ring, shared by the butler's writer and the audio thread's
/// reader.
pub(crate) struct Ring {
    region_id: RegionId,
    file_path: PathBuf,
    channels: ChannelLayout,
    /// `channels.count()`, cached — the interleave stride of `slots`.
    stride: usize,
    /// Slots, `N`.
    frames: usize,
    /// `frames * stride` samples, as `f32` bits.
    slots: Box<[AtomicU32]>,
    /// The packed [`Window`] of positions the slots hold.
    window: AtomicU64,
    /// Reader → butler: the straight position the reader plays (what the
    /// butler fills ahead of).
    play: AtomicU64,
    /// Reader → butler: the ranges the block in flight may read,
    /// `[a0, e0)` and `[a1, e1)`.
    reads: [AtomicU64; 4],
    /// Butler → reader: the channel's PDC preroll, in file frames: the reader
    /// plays straight position `gate - preroll`.
    preroll: AtomicU64,
    /// Butler → reader: where a free-running reader starts (the stream's
    /// offset).
    origin: AtomicU64,
    /// Butler → reader: a free-running reader's seek, `(epoch, target)`.
    seek_epoch: AtomicU64,
    seek_target: AtomicU64,
    /// Butler → reader: output frames a reader's seek crossfade lasts.
    fade_frames: AtomicU64,
    /// Butler → reader: how to read the slots.
    map: RtPublish<RingMap>,
    /// Butler-side count of window moves and retractions (tests).
    #[cfg(test)]
    moves: AtomicU64,
}

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring")
            .field("region_id", &self.region_id)
            .field("frames", &self.frames)
            .field("window", &self.window())
            .finish_non_exhaustive()
    }
}

impl Ring {
    pub(crate) fn region_id(&self) -> RegionId {
        self.region_id
    }

    /// Declared width — the file's own layout.
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Slots, in frames.
    pub(crate) fn frames(&self) -> usize {
        self.frames
    }

    /// The positions held.
    #[inline]
    pub(crate) fn window(&self) -> (u64, u64) {
        let w = Window::unpack(self.window.load(Ordering::SeqCst));
        (w.from, w.to)
    }

    /// Audio thread, once per block before any read: say where the reader
    /// plays and which positions the block may read, then take the window it
    /// may read them in. The order is the no-tear argument in the module docs.
    #[inline]
    pub(crate) fn claim(&self, play: u64, reads: [(u64, u64); 2]) -> Window {
        self.play.store(play, Ordering::SeqCst);
        for (i, (a, e)) in reads.into_iter().enumerate() {
            self.reads[2 * i].store(a, Ordering::SeqCst);
            self.reads[2 * i + 1].store(e, Ordering::SeqCst);
        }
        Window::unpack(self.window.load(Ordering::SeqCst))
    }

    /// Channel `c` of the frame at position `s`. The caller holds `s` in a
    /// claimed window and range.
    #[inline]
    pub(crate) fn sample(&self, s: u64, c: usize) -> f32 {
        let slot = (s % self.frames as u64) as usize;
        f32::from_bits(self.slots[slot * self.stride + c].load(Ordering::Relaxed))
    }

    /// How to read the slots, for this block (once per block, never per
    /// sample; never parked across blocks).
    #[inline]
    pub(crate) fn map(&self) -> RtRef<'_, RingMap> {
        self.map.read()
    }

    /// The channel's PDC preroll, file frames.
    #[inline]
    pub(crate) fn preroll(&self) -> u64 {
        self.preroll.load(Ordering::Relaxed)
    }

    /// Where a free-running reader starts.
    pub(crate) fn origin(&self) -> u64 {
        self.origin.load(Ordering::Relaxed)
    }

    /// A free-running reader's pending seek, `(epoch, target)`.
    #[inline]
    pub(crate) fn seek_request(&self) -> (u64, u64) {
        let epoch = self.seek_epoch.load(Ordering::Acquire);
        (epoch, self.seek_target.load(Ordering::Relaxed))
    }

    /// Output frames a reader's seek crossfade lasts.
    #[inline]
    pub(crate) fn fade_frames(&self) -> usize {
        self.fade_frames.load(Ordering::Relaxed) as usize
    }

    /// Where the reader plays.
    pub(crate) fn play(&self) -> u64 {
        self.play.load(Ordering::SeqCst)
    }

    /// One past the last position the block in flight may read.
    pub(crate) fn in_flight_end(&self) -> u64 {
        self.reads[1]
            .load(Ordering::SeqCst)
            .max(self.reads[3].load(Ordering::SeqCst))
    }

    fn in_flight(&self) -> [(u64, u64); 2] {
        let r = |i: usize| self.reads[i].load(Ordering::SeqCst);
        [(r(0), r(1)), (r(2), r(3))]
    }

    fn store_window(&self, window: Window) {
        #[cfg(test)]
        if Window::unpack(self.window.load(Ordering::SeqCst)).to > window.to {
            self.moves.fetch_add(1, Ordering::Relaxed);
        }
        self.window.store(window.pack(), Ordering::SeqCst);
    }

    /// Butler: drop every position at and past `x` from the window (a rewrite
    /// from `x` follows).
    pub(crate) fn retract_to(&self, x: u64) {
        let (from, to) = self.window();
        if x < to {
            self.store_window(Window {
                from: from.min(x),
                to: x,
            });
        }
    }

    /// Butler: an empty window at `start` (the reader jumped outside it).
    pub(crate) fn reset(&self, start: u64) {
        #[cfg(test)]
        self.moves.fetch_add(1, Ordering::Relaxed);
        self.window.store(
            Window {
                from: start,
                to: start,
            }
            .pack(),
            Ordering::SeqCst,
        );
    }

    /// Butler: publish how to read the slots.
    pub(crate) fn publish_map(&self, map: RingMap) {
        self.map.publish(Arc::new(map));
    }

    /// Butler: the channel's PDC preroll.
    pub(crate) fn set_preroll(&self, preroll: u64) {
        self.preroll.store(preroll, Ordering::Relaxed);
    }

    /// Butler: where the reader plays, before one has said (a stream's start,
    /// a free-running reader's seek), so the window is filled there.
    pub(crate) fn set_play(&self, play: u64) {
        self.play.store(play, Ordering::SeqCst);
    }

    /// Butler: seek a free-running reader to `target` (a placed voice follows
    /// its clock instead).
    pub(crate) fn request_seek(&self, target: u64) {
        self.seek_target.store(target, Ordering::Relaxed);
        self.seek_epoch.fetch_add(1, Ordering::Release);
    }

    /// Window moves and retractions so far (tests).
    #[cfg(test)]
    pub(crate) fn moves(&self) -> u64 {
        self.moves.load(Ordering::Relaxed)
    }
}

/// The audio thread's handle on a region's ring.
pub(crate) type SharedReader = Arc<Ring>;

/// The butler's end of a region's ring: the writer, its source (a disk
/// decoder, or the file resident whole) and what it holds (`loops::Content`).
/// Butler-thread-local.
pub(crate) struct RegionOut {
    ring: Arc<Ring>,
    content: Content,
    /// Incremental disk decoder for real streaming. `None` means this region
    /// reads the file resident whole (`resident`).
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    decoder: Option<FileIn>,
    /// The file decoded whole, for a format that cannot seek. Held here, not
    /// only in the LRU cache, so a file too large to cache still streams.
    resident: Option<Arc<Wave>>,
}

impl RegionOut {
    /// The shared ring.
    pub(crate) fn ring(&self) -> &Arc<Ring> {
        &self.ring
    }

    /// What the ring holds.
    pub(crate) fn content(&self) -> &Content {
        &self.content
    }

    pub(crate) fn set_content(&mut self, content: Content) {
        self.content = content;
    }

    /// Install an incremental disk decoder.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    pub(crate) fn set_decoder(&mut self, decoder: FileIn) {
        self.decoder = Some(decoder);
    }

    /// Hold the file resident whole (a format that cannot seek).
    pub(crate) fn set_resident(&mut self, wave: Arc<Wave>) {
        self.resident = Some(wave);
    }

    /// Declared ring width — the file's own layout.
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.ring.channels
    }

    /// Frames held ahead of where the reader plays.
    pub(crate) fn buffered(&self) -> Samples {
        let (from, to) = self.ring.window();
        let play = self.ring.play().max(from);
        Samples(to.saturating_sub(play) as usize)
    }

    /// The file this region streams.
    pub(crate) fn file_path(&self) -> &Path {
        &self.ring.file_path
    }

    pub(crate) fn region_id(&self) -> RegionId {
        self.ring.region_id
    }

    /// Read file frames `at..` into `out` (flat, the ring's width), from the
    /// decoder or the resident file; zero past the end. `false` when the
    /// file could not be read there.
    pub(crate) fn read_file(&mut self, at: usize, out: &mut [f32]) -> bool {
        let channels = self.ring.channels;
        let ch = self.ring.stride;
        #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
        if let Some(decoder) = self.decoder.as_mut() {
            if decoder.cursor() != at as u64 && decoder.seek(at as u64).is_err() {
                out.fill(0.0);
                return false;
            }
            return match decoder.fill_sequential_interleaved(out) {
                Ok(got) => {
                    out[got * ch..].fill(0.0);
                    true
                }
                Err(_) => {
                    out.fill(0.0);
                    false
                }
            };
        }
        match self.resident.as_deref() {
            Some(wave) => {
                super::loops::read_wave(wave, at, out, channels);
                true
            }
            None => {
                out.fill(0.0);
                false
            }
        }
    }

    /// Fill `out` with what straight positions `pos..` hold under `mapping`,
    /// reading the file.
    pub(crate) fn fill_with(&mut self, mapping: &Mapping, pos: u64, out: &mut [f32]) {
        let ch = self.ring.stride;
        mapping.fill(pos, out, ch, &mut |at, run| {
            let _ = self.read_file(at, run);
        });
    }

    /// Append frames at the window's end, returning how many **frames**
    /// landed: short when a slot the reader's block in flight may read would
    /// be reused (see the module docs), or the ring is full ahead of the
    /// reader. A trailing partial frame is ignored.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Samples {
        let ring = &*self.ring;
        let ch = ring.stride;
        let n = (samples.len() / ch) as u64;
        let frames = ring.frames as u64;
        let (from, to) = ring.window();
        let play = ring.play();
        // Never reuse the slot of a position the reader still stands on.
        let room = (play.saturating_sub(4) + frames).saturating_sub(to);
        let mut end = to + n.min(room);
        let new_from = from.max(end.saturating_sub(frames));
        if new_from != from {
            ring.store_window(Window { from: new_from, to });
        }
        for (a, e) in ring.in_flight() {
            end = end.min(first_alias(to, a, e, frames));
        }
        if end <= to {
            return Samples(0);
        }
        for (i, frame) in samples
            .chunks_exact(ch)
            .take((end - to) as usize)
            .enumerate()
        {
            let slot = ((to + i as u64) % frames) as usize;
            for (c, &s) in frame.iter().enumerate() {
                ring.slots[slot * ch + c].store(s.to_bits(), Ordering::Relaxed);
            }
        }
        ring.store_window(Window {
            from: new_from.min(end),
            to: end,
        });
        Samples((end - to) as usize)
    }
}

/// The first position at or past `to` whose slot also holds a position of the
/// reader's range `[a, e)` other than itself — an alias `[a + kN, e + kN)`,
/// `k != 0`, of a ring `n` slots long. Writing a position *in* the range is
/// no conflict: the reader only reads it once the window holds it.
fn first_alias(to: u64, a: u64, e: u64, n: u64) -> u64 {
    if e <= a {
        return u64::MAX;
    }
    let len = e - a;
    let d = (to as i128 - a as i128).rem_euclid(n as i128) as u64;
    if d < len {
        return if (a..e).contains(&to) { a + n } else { to };
    }
    let next = to + (n - d);
    if next == a {
        a + n
    } else {
        next
    }
}

/// The ring is the engine's [`AudioOut`](tutti_core::AudioOut) shape: flat
/// interleaved in, a runtime [`ChannelLayout`] for the width, counts in
/// frames. `write` appends at the window's end; the inherent
/// [`push_interleaved`](RegionOut::push_interleaved) stays the one production
/// callers use, because it returns what landed.
impl tutti_core::AudioOut<f32> for RegionOut {
    fn layout(&self) -> ChannelLayout {
        self.ring.channels
    }

    /// Append frames, discarding the landed count.
    fn write(&mut self, interleaved: &[f32]) {
        let _ = self.push_interleaved(interleaved);
    }

    /// No-op: the ring is live, never a stream that gets closed.
    fn finalize(self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Constructor namespace for a region's ring.
pub(crate) struct RegionBuffer;

impl RegionBuffer {
    /// Build a region's ring of `capacity` **frames** of `channels` each
    /// (floored at 4096 frames and one channel, capped at
    /// [`MAX_RING_FRAMES`]), holding nothing, read as an unlooped forward
    /// stream of a file `len` frames long (`usize::MAX` for a length not
    /// known). Returns the writer and the reader's handle.
    #[cfg(test)]
    pub(crate) fn with_capacity(
        region_id: RegionId,
        file_path: PathBuf,
        capacity: usize,
        channels: impl Into<ChannelLayout>,
    ) -> (RegionOut, SharedReader) {
        Self::for_file(region_id, file_path, capacity, channels, usize::MAX)
    }

    /// [`with_capacity`](Self::with_capacity) for a file `len` frames long.
    pub(crate) fn for_file(
        region_id: RegionId,
        file_path: PathBuf,
        capacity: usize,
        channels: impl Into<ChannelLayout>,
        len: usize,
    ) -> (RegionOut, SharedReader) {
        let channels = nonempty(channels.into());
        let stride = channels.count() as usize;
        let frames = capacity.clamp(4096, MAX_RING_FRAMES);
        let mapping = Mapping::plain(len);
        let ring = Arc::new(Ring {
            region_id,
            file_path,
            channels,
            stride,
            frames,
            slots: (0..frames * stride).map(|_| AtomicU32::new(0)).collect(),
            window: AtomicU64::new(0),
            play: AtomicU64::new(0),
            reads: std::array::from_fn(|_| AtomicU64::new(0)),
            preroll: AtomicU64::new(0),
            origin: AtomicU64::new(0),
            seek_epoch: AtomicU64::new(0),
            seek_target: AtomicU64::new(0),
            fade_frames: AtomicU64::new(0),
            map: RtPublish::new(RingMap::plain(Arrangement::plain(len), 0)),
            #[cfg(test)]
            moves: AtomicU64::new(0),
        });
        let writer = RegionOut {
            ring: Arc::clone(&ring),
            content: Content::new(mapping),
            #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
            decoder: None,
            resident: None,
        };
        (writer, ring)
    }

    /// Point a fresh ring at `start`: the reader there and the window from
    /// just behind it (the taps a position reads one frame back), a
    /// free-running reader's origin, the preroll and the seek fade set.
    pub(crate) fn place(ring: &Ring, start: u64, origin: u64, preroll: u64, fade_frames: usize) {
        let from = start.saturating_sub(HISTORY_FRAMES);
        ring.window
            .store(Window { from, to: from }.pack(), Ordering::SeqCst);
        ring.play.store(start, Ordering::SeqCst);
        ring.origin.store(origin, Ordering::Relaxed);
        ring.preroll.store(preroll, Ordering::Relaxed);
        ring.fade_frames
            .store(fade_frames as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::AudioOut;

    /// `frames` frames at `channels`, sample `c` of frame `f` carrying
    /// `f * channels + c`, so a rotation or a tear is a wrong value.
    fn indexed(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| i as f32).collect()
    }

    fn ring(capacity: usize, channels: usize) -> (RegionOut, SharedReader) {
        RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), capacity, channels)
    }

    fn frame_at(ring: &Ring, s: u64) -> Vec<f32> {
        (0..ring.stride).map(|c| ring.sample(s, c)).collect()
    }

    /// **The window packs into one word and back**, up to its largest length
    /// and far along the straight line.
    ///
    /// Mutation (run): the length mask one bit short → the longest window
    /// unpacks short → fails.
    #[test]
    fn a_window_packs_into_one_word_and_back() {
        let far = 1u64 << 39;
        for (from, to) in [(0, 0), (5, 4_100), (far, far + MAX_RING_FRAMES as u64)] {
            let w = Window { from, to };
            assert_eq!(Window::unpack(w.pack()), w);
        }
    }

    /// **Frames land at the window's end and read back by position**, every
    /// channel in its place at six channels — through the inherent push and
    /// through the `AudioOut` trait — and a trailing partial frame is dropped.
    ///
    /// Mutation (run): the slot index taken in samples (`slot * ch` omitted) →
    /// channels rotate → fails.
    #[test]
    fn frames_land_at_the_windows_end_and_read_back_by_position() {
        let (mut writer, reader) = ring(64, 6);
        assert_eq!(AudioOut::layout(&writer), ChannelLayout::from(6u16));
        assert_eq!(writer.push_interleaved(&indexed(10, 6)), Samples(10));
        let mut more = indexed(12, 6)[60..].to_vec();
        more.extend_from_slice(&[99.0, 99.0, 99.0]);
        writer.write(&more);
        assert_eq!(reader.window(), (0, 12), "twelve whole frames");
        for s in 0..12 {
            let want: Vec<f32> = (0..6).map(|c| (s as usize * 6 + c) as f32).collect();
            assert_eq!(frame_at(&reader, s), want, "frame {s}");
        }
    }

    /// **A ring holds a whole number of frames**: the capacity asked for,
    /// floored at 4096 and capped at what the window word can say.
    #[test]
    fn a_ring_holds_a_whole_number_of_frames() {
        assert_eq!(ring(10, 2).1.frames(), 4_096);
        assert_eq!(ring(10_000, 6).1.frames(), 10_000);
        assert_eq!(ring(usize::MAX >> 8, 1).1.frames(), MAX_RING_FRAMES);
    }

    /// **A write never goes round the ring onto the reader**: with the
    /// reader at 1 000, a ring of 4 096 takes positions up to 1 000 - 4 +
    /// 4 096 and no further, so the frames the reader stands on (and the few
    /// behind it its taps read) are intact.
    ///
    /// Mutation (run): the room check removed → the push wraps onto 996.. →
    /// fails.
    #[test]
    fn a_write_never_goes_round_the_ring_onto_the_reader() {
        let (mut writer, reader) = ring(4_096, 1);
        writer.push_interleaved(&indexed(1_000, 1));
        reader.set_play(1_000);
        let landed = writer.push_interleaved(&indexed(8_000, 1));
        assert_eq!(landed, Samples(4_096 - 4), "up to 996 + 4 096");
        assert_eq!(reader.window().1, 1_000 - 4 + 4_096);
        assert_eq!(frame_at(&reader, 996), [996.0]);
    }

    /// **A write never reuses a slot the block in flight reads**: a reader
    /// whose block reads `[3 997, 4 069)` and whose jump copy reads `[100,
    /// 200)` keeps both — the writer stops at 4 196, the first position whose
    /// slot is 100's — while it writes the block's own positions freely.
    ///
    /// Mutation (run): the in-flight check removed → 4 196.. overwrite 100.. →
    /// fails. Mutation (run): a position *in* the range counted as its own
    /// alias (the first cut of `first_alias`) → the writer stops at once →
    /// fails.
    #[test]
    fn a_write_never_reuses_a_slot_the_block_in_flight_reads() {
        let (mut writer, reader) = ring(4_096, 1);
        writer.push_interleaved(&indexed(4_000, 1));
        let window = reader.claim(3_997, [(3_997, 4_069), (100, 200)]);
        assert_eq!(window, Window { from: 0, to: 4_000 });
        let landed = writer.push_interleaved(&vec![-1.0; 300]);
        assert_eq!(landed, Samples(196), "stops at 4 196");
        assert_eq!(
            frame_at(&reader, 150),
            [150.0],
            "the copy's frames are intact"
        );
    }

    /// `first_alias` against a brute-force search, over ranges before, around
    /// and after the write's start, near and across a lap of the ring.
    ///
    /// Mutation (run): `a + n` returned as `a` for a range containing the
    /// start → fails.
    #[test]
    fn first_alias_is_the_first_slot_another_position_of_the_range_holds() {
        let n = 16u64;
        for to in 0..64u64 {
            for a in 0..64u64 {
                for len in 0..8u64 {
                    let e = a + len;
                    let brute = (to..to + 3 * n)
                        .find(|&s| (a..e).any(|q| q != s && q % n == s % n))
                        .unwrap_or(u64::MAX);
                    let got = first_alias(to, a, e, n);
                    assert!(
                        got == brute || (brute == u64::MAX && got >= to + 3 * n),
                        "to {to} range [{a}, {e}): {got}, brute {brute}"
                    );
                }
            }
        }
    }

    /// **A reset empties the window where the reader jumped; a retraction
    /// drops what lies past a switch.**
    #[test]
    fn a_reset_and_a_retraction_move_the_window() {
        let (mut writer, reader) = ring(4_096, 1);
        writer.push_interleaved(&indexed(1_000, 1));
        reader.retract_to(600);
        assert_eq!(reader.window(), (0, 600));
        reader.reset(9_000);
        assert_eq!(reader.window(), (9_000, 9_000));
        assert_eq!(reader.moves(), 2);
    }
}
