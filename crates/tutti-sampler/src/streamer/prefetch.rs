//! A stream's ring: file frames the streamer has read ahead, indexed by
//! position.
//!
//! One ring per streaming region. The streamer writes frames into it
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
//! a jump outside it is the streamer moving the window to where the reader now
//! plays (`Ring::play`), with no flush, no stale frames to discard and no
//! read head to guess.
//!
//! The slots, the window and the protocol that keeps a write from ever
//! landing under a read are `tutti_rt::PosRing`'s (its module docs have the
//! argument, and `tutti-rt/tests/pos_ring_loom.rs` checks it under loom).
//! Its one `PosWriter` is the streamer's [`RegionOut`]'s; its one `PosReader`
//! is handed out by value, once, with the stream's open
//! ([`Opening::poll`](crate::Opening::poll)), so one live reader per ring is the
//! type's rule, not a flag's. This module
//! adds what a disk stream needs on top: the region, the file, the map the
//! reader reads the slots by (and the direction and applied loop beside it: the
//! ring's one published `RingControl`).
//!
//! A position-indexed ring rather than an SPSC FIFO the reader pops: a FIFO
//! has to be flushed to move, which drops what was refilled since, leaves the
//! reader's head unknowable while the flush is pending, and costs the reader
//! its history.
//!
//! # Where a ring is dropped
//!
//! The streamer's writer holds one handle, every voice taken from the stream
//! another. A voice in a live graph is dropped where the graph frees units:
//! under the graph's executor, retired units travel back to the control thread
//! (`tutti_graph::Editor::collect`), so the last handle is never dropped on
//! the audio thread, and the ring's slots and its `ArcLend` are freed off it.
//!
//! # Frames, not samples
//!
//! Every count crossing this module is in frames; the stride never leaks
//! out.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_codec::Wave;
use tutti_rt::{ArcLend, MAX_POS_RING_FRAMES, PosReader, PosRing, PosWriter, Reader};
use tutti_types::{ChannelLayout, ChannelTopology, Samples};

#[cfg(codec)]
use tutti_codec::FileIn;

use super::command::StreamId;
use super::loops::{Arrangement, Content, Mapping, RingControl, RingMap};
use crate::nonempty;
use crate::voice::LoopSetting;

/// Frames a ring keeps behind where its reader plays: the taps a position
/// reads behind itself, and a little slack.
pub(crate) const HISTORY_FRAMES: u64 = 4;

/// A region's ring, shared by the streamer's writer and the audio thread's
/// reader.
pub(crate) struct Ring {
    region_id: StreamId,
    file_path: PathBuf,
    channels: ChannelLayout,
    /// Which speaker each of the file's channels feeds. The ring itself is at
    /// the file's width and order; the placement the voice reads it by is built
    /// when the stream opens (`Opened::place`), so only tests read this.
    #[cfg(test)]
    topology: ChannelTopology,
    /// Glances at the window and where the reader plays (the writer's
    /// `watch`): no sample is read through it.
    core: Arc<PosRing>,
    /// Output frames a reader's crossfade lasts: the config's seek crossfade,
    /// fixed when the ring is placed.
    fade_frames: usize,
    /// Streamer → reader: how to read the slots, the direction and the
    /// applied loop, in one value, lent through the reader the voice registers
    /// ([`control_reader`](Self::control_reader)). The streamer is its only
    /// publisher.
    control: Arc<ArcLend<RingControl>>,
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
    pub(crate) fn region_id(&self) -> StreamId {
        self.region_id
    }

    /// Declared width — the file's own layout.
    pub(crate) fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The speakers the file's channels feed, in the ring's order (tests).
    #[cfg(test)]
    pub(crate) fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// Slots, in frames.
    #[cfg(test)]
    pub(crate) fn frames(&self) -> usize {
        self.core.frames()
    }

    /// The positions held: a glance (tests), not a claim.
    #[cfg(test)]
    pub(crate) fn window(&self) -> (u64, u64) {
        let w = self.core.window();
        (w.from, w.to)
    }

    /// A reader of the ring's control, registered for the voice that reads
    /// this ring: it lends once per block, never per sample, never parked
    /// across blocks. Control thread: registering may allocate.
    pub(crate) fn control_reader(&self) -> Reader<RingControl> {
        self.control.reader()
    }

    /// The ring's control cell, for the stream's handle to read the applied
    /// loop from. Control side.
    pub(crate) fn control_cell(&self) -> Arc<ArcLend<RingControl>> {
        Arc::clone(&self.control)
    }

    /// The control as the streamer last published it: what an export fork of
    /// the voice reads (the loop as applied, the direction). Control thread: it
    /// takes the cell's lock and holds an owning `Arc`, which frees wherever it
    /// drops last.
    pub(crate) fn applied(&self) -> Arc<RingControl> {
        self.control.current()
    }

