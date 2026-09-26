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
//! placing it. The reader looks the four taps a position needs up by position,
//! so it seats on the clock exactly as the memory tier does and reads what the
//! memory tier reads, at any rate; a jump inside the window costs nothing; and
//! a jump outside it is the butler moving the window to where the reader now
//! plays ([`Ring::play`]), with no flush, no stale frames to discard and no
//! read head to guess.
//!
//! The slots, the window and the protocol that keeps a write from ever
//! landing under a read are `tutti_core::PosRing`'s (its module docs have the
//! argument, and `tutti-types/tests/pos_ring_loom.rs` checks it under loom).
//! This module adds what a disk stream needs on top: the region, the file,
//! the map the reader reads the slots by, the preroll, a free-running
//! reader's origin and seek, and the rule of one live reader per ring.
//!
//! This replaced an SPSC FIFO the reader popped: the butler had to flush it to
//! move it, which dropped what it had refilled since, left the reader's head
//! unknowable while the flush was pending, and cost the reader its history —
//! doc 013, "The live disk loop and its repositions (#48)".
//!
//! # Where a ring is dropped
//!
//! The butler's writer holds one handle, every voice taken from the stream
//! another. A voice in a live graph is dropped where the graph frees units:
//! under the native executor, retired units travel back to the control thread
//! (`tutti_graph::Editor::collect`), so the last handle is never dropped on
//! the audio thread, and the ring's slots and its `RtPublish` are freed off it.
//!
//! # Frames, not samples
//!
//! Every count crossing this module is in **frames**; the stride never leaks
//! out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{
    ChannelLayout, PosRing, RingWindow, RtPublish, RtRef, Samples, MAX_POS_RING_FRAMES,
};
use tutti_io::Wave;

#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
use tutti_io::FileIn;

use super::command::RegionId;
use super::loops::{Arrangement, Content, Mapping, RingMap};
use crate::nonempty;

/// Frames a ring keeps behind where its reader plays: the taps a position
/// reads behind itself, and a little slack.
pub(crate) const HISTORY_FRAMES: u64 = 4;

/// The window a claim returns: the positions the reader may read this block.
pub(crate) type Window = RingWindow;

/// Why a live voice could not be taken from a channel's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeVoiceError {
    /// The channel streams nothing yet: the butler has not applied its
    /// `Command::Stream`. Poll again next frame.
    NotStreaming,
    /// The stream already has its live reader. A ring serves one: the butler
    /// fills ahead of the one position it is told (`Ring::play`), so two
    /// readers at two positions would pull its window back and forth, each
    /// starving the other. Take one voice per stream, or stream the file again
    /// on another channel.
    ReaderTaken,
}

impl std::fmt::Display for TakeVoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotStreaming => "the channel streams nothing yet",
            Self::ReaderTaken => {
                "the stream already has a live reader; a ring serves one (stream the file \
                 again on another channel for a second voice)"
            }
        })
    }
}

impl std::error::Error for TakeVoiceError {}

