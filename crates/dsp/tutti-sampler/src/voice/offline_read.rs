//! A disk voice's file, read on demand for an offline render.
//!
//! A [`DiskVoice`](super::disk_voice::DiskVoice) plays what the butler streams
//! into its ring. A **fork** of one for an export cannot: the ring has one
//! consumer, the live audio thread, and a seek asked of the butler moves the
//! live stream. So a forked voice reads the file itself, through this: the
//! file the butler's record names ([`StreamFile`]), re-opened by its path and
//! decoded a page at a time on the render's own thread as the render reaches
//! it — or read in place, when the butler's cache already holds it decoded.
//!
//! # Why a synchronous reader, not a second butler stream
//!
//! Offline there is no deadline to meet, so there is nothing to prefetch for.
//! A second butler stream would put the render back on the butler's schedule
//! (a render faster than real time outruns its refills, and an underrun is
//! silence written into the file as a success), and would need tearing down
//! when the export ends. Reading on demand makes the export a function of the
//! file and the timeline alone, and its only resources — a decoder and two
//! pages — drop with the fork, or earlier, once the voice is past its file
//! ([`OfflineRead::close`]).
//!
//! # A failure is the render's, not silence
//!
//! A file that cannot be opened, sought or decoded renders silence from that
//! point, and the first such failure is latched ([`FaultLatch`]): the fork
//! hands the latch to the graph (`AudioUnit::render_fault`), and the export
//! fails naming the node and the path rather than writing the silence as a
//! success.
//!
//! # The same read as the memory tier
//!
//! A position is read exactly as `MemorySource` reads one: the four taps
//! [`tap_indices`] names (a loop's, `LoopSpan::taps`, on a loop), through
//! [`interpolate_taps`] (the kernel and the channel policy `read_frame` uses),
//! silent at and past the end, mirrored the same way in reverse. A clip
//! exported from disk is therefore the samples the same clip in memory
//! renders, bit for bit, wherever the two are asked for the same position.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_core::{FaultLatch, SamplePosition};
use tutti_io::Wave;

use super::interp::{interpolate_taps, read_frame, read_looped_frame, tap_indices};
use super::loop_span::{blend, LoopSpan};
use super::types::Direction;
use super::LoopSetting;
use crate::butler::control::StreamFile;
use crate::MAX_SAMPLER_CHANNELS;

/// Frames per page. Two pages are resident; see [`Pages`].
const PAGE_FRAMES: usize = 1 << 15;

/// How far a page reaches past the frame that missed it, on the side the read
/// came from, so the taps around that frame (one behind, two ahead) are in the
/// same page.
const PAGE_LEAD: usize = 4;

/// Why an offline disk voice renders silence where its file should be: the
/// file, and what went wrong with it.
#[derive(Debug)]
pub(crate) struct OfflineReadError {
    path: PathBuf,
    what: String,
}

impl std::fmt::Display for OfflineReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot read {}: {}", self.path.display(), self.what)
    }
}

impl std::error::Error for OfflineReadError {}

/// The file a forked disk voice plays: the butler's description of it, and
/// the file itself, opened on the first read.
pub(crate) struct OfflineRead {
    /// What to play. Shared by clones: it is a description, never written.
    file: Arc<StreamFile>,
    /// The open file. `None` until the first read and after a
    /// [`close`](Self::close); never cloned: a clone opens its own, so no two
    /// renders share a decoder's cursor.
    open: Option<Open>,
    /// The file's length in frames, once it has been opened: what lets a
    /// closed reader know a position past the end without opening again.
    len: Option<usize>,
    /// Where a failure goes. Shared with the fork's probe.
    fault: Arc<FaultLatch>,
    /// Opening failed once. Kept so a missing file costs one attempt, not one
    /// per frame.
    failed: bool,
}

impl Clone for OfflineRead {
    fn clone(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            open: None,
            len: self.len,
            fault: Arc::clone(&self.fault),
            failed: false,
        }
    }
}