    /// Output frames a reader's crossfade lasts.
    #[inline]
    pub(crate) fn fade_frames(&self) -> usize {
        self.fade_frames
    }

    /// Where the reader plays: a glance (tests).
    #[cfg(test)]
    pub(crate) fn play(&self) -> u64 {
        self.core.play()
    }

    /// Streamer: publish how to read the slots, with the direction that map
    /// implies and the loop as last applied.
    pub(crate) fn publish_map(&self, map: RingMap) {
        let loop_ = self.control.current().loop_;
        self.control.publish(RingControl::new(map, loop_));
    }

    /// Streamer: publish `loop_` as the loop the stream runs, beside the map
    /// and direction already published. Nothing is published when it is the
    /// loop already there.
    pub(crate) fn publish_loop(&self, loop_: LoopSetting) {
        let now = self.control.current();
        if now.loop_ == loop_ {
            return;
        }
        self.control
            .publish(RingControl::new(now.map.clone(), loop_));
    }

    /// Streamer: free the controls the reader has let go of since the last
    /// publish, without publishing. A ring whose control stops changing would
    /// otherwise keep one a block held across its last publish for as long as
    /// the ring lives.
    pub(crate) fn reclaim_control(&self) -> usize {
        self.control.reclaim()
    }

    /// Holders of the ring's control cell but the ring itself (tests): the
    /// stream's handle (it reads the applied loop) and each registered
    /// reader.
    #[cfg(test)]
    pub(crate) fn control_holders(&self) -> usize {
        Arc::strong_count(&self.control) - 1
    }

    /// Controls retired and not yet freed (tests).
    #[cfg(test)]
    pub(crate) fn retired_controls(&self) -> usize {
        self.control.retired_len()
    }

    /// Window shrinks and moves so far (the ring's generation).
    #[cfg(test)]
    pub(crate) fn moves(&self) -> u64 {
        self.core.generation()
    }
}

/// The audio thread's handle on a region's ring.
pub(crate) type SharedReader = Arc<Ring>;

/// The streamer's end of a region's ring: the writer, its source (a disk
/// decoder, or the file resident whole) and what it holds (`loops::Content`).
/// Streamer-thread-local.
pub(crate) struct RegionOut {
    ring: Arc<Ring>,
    /// The ring's one writer.
    writer: PosWriter,
    content: Content,
    /// Incremental disk decoder for real streaming. `None` means this region
    /// reads the file resident whole (`resident`).
    #[cfg(codec)]
    decoder: Option<FileIn>,
    /// Whether the decoder's current fault has been logged, so a stream that
    /// keeps reading past it logs once.
    #[cfg(codec)]
    fault_logged: bool,
    /// The file decoded whole, for a format that cannot seek. Held here, not
    /// only in the LRU cache, so a file too large to cache still streams.
    resident: Option<Arc<Wave>>,
    /// Test seam: runs where the stream reads its file, and may block or
    /// panic (a decode that panics, B5).
    #[cfg(test)]
    read_hook: Option<super::decode::ReadHook>,
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
    #[cfg(codec)]
    pub(crate) fn set_decoder(&mut self, decoder: FileIn) {
        self.decoder = Some(decoder);
    }

    /// Run `hook` where this region reads its file (tests).
    #[cfg(test)]
    pub(crate) fn set_read_hook(&mut self, hook: super::decode::ReadHook) {
        self.read_hook = Some(hook);
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
        let (from, to) = self.window();
        let play = self.play().max(from);
        Samples(to.saturating_sub(play) as usize)
    }

    /// Slots, in frames.
    pub(crate) fn frames(&self) -> usize {
        self.writer.frames()
    }

    /// The positions held.
    pub(crate) fn window(&self) -> (u64, u64) {
        let w = self.writer.window();
        (w.from, w.to)
    }

    /// Where the reader plays (or this writer said it would).
    pub(crate) fn play(&self) -> u64 {
        self.writer.play()
    }

    /// One past the last position the block in flight may read.
    pub(crate) fn in_flight_end(&self) -> u64 {
        self.writer.in_flight_end()
    }

    /// Drop every position at and past `x` (a rewrite from `x` follows).
    pub(crate) fn retract_to(&mut self, x: u64) {
        self.writer.retract_to(x);
    }

    /// Drop every position below `y` (so a reader that jumps back there
    /// moves the window rather than read what lies below).
    pub(crate) fn raise_from(&mut self, y: u64) {
        self.writer.raise_from(y);
    }

    /// An empty window at `start` (the reader jumped outside it).
    pub(crate) fn reset(&mut self, start: u64) {
        self.writer.reset(start);
    }

    /// Where the reader plays, before one has said (a stream's start), so the window is filled there.
    pub(crate) fn set_play(&mut self, play: u64) {
        self.writer.set_play(play);
    }

