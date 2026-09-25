//! A disk voice's file, read on demand for an offline render.
//!
//! A [`DiskVoice`](super::disk_voice::DiskVoice) plays what the butler streams
//! into its ring. A **fork** of one for an export cannot: the ring has one
//! consumer, the live audio thread, and a seek asked of the butler moves the
//! live stream. So a forked voice reads the file itself, through this: the
//! file the butler's record names ([`StreamFile`]), decoded a page at a time on
//! the render's own thread as the render reaches it.
//!
//! # Why a synchronous reader, not a second butler stream
//!
//! Offline there is no deadline to meet, so there is nothing to prefetch for.
//! A second butler stream would put the render back on the butler's schedule
//! (a render faster than real time outruns its refills, and an underrun is
//! silence written into the file as a success), and would need tearing down
//! when the export ends. Reading on demand makes the export a function of the
//! file and the timeline alone, and its only resources — a decoder and two
//! pages — drop with the fork.
//!
//! # The same read as the memory tier
//!
//! A position is read exactly as `MemorySource` reads one: the four taps
//! [`tap_indices`] names, through [`interpolate_taps`] (the kernel and the
//! channel policy `read_frame` uses), silent at and past the end, mirrored the
//! same way in reverse. A clip exported from disk is therefore the samples
//! the same clip in memory renders, bit for bit, wherever the two are asked
//! for the same position.

use std::sync::Arc;

use tutti_core::SamplePosition;

use super::interp::{interpolate_taps, tap_indices};
use super::memory_source::wrap_into_loop;
use super::types::Direction;
use super::LoopSetting;
use crate::butler::control::StreamFile;
use crate::MAX_SAMPLER_CHANNELS;

/// Frames per page. Two pages are resident; see [`Pages`].
const PAGE_FRAMES: usize = 1 << 15;

/// How far before the frame that missed a page starts, so the tap one frame
/// behind it (and a little more) is in the same page.
const PAGE_LEAD: usize = 4;

/// The file a forked disk voice plays: the butler's description of it, and
/// the file itself, opened on the first read.
pub(crate) struct OfflineRead {
    /// What to play. Shared by clones: it is a description, never written.
    file: Arc<StreamFile>,
    /// The open file. `None` until the first read, and never cloned: a clone
    /// opens its own, so no two renders share a decoder's cursor.
    pages: Option<Pages>,
    /// Opening failed once. Kept so a missing file costs one attempt and one
    /// warning, not one per frame.
    failed: bool,
}

impl Clone for OfflineRead {
    fn clone(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            pages: None,
            failed: false,
        }
    }
}

impl std::fmt::Debug for OfflineRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRead")
            .field("file", &self.file)
            .field("open", &self.pages.is_some())
            .field("failed", &self.failed)
            .finish()
    }
}

impl OfflineRead {
    /// A reader for `file`. Opens nothing yet.
    pub(crate) fn new(file: StreamFile) -> Self {
        Self {
            file: Arc::new(file),
            pages: None,
            failed: false,
        }
    }

    /// Read file position `pos` (fractional file frames), gain-free, into
    /// `out`, writing every element.
    ///
    /// Forward, a position at or past the stream's loop end wraps into the
    /// loop, and the last `crossfade_frames` before the end blend linearly
    /// into the frames from the loop start, as the butler's loop crossfade
    /// blends them. Reverse mirrors the position about the file's last frame,
    /// as the memory tier does (`read_clip_sample_into`), and ignores the
    /// loop, as the butler's reverse refill does. At or past the end:
    /// silence.
    ///
    /// Blocks on file I/O when the position leaves the resident pages. That
    /// is the point of this type, and why it is only ever built for an
    /// offline render.
    pub(crate) fn read_into(&mut self, pos: SamplePosition, direction: Direction, out: &mut [f32]) {
        let loop_ = self.file.loop_;
        let Some(pages) = self.pages() else {
            out.fill(0.0);
            return;
        };
        let len = pages.len as f64;
        let mut p = pos.get();
        if direction.is_reverse() {
            p = (len - 1.0 - p).max(0.0);
        } else if let LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } = loop_
        {
            let (start, end) = (start.get(), end.get());
            if end > start {
                if p >= end {
                    p = wrap_into_loop(p, start, end);
                }
                let fade = (crossfade_frames as f64).min(end - start);
                let fade_start = end - fade;
                if fade > 0.0 && p >= fade_start {
                    let into = p - fade_start;
                    let t = (into / fade) as f32;
                    pages.read_into(p, out);
                    let mut head = [0.0f32; MAX_SAMPLER_CHANNELS];
                    let head = &mut head[..out.len().min(MAX_SAMPLER_CHANNELS)];
                    pages.read_into(start + into, head);
                    // One envelope for every channel, as both crossfades in
                    // this crate use (`StreamingCrossfader`, `LoopCrossfade`).
                    for (s, &h) in out.iter_mut().zip(head.iter()) {
                        *s = *s * (1.0 - t) + h * t;
                    }
                    return;
                }
            }
        }
        pages.read_into(p, out);
    }

    /// The open file, opening it on first use. `None` when it cannot be opened
    /// (moved, deleted, no codec for it): the voice renders silence, and says
    /// so once.
    fn pages(&mut self) -> Option<&mut Pages> {
        if self.pages.is_none() && !self.failed {
            match Pages::open(&self.file) {
                Ok(pages) => self.pages = Some(pages),
                Err(why) => {
                    self.failed = true;
                    tracing::warn!(
                        "an offline disk voice cannot read {}: {why}; it renders silence",
                        self.file.path.display()
                    );
                }
            }
        }
        self.pages.as_mut()
    }
}