impl std::fmt::Debug for OfflineRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRead")
            .field("file", &self.file)
            .field("open", &self.open.is_some())
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl OfflineRead {
    /// A reader for `file`, latching its failures into `fault`. Opens
    /// nothing yet.
    pub(crate) fn new(file: StreamFile, fault: Arc<FaultLatch>) -> Self {
        Self {
            file: Arc::new(file),
            open: None,
            len: None,
            fault,
            failed: false,
        }
    }

    /// This reader, closed, latching into `fault` from now on: what a copy
    /// isolated again keeps, with the new copy's latch.
    pub(crate) fn relatched(mut self, fault: Arc<FaultLatch>) -> Self {
        self.open = None;
        self.failed = false;
        self.fault = fault;
        self
    }

    /// Read file position `pos` (fractional file frames), gain-free, into
    /// `out`, writing every element.
    ///
    /// Forward, on a loop, the position is placed on it and read as
    /// `LoopSpan` has a loop (the fade leads into the loop's start; taps near
    /// the end wrap through it), as `MemorySource::read_placed_into` reads the
    /// same loop. Reverse mirrors the position about the file's last frame, as
    /// the memory tier does, and ignores the loop, as the butler's reverse
    /// refill does. Silence where the file has nothing to play — forward at or
    /// past the end, reverse at a position whose mirror is before the first
    /// frame (`pos >= len`, as forward) — and the file is closed there (there
    /// is nothing more to read in that direction).
    ///
    /// Blocks on file I/O when the position leaves the resident pages. That
    /// is the point of this type, and why it is only ever built for an
    /// offline render.
    pub(crate) fn read_into(&mut self, pos: SamplePosition, direction: Direction, out: &mut [f32]) {
        // Past what this direction can play, known without opening the file.
        let played_out = match direction {
            Direction::Reverse => true,
            Direction::Forward => self.file.loop_ == LoopSetting::Off,
        };
        if played_out {
            if let Some(len) = self.len {
                if pos.get() >= len as f64 {
                    self.close();
                    out.fill(0.0);
                    return;
                }
            }
        }
        let setting = self.file.loop_;
        let Some(open) = self.open() else {
            out.fill(0.0);
            return;
        };
        let span = LoopSpan::from_setting(setting, open.len());
        let len = open.len() as f64;
        match (direction, span) {
            (Direction::Reverse, _) => {
                if pos.get() >= len {
                    out.fill(0.0);
                    return;
                }
                open.read_into((len - 1.0 - pos.get()).max(0.0), out);
            }
            (Direction::Forward, Some(span)) => {
                let (p, looped) = span.place(pos.get());
                open.read_looped_into(&span, p, looped, out);
            }
            (Direction::Forward, None) => open.read_into(pos.get(), out),
        }
    }

    /// Close the file: drop the decoder and its pages. The next read opens it
    /// again by its path. A voice closes it once the playhead has left its
    /// window for good, so a render holding many voices keeps one file open
    /// per voice sounding, not per voice in the graph.
    pub(crate) fn close(&mut self) {
        self.open = None;
    }

    /// Whether the file is open.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Pages decoded since the file was opened.
    #[cfg(test)]
    pub(crate) fn page_loads(&self) -> usize {
        match &self.open {
            Some(Open::Paged(pages)) => pages.loads,
            _ => 0,
        }
    }

    /// The open file, opening it on first use. `None` when it cannot be opened
    /// (moved, deleted, no codec for it): the voice renders silence, and the
    /// failure is latched.
    fn open(&mut self) -> Option<&mut Open> {
        if self.open.is_none() && !self.failed {
            match Open::from_file(&self.file, Arc::clone(&self.fault)) {
                Ok(open) => {
                    self.len = Some(open.len());
                    self.open = Some(open);
                }
                Err(what) => {
                    self.failed = true;
                    self.fault.latch(OfflineReadError {
                        path: self.file.path.clone(),
                        what,
                    });
                }
            }
        }
        self.open.as_mut()
    }
}

/// An opened file: decoded whole already (the butler's cached wave, or a
/// file that cannot seek), or paged in through a decoder.
enum Open {
    Resident(Arc<Wave>),
    /// Boxed: a decoder and two pages' bookkeeping, against an `Arc`. With
    /// no codec compiled in nothing builds one (`open_path` fails first).
    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")),
        allow(dead_code)
    )]
    Paged(Box<Pages>),
}