    /// The file this region streams.
    pub(crate) fn file_path(&self) -> &Path {
        &self.ring.file_path
    }

    pub(crate) fn region_id(&self) -> StreamId {
        self.ring.region_id
    }

    /// Read file frames `at..` into `out` (flat, the ring's width), from the
    /// decoder or the resident file; zero past the end. `false` when the
    /// file could not be read there: a decode fault is logged once and the
    /// frames read before it are kept.
    pub(crate) fn read_file(&mut self, at: usize, out: &mut [f32]) -> bool {
        #[cfg(test)]
        if let Some(hook) = &self.read_hook {
            hook(self.ring.region_id, at);
        }
        let channels = self.ring.channels;
        #[cfg(codec)]
        if let Some(decoder) = self.decoder.as_mut() {
            let ch = self.writer.stride();
            if decoder.cursor() != Samples(at) && decoder.seek(Samples(at)).is_err() {
                out.fill(0.0);
                return false;
            }
            // Frames read before a fault are kept; the rest is silence.
            let got = decoder
                .fill_sequential_interleaved(out)
                .map_or(0, Samples::get);
            out[got * ch..].fill(0.0);
            let Some(fault) = decoder.error() else {
                self.fault_logged = false;
                return true;
            };
            if !self.fault_logged {
                self.fault_logged = true;
                tracing::warn!(
                    "decode fault at frame {at} of {}: {fault}; the stream plays silence from there",
                    self.ring.file_path.display()
                );
            }
            return false;
        }
        match self.resident.as_deref() {
            Some(wave) => {
                super::io::wave_io::read_wave(wave, at, out, channels);
                true
            }
            None => {
                out.fill(0.0);
                false
            }
        }
    }

    /// Fill `out` with what straight positions `pos..` hold under `mapping`,
    /// reading the file. `false` when any read of the file failed (a decode
    /// fault: the frames before it are kept, the rest is silence).
    pub(crate) fn fill_with(&mut self, mapping: &Mapping, pos: u64, out: &mut [f32]) -> bool {
        let ch = self.writer.stride();
        let mut read_ok = true;
        mapping.fill(pos, out, ch, &mut |at, run| {
            read_ok &= self.read_file(at, run);
        });
        read_ok
    }

    /// Append frames at the window's end, returning how many frames
    /// landed: short where a slot the reader's block in flight may read would
    /// be written, or where the ring is full ahead of the reader
    /// (`PosRing::push`). A trailing partial frame is ignored.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Samples {
        Samples(self.writer.push(samples))
    }
}

/// Constructor namespace for a region's ring.
pub(crate) struct RegionBuffer;

impl RegionBuffer {
    /// Build a region's ring of `capacity` frames of `channels` each
    /// (floored at 4096 frames and one channel, capped at
    /// [`MAX_POS_RING_FRAMES`]), holding nothing, read as an unlooped forward
    /// stream of a file `len` frames long (`usize::MAX` for a length not
    /// known). Returns the writer, the ring and the ring's one position reader.
    #[cfg(test)]
    pub(crate) fn with_capacity(
        region_id: StreamId,
        file_path: PathBuf,
        capacity: usize,
        channels: impl Into<ChannelLayout>,
    ) -> (RegionOut, SharedReader, PosReader) {
        Self::with_fade(region_id, file_path, capacity, channels, 0)
    }

    /// [`with_capacity`](Self::with_capacity), whose readers crossfade over
    /// `fade_frames` output frames at a jump.
    #[cfg(test)]
    pub(crate) fn with_fade(
        region_id: StreamId,
        file_path: PathBuf,
        capacity: usize,
        channels: impl Into<ChannelLayout>,
        fade_frames: usize,
    ) -> (RegionOut, SharedReader, PosReader) {
        let layout = nonempty(channels.into());
        let topology =
            ChannelTopology::assumed(layout).unwrap_or_else(|| ChannelTopology::discrete(layout));
        Self::for_file(
            region_id,
            file_path,
            capacity,
            topology,
            usize::MAX,
            fade_frames,
        )
    }