/// A file, decoded into two resident pages of [`PAGE_FRAMES`] as it is read.
///
/// Two, not one, because two places are read near each other in time: the
/// loop seam (the taps before a loop's end and after its start) and a loop
/// crossfade (the tail and the head at once). With one page each would evict
/// the other every frame.
struct Pages {
    source: Source,
    /// The file's width. Every page is interleaved at it.
    channels: usize,
    /// The file's length in frames.
    len: usize,
    slots: [Page; 2],
    /// The slot read last; a miss replaces the other one.
    recent: usize,
}

/// Where a page is decoded from.
enum Source {
    /// An incremental decoder, sought to each page. Seekable files.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    Stream(tutti_io::FileIn),
    /// The whole file, decoded into the first page when opened, for a format
    /// that cannot seek: the butler's own fallback for it (`load_wave`).
    Resident,
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
    /// Open `file`. Header only for a seekable file; a whole decode for one
    /// that is not.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    fn open(file: &StreamFile) -> Result<Self, String> {
        let decoder = tutti_io::FileIn::open(&file.path).map_err(|e| e.to_string())?;
        let channels = decoder.channels();
        if channels == 0 {
            return Err("the file has no channels".into());
        }
        match decoder.total_frames().filter(|_| decoder.seekable()) {
            Some(len) => Ok(Self {
                source: Source::Stream(decoder),
                channels,
                len: len as usize,
                slots: Default::default(),
                recent: 0,
            }),
            None => {
                drop(decoder);
                let wave = tutti_io::Wave::load(&file.path).map_err(|e| e.to_string())?;
                let (len, channels) = (wave.len(), wave.channels());
                let mut data = Vec::with_capacity(len * channels);
                for i in 0..len {
                    for c in 0..channels {
                        data.push(wave.at(c, i));
                    }
                }
                Ok(Self {
                    source: Source::Resident,
                    channels,
                    len,
                    slots: [
                        Page {
                            start: 0,
                            frames: len,
                            data,
                        },
                        Page::default(),
                    ],
                    recent: 0,
                })
            }
        }
    }

    /// No codec compiled in: nothing can be decoded, here or by the butler.
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    fn open(_file: &StreamFile) -> Result<Self, String> {
        Err("no audio codec feature is enabled".into())
    }

    /// Interpolate position `p` into `out` as `read_frame` would from the
    /// whole file resident: silence at and past the end.
    fn read_into(&mut self, p: f64, out: &mut [f32]) {
        if self.len == 0 || p >= self.len as f64 {
            out.fill(0.0);
            return;
        }
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

    /// Frame `at` (`< len`), interleaved at the file's width, paging it in if
    /// neither resident page holds it.
    fn frame(&mut self, at: usize) -> &[f32] {
        let slot = match self.slots.iter().position(|page| page.holds(at)) {
            Some(slot) => slot,
            None => {
                let slot = 1 - self.recent;
                self.load(slot, at);
                slot
            }
        };
        self.recent = slot;
        let ch = self.channels;
        let page = &self.slots[slot];
        let from = (at - page.start) * ch;
        &page.data[from..from + ch]
    }

    /// Decode the page around `at` into `slot`. A decode that comes up short
    /// (a truncated file) leaves the rest of the page silent, as the butler's
    /// refill does.
    fn load(&mut self, slot: usize, at: usize) {
        let ch = self.channels;
        let start = at.saturating_sub(PAGE_LEAD);
        let frames = PAGE_FRAMES.min(self.len - start);
        let page = &mut self.slots[slot];
        page.start = start;
        page.frames = frames;
        page.data.clear();
        page.data.resize(frames * ch, 0.0);
        match &mut self.source {
            #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
            Source::Stream(decoder) => {
                if decoder.cursor() != start as u64 {
                    if let Err(e) = decoder.seek(start as u64) {
                        tracing::warn!("an offline disk voice could not seek to {start}: {e}");
                        return;
                    }
                }
                if let Err(e) = decoder.fill_sequential_interleaved(&mut page.data) {
                    tracing::warn!("an offline disk voice could not decode at {start}: {e}");
                }
            }
            // Every frame is in slot 0 from the moment it opened, so no read
            // misses; an empty page here is unreachable, and silent if not.
            Source::Resident => {}
        }
    }
}