impl Open {
    /// Open `file`: the butler's decoded copy when its cache holds one, else
    /// the file by its path — header only when it can seek, a whole decode
    /// when it cannot.
    fn from_file(file: &StreamFile, fault: Arc<FaultLatch>) -> Result<Self, String> {
        if let Some(wave) = &file.resident {
            return Ok(Self::Resident(Arc::clone(wave)));
        }
        Self::open_path(file, fault)
    }

    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    fn open_path(file: &StreamFile, fault: Arc<FaultLatch>) -> Result<Self, String> {
        let decoder = tutti_io::FileIn::open(&file.path).map_err(|e| e.to_string())?;
        let channels = decoder.channels();
        if channels == 0 {
            return Err("the file has no channels".into());
        }
        match decoder.total_frames().filter(|_| decoder.seekable()) {
            Some(len) => Ok(Self::Paged(Box::new(Pages {
                decoder,
                path: file.path.clone(),
                fault,
                channels,
                len: len as usize,
                slots: Default::default(),
                recent: 0,
                loads: 0,
            }))),
            // Decoded once, and read in place as a memory voice reads its
            // wave: no second, interleaved copy.
            None => {
                drop(decoder);
                Wave::load(&file.path)
                    .map(|wave| Self::Resident(Arc::new(wave)))
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// No codec compiled in: nothing can be decoded, here or by the butler.
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    fn open_path(_file: &StreamFile, _fault: Arc<FaultLatch>) -> Result<Self, String> {
        Err("no audio codec feature is enabled".into())
    }

    fn len(&self) -> usize {
        match self {
            Self::Resident(wave) => wave.len(),
            Self::Paged(pages) => pages.len,
        }
    }

    /// Interpolate position `p` into `out` as `read_frame` would from the
    /// whole file resident: silence at and past the end.
    fn read_into(&mut self, p: f64, out: &mut [f32]) {
        if p >= self.len() as f64 {
            out.fill(0.0);
            return;
        }
        match self {
            // The memory tier's own read, on the same kind of wave.
            Self::Resident(wave) => read_frame(wave, p, out),
            Self::Paged(pages) => pages.read_into(p, out),
        }
    }

    /// Interpolate position `p`, placed on `span` (`looped` once round it),
    /// into `out` as `read_looped_frame` would from the whole file resident:
    /// silence at and past the end.
    fn read_looped_into(&mut self, span: &LoopSpan, p: f64, looped: bool, out: &mut [f32]) {
        if p >= self.len() as f64 {
            out.fill(0.0);
            return;
        }
        match self {
            // The memory tier's own looped read, on the same kind of wave.
            Self::Resident(wave) => read_looped_frame(wave, span, p, looped, out),
            Self::Paged(pages) => pages.read_looped_into(span, p, looped, out),
        }
    }
}

/// A file, decoded into two resident pages of [`PAGE_FRAMES`] as it is read.
///
/// Two, not one, because two places are read near each other in time: the
/// loop seam (the taps before a loop's end and after its start) and a loop
/// crossfade (the tail and the head at once). With one page each would evict
/// the other every frame.
///
/// A page is laid out ahead of the read: after the frame that missed going
/// forward, before it going backward ([`load`](Self::load)), so a reversed
/// read decodes a page per page of frames, not one per few frames.
// With no codec compiled in nothing builds one (`open_path` fails first).
#[cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")),
    allow(dead_code)
)]
struct Pages {
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    decoder: tutti_io::FileIn,
    path: PathBuf,
    fault: Arc<FaultLatch>,
    /// The file's width. Every page is interleaved at it.
    channels: usize,
    /// The file's length in frames.
    len: usize,
    slots: [Page; 2],
    /// The slot read last; a miss replaces the other one.
    recent: usize,
    /// Pages decoded so far. What a test bounds the paging by.
    loads: usize,
}

/// A run of decoded frames, `data` interleaved at the file's width.
#[derive(Default)]
struct Page {
    start: usize,
    frames: usize,
    data: Vec<f32>,
}

impl Page {
    fn holds(&self, frame: usize) -> bool {
        frame >= self.start && frame < self.start + self.frames
    }
}

impl Pages {
    /// Interpolate position `p` (`< len`) into `out`.
    fn read_into(&mut self, p: f64, out: &mut [f32]) {
        // The file's width, narrowed to what a stack frame holds — the same
        // ceiling `DiskSource` narrows its ring to.
        let ch = self.channels.min(MAX_SAMPLER_CHANNELS);
        let (taps, frac) = tap_indices(self.len, p);
        let mut frames = [[0.0f32; MAX_SAMPLER_CHANNELS]; 4];
        for (frame, &at) in frames.iter_mut().zip(taps.iter()) {
            frame[..ch].copy_from_slice(&self.frame(at)[..ch]);
        }
        interpolate_taps(ch, frac, out, |c, t| frames[t][c]);
    }