    /// `RegionBuffer::with_capacity` for a file `len` frames long, whose
    /// readers crossfade over `fade_frames` output frames at a jump.
    pub(crate) fn for_file(
        region_id: StreamId,
        file_path: PathBuf,
        capacity: usize,
        topology: ChannelTopology,
        len: usize,
        fade_frames: usize,
    ) -> (RegionOut, SharedReader, PosReader) {
        let channels = nonempty(topology.layout());
        let stride = channels.count() as usize;
        let frames = capacity.clamp(4096, MAX_POS_RING_FRAMES);
        let mapping = Mapping::plain(len);
        let (writer, reader) = PosRing::new(frames, stride, HISTORY_FRAMES);
        let ring = Arc::new(Ring {
            region_id,
            file_path,
            channels,
            #[cfg(test)]
            topology,
            core: writer.watch(),
            fade_frames,
            control: Arc::new(ArcLend::new(RingControl::new(
                RingMap::plain(Arrangement::plain(len), 0),
                LoopSetting::Off,
            ))),
        });
        let writer = RegionOut {
            ring: Arc::clone(&ring),
            writer,
            content: Content::new(mapping),
            #[cfg(codec)]
            decoder: None,
            #[cfg(codec)]
            fault_logged: false,
            resident: None,
            #[cfg(test)]
            read_hook: None,
        };
        (writer, ring, reader)
    }

    /// Point a fresh ring at `start`: the reader there and the window from
    /// just behind it (the taps a position reads one frame back).
    pub(crate) fn place(writer: &mut RegionOut, start: u64) {
        writer.reset(start.saturating_sub(HISTORY_FRAMES));
        writer.set_play(start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `frames` frames at `channels`, sample `c` of frame `f` carrying
    /// `f * channels + c`, so a rotation or a tear is a wrong value.
    fn indexed(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| i as f32).collect()
    }

    fn ring(capacity: usize, channels: usize) -> (RegionOut, SharedReader, PosReader) {
        RegionBuffer::with_capacity(
            StreamId(1),
            PathBuf::new(),
            capacity,
            ChannelLayout::try_from(channels).expect("a test width is small"),
        )
    }

    /// Frames pushed land by position, every channel in its place at six
    /// channels, a trailing partial frame dropped. (The ring's own protocol is
    /// `PosRing`'s tests and loom model.)
    ///
    /// Mutation (run): `push_interleaved` discarding its push → fails.
    #[test]
    fn pushed_frames_land_by_position() {
        let (mut writer, ring, mut reader) = ring(64, 6);
        assert_eq!(writer.channels(), ChannelLayout::from(6u16));
        let mut data = indexed(12, 6);
        data.extend_from_slice(&[99.0, 99.0, 99.0]);
        assert_eq!(writer.push_interleaved(&data), Samples(12));
        assert_eq!(ring.window(), (0, 12), "twelve whole frames");
        let claim = reader.claim(0, [(0, 12), (0, 0)]);
        for s in 0..12u64 {
            for c in 0..6 {
                assert_eq!(claim.sample(s, c), (s as usize * 6 + c) as f32, "frame {s}");
            }
        }
    }

    /// A ring holds a whole number of frames: the capacity asked for,
    /// floored at 4096 and capped at what the window word can say.
    #[test]
    fn a_ring_holds_a_whole_number_of_frames() {
        assert_eq!(ring(10, 2).1.frames(), 4_096);
        assert_eq!(ring(10_000, 6).1.frames(), 10_000);
        assert_eq!(ring(usize::MAX >> 8, 1).1.frames(), MAX_POS_RING_FRAMES);
    }

    /// `read_file` reports a damaged file. The repo's FLAC fixture cut to
    /// its first 90% ends short of its frame count; `read_file` keeps the
    /// frames decoded before the cut, zero-fills the rest, returns `false` and
    /// sets the `fault_logged` flag (the log line itself is not captured). A
    /// read at frame 0 afterwards clears the flag. Called directly, not
    /// through a streamer step or a voice.
    ///
    /// Mutation (run): discarding `got` on a fault (zero-filling all of
    /// `out`, as the old `Err` arm did) fails the kept-frames assertion.
    #[test]
    fn a_damaged_file_keeps_its_frames_and_reports_the_fault() {
        let source = tutti_fixtures::asset("audio/stereo.flac");
        let bytes = std::fs::read(source).expect("fixture");
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("cut.flac");
        std::fs::write(&path, &bytes[..bytes.len() * 9 / 10]).expect("write");

        let decoder = FileIn::open(&path).expect("a cut file still opens");
        let total = decoder.total_frames().expect("flac reports a length").get();
        let (mut writer, _ring, _reader) = ring(4096, 2);
        writer.set_decoder(decoder);

        let mut out = vec![1.0f32; total * 2];
        assert!(!writer.read_file(0, &mut out), "the cut is a fault");
        assert!(writer.fault_logged);
        let kept = out
            .chunks(2)
            .rposition(|f| f != [0.0, 0.0])
            .map_or(0, |i| i + 1);
        assert!(kept > 1000 && kept < total, "kept {kept} of {total} frames");
        assert!(
            out[kept * 2..].iter().all(|&s| s == 0.0),
            "silence after the fault"
        );

        // A seek back to good frames clears the fault, and reading resumes.
        let mut head = vec![0.0f32; 512 * 2];
        assert!(writer.read_file(0, &mut head));
        assert!(!writer.fault_logged);
    }
}