/// A region's ring, shared by the butler's writer and the audio thread's
/// reader.
pub(crate) struct Ring {
    region_id: RegionId,
    file_path: PathBuf,
    channels: ChannelLayout,
    /// The slots, the window, the reader's claims.
    core: PosRing,
    /// Butler → reader: the channel's PDC preroll, in file frames: the reader
    /// plays straight position `gate - preroll`.
    preroll: AtomicU64,
    /// Butler → reader: where a free-running reader starts (the stream's
    /// offset).
    origin: AtomicU64,
    /// Butler → reader: a free-running reader's seek, `(epoch, target)`.
    seek_epoch: AtomicU64,
    seek_target: AtomicU64,
    /// Butler → reader: output frames a reader's crossfade lasts.
    fade_frames: AtomicU64,
    /// Butler → reader: how to read the slots.
    map: RtPublish<RingMap>,
    /// A live reader has been taken (one per ring, [`TakeVoiceError`]).
    taken: AtomicBool,
    /// The live reader is a placed voice (it follows its clock, so a relayed
    /// seek must not move where the butler fills).
    placed: AtomicBool,
}

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring")
            .field("region_id", &self.region_id)
            .field("core", &self.core)
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
        self.core.frames()
    }

    /// The positions held.
    #[inline]
    pub(crate) fn window(&self) -> (u64, u64) {
        let w = self.core.window();
        (w.from, w.to)
    }

    /// Audio thread, once per block before any read: where the reader plays,
    /// which positions the block may read, and the window it may read them in
    /// (`PosRing::claim`).
    #[inline]
    pub(crate) fn claim(&self, play: u64, reads: [(u64, u64); 2]) -> Window {
        self.core.claim(play, reads)
    }

    /// Channel `c` of the frame at position `s`, which the caller's claimed
    /// window and range hold.
    #[inline]
    pub(crate) fn sample(&self, s: u64, c: usize) -> f32 {
        self.core.sample(s, c)
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

    /// Output frames a reader's crossfade lasts.
    #[inline]
    pub(crate) fn fade_frames(&self) -> usize {
        self.fade_frames.load(Ordering::Relaxed) as usize
    }

    /// Where the reader plays.
    pub(crate) fn play(&self) -> u64 {
        self.core.play()
    }

    /// One past the last position the block in flight may read.
    pub(crate) fn in_flight_end(&self) -> u64 {
        self.core.in_flight_end()
    }

    /// Butler: drop every position at and past `x` (a rewrite from `x`
    /// follows).
    pub(crate) fn retract_to(&self, x: u64) {
        self.core.retract_to(x);
    }

    /// Butler: drop every position below `y` (so a reader that jumps back
    /// there moves the window rather than read what lies below).
    pub(crate) fn raise_from(&self, y: u64) {
        self.core.raise_from(y);
    }

    /// Butler: an empty window at `start` (the reader jumped outside it).
    pub(crate) fn reset(&self, start: u64) {
        self.core.reset(start);
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
        self.core.set_play(play);
    }

    /// Butler: seek a free-running reader to `target`.
    pub(crate) fn request_seek(&self, target: u64) {
        self.seek_target.store(target, Ordering::Relaxed);
        self.seek_epoch.fetch_add(1, Ordering::Release);
    }

    /// Take the ring's one live reader, `placed` when it is a voice that
    /// follows a clock.
    pub(crate) fn take_reader(&self, placed: bool) -> Result<(), TakeVoiceError> {
        if self.taken.swap(true, Ordering::AcqRel) {
            return Err(TakeVoiceError::ReaderTaken);
        }
        self.placed.store(placed, Ordering::Release);
        Ok(())
    }

    /// Whether the live reader follows a clock.
    pub(crate) fn placed(&self) -> bool {
        self.placed.load(Ordering::Acquire)
    }

    /// Window shrinks and moves so far (the ring's generation).
    #[cfg(test)]
    pub(crate) fn moves(&self) -> u64 {
        self.core.generation()
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
        #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
        if let Some(decoder) = self.decoder.as_mut() {
            let ch = self.ring.core.stride();
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
        let ch = self.ring.core.stride();
        mapping.fill(pos, out, ch, &mut |at, run| {
            let _ = self.read_file(at, run);
        });
    }

    /// Append frames at the window's end, returning how many **frames**
    /// landed: short where a slot the reader's block in flight may read would
    /// be written, or where the ring is full ahead of the reader
    /// (`PosRing::push`). A trailing partial frame is ignored.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Samples {
        Samples(self.ring.core.push(samples))
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
    /// [`MAX_POS_RING_FRAMES`]), holding nothing, read as an unlooped forward
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
        let frames = capacity.clamp(4096, MAX_POS_RING_FRAMES);
        let mapping = Mapping::plain(len);
        let ring = Arc::new(Ring {
            region_id,
            file_path,
            channels,
            core: PosRing::new(frames, stride, HISTORY_FRAMES),
            preroll: AtomicU64::new(0),
            origin: AtomicU64::new(0),
            seek_epoch: AtomicU64::new(0),
            seek_target: AtomicU64::new(0),
            fade_frames: AtomicU64::new(0),
            map: RtPublish::new(RingMap::plain(Arrangement::plain(len), 0)),
            taken: AtomicBool::new(false),
            placed: AtomicBool::new(false),
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
        ring.core.reset(start.saturating_sub(HISTORY_FRAMES));
        ring.core.set_play(start);
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

    /// **Frames written through the `AudioOut` trait land by position**,
    /// every channel in its place at six channels, a trailing partial frame
    /// dropped. (The ring's own protocol is `PosRing`'s tests and loom model.)
    ///
    /// Mutation (run): `write` pushing nothing → fails.
    #[test]
    fn frames_written_through_the_trait_land_by_position() {
        let (mut writer, reader) = ring(64, 6);
        assert_eq!(AudioOut::layout(&writer), ChannelLayout::from(6u16));
        let mut data = indexed(12, 6);
        data.extend_from_slice(&[99.0, 99.0, 99.0]);
        writer.write(&data);
        assert_eq!(reader.window(), (0, 12), "twelve whole frames");
        for s in 0..12u64 {
            for c in 0..6 {
                assert_eq!(
                    reader.sample(s, c),
                    (s as usize * 6 + c) as f32,
                    "frame {s}"
                );
            }
        }
    }

    /// **A ring holds a whole number of frames**: the capacity asked for,
    /// floored at 4096 and capped at what the window word can say.
    #[test]
    fn a_ring_holds_a_whole_number_of_frames() {
        assert_eq!(ring(10, 2).1.frames(), 4_096);
        assert_eq!(ring(10_000, 6).1.frames(), 10_000);
        assert_eq!(ring(usize::MAX >> 8, 1).1.frames(), MAX_POS_RING_FRAMES);
    }

    /// **A ring serves one live reader** (the second review of #48, S2):
    /// taking a second is refused, naming why.
    ///
    /// Mutation (run): `take_reader` not checking → fails.
    #[test]
    fn a_ring_serves_one_live_reader() {
        let (_writer, reader) = ring(64, 2);
        assert_eq!(reader.take_reader(true), Ok(()));
        assert!(reader.placed());
        assert_eq!(reader.take_reader(false), Err(TakeVoiceError::ReaderTaken));
        assert!(reader.placed(), "a refused take changes nothing");
    }
}