    /// Interpolate position `p` (`< len`), placed on `span`, into `out`: the
    /// loop's four taps, each blended toward its lead-in inside the fade, as
    /// `read_looped_frame` blends them.
    fn read_looped_into(&mut self, span: &LoopSpan, p: f64, looped: bool, out: &mut [f32]) {
        let ch = self.channels.min(MAX_SAMPLER_CHANNELS);
        let (taps, frac) = span.taps(self.len, p, looped);
        let mut frames = [[0.0f32; MAX_SAMPLER_CHANNELS]; 4];
        for (frame, tap) in frames.iter_mut().zip(taps.iter()) {
            frame[..ch].copy_from_slice(&self.frame(tap.frame)[..ch]);
            if let Some((lead, t)) = tap.fade {
                let lead = self.frame(lead);
                for (s, &l) in frame[..ch].iter_mut().zip(lead.iter()) {
                    *s = blend(*s, l, t);
                }
            }
        }
        interpolate_taps(ch, frac, out, |c, t| frames[t][c]);
    }

    /// Frame `at` (`< len`), interleaved at the file's width, paging it in if
    /// neither resident page holds it.
    fn frame(&mut self, at: usize) -> &[f32] {
        let slot = match self.slots.iter().position(|page| page.holds(at)) {
            Some(slot) => slot,
            None => {
                // Behind the page read last: the read is going backward.
                let recent = &self.slots[self.recent];
                let backward = recent.frames > 0 && at < recent.start;
                let slot = 1 - self.recent;
                self.load(slot, at, backward);
                slot
            }
        };
        self.recent = slot;
        let ch = self.channels;
        let page = &self.slots[slot];
        let from = (at - page.start) * ch;
        &page.data[from..from + ch]
    }

    /// Decode the page around `at` into `slot`: `[at - LEAD, at - LEAD +
    /// PAGE)` going forward, `(at + LEAD - PAGE, at + LEAD]` going backward,
    /// clamped to the file. A seek or decode that fails leaves the page
    /// silent and latches the failure; a decode that comes up short (a
    /// truncated file) leaves the rest silent, as the butler's refill does.
    fn load(&mut self, slot: usize, at: usize, backward: bool) {
        let ch = self.channels;
        let start = if backward {
            (at + PAGE_LEAD + 1).saturating_sub(PAGE_FRAMES)
        } else {
            at.saturating_sub(PAGE_LEAD)
        };
        let frames = PAGE_FRAMES.min(self.len - start);
        self.loads += 1;
        let page = &mut self.slots[slot];
        page.start = start;
        page.frames = frames;
        page.data.clear();
        page.data.resize(frames * ch, 0.0);
        #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
        {
            let fail = |what: String| OfflineReadError {
                path: self.path.clone(),
                what,
            };
            if self.decoder.cursor() != start as u64 {
                if let Err(e) = self.decoder.seek(start as u64) {
                    self.fault
                        .latch(fail(format!("seek to frame {start}: {e}")));
                    return;
                }
            }
            if let Err(e) = self.decoder.fill_sequential_interleaved(&mut page.data) {
                self.fault
                    .latch(fail(format!("decode at frame {start}: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::SampleRate;

    const LEN: usize = 100_000;

    fn value(i: usize) -> f32 {
        (i as f32 + 1.0) * 1e-5
    }

    /// A mono f32 ramp of [`LEN`] frames, three pages long.
    fn ramp_file(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("ramp.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("writes");
        for i in 0..LEN {
            w.write_sample(value(i)).expect("writes");
        }
        w.finalize().expect("writes");
        path
    }

    fn reader(path: std::path::PathBuf) -> OfflineRead {
        OfflineRead::new(
            StreamFile {
                path,
                file_rate: SampleRate(48_000.0),
                loop_: LoopSetting::Off,
                resident: None,
            },
            Arc::default(),
        )
    }

    /// **A reversed read decodes a page per page of frames**, as a forward
    /// one does: reading the whole file backwards loads about `LEN / PAGE`
    /// pages, not one every few frames (a page laid out ahead of a forward
    /// read leaves a backward one below its start almost at once). Counted,
    /// not timed. Both directions read the right frames.
    ///
    /// Mutation (run): `Pages::frame` never treating a miss as backward →
    /// ~25 000 loads reversed → fails.
    #[test]
    fn a_reversed_read_decodes_a_page_per_page_of_frames() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = ramp_file(dir.path());
        let bound = LEN / (PAGE_FRAMES - 2 * PAGE_LEAD) + 2;
        for direction in [Direction::Forward, Direction::Reverse] {
            let mut read = reader(path.clone());
            let mut out = [0.0f32; 1];
            for k in 0..LEN {
                read.read_into(SamplePosition(k as f64), direction, &mut out);
                let want = match direction {
                    Direction::Forward => value(k),
                    Direction::Reverse => value(LEN - 1 - k),
                };
                assert_eq!(out[0], want, "{direction:?}: frame {k}");
            }
            assert!(
                read.page_loads() <= bound,
                "{direction:?}: {} page loads for {LEN} frames (at most {bound})",
                read.page_loads()
            );
        }
    }

    /// **The taps wrap through the loop** (doc 013's N2, the disk fork): on a
    /// hard loop `[10, 20)`, half a frame before the end interpolates frames
    /// 18, 19, then 10, 11 — what the loop plays next, and what the butler's
    /// ring holds there — not 20, 21 from past it; half a frame into a later
    /// pass the frame behind is 19; the first pass reaches 10 from 9. Paged,
    /// through the loop's own tap layout, as the memory tier reads it.
    ///
    /// Mutation (run): `LoopSpan::taps` clamping to the file rather than
    /// wrapping → `value` 20, 21 in the taps at 19.5 → fails.
    #[test]
    fn a_loops_taps_wrap_through_its_seam() {
        use super::super::interp::cubic_hermite;
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut read = OfflineRead::new(
            StreamFile {
                path: ramp_file(dir.path()),
                file_rate: SampleRate(48_000.0),
                loop_: LoopSetting::On {
                    start: SamplePosition(10.0),
                    end: SamplePosition(20.0),
                    crossfade_frames: 0,
                },
                resident: None,
            },
            Arc::default(),
        );
        let mut out = [0.0f32; 1];
        let mut at = |pos: f64| {
            read.read_into(SamplePosition(pos), Direction::Forward, &mut out);
            out[0]
        };
        let v = value;
        assert_eq!(at(19.5), cubic_hermite(v(18), v(19), v(10), v(11), 0.5));
        assert_eq!(at(30.5), cubic_hermite(v(19), v(10), v(11), v(12), 0.5));
        assert_eq!(at(10.5), cubic_hermite(v(9), v(10), v(11), v(12), 0.5));
        assert!(read.page_loads() > 0, "read through the pages");
    }

    /// **A forward read past the end of an unlooped file closes it**: there
    /// is nothing more to read, so the render stops holding the file, and a
    /// later read past the end does not open it again.
    ///
    /// Mutation (run): the close removed from `OfflineRead::read_into` → the
    /// file stays open → fails.
    #[test]
    fn a_read_past_the_end_closes_the_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut read = reader(ramp_file(dir.path()));
        let mut out = [0.0f32; 1];
        read.read_into(SamplePosition(10.0), Direction::Forward, &mut out);
        assert!(read.is_open());
        read.read_into(
            SamplePosition((LEN + 10) as f64),
            Direction::Forward,
            &mut out,
        );
        assert_eq!(out[0], 0.0);
        assert!(!read.is_open(), "the file is still open past its end");
        read.read_into(
            SamplePosition((LEN + 20) as f64),
            Direction::Forward,
            &mut out,
        );
        assert!(!read.is_open(), "a read past the end opened it again");
    }
}
