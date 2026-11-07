//! [`WavOut`] — the live WAV implementation of [`AudioOut`].
//!
//! An [`AudioOut`] is "push frames → destination"; this is that
//! destination for a WAV file. It writes any
//! [`BitDepth`], INCREMENTALLY — a recording is
//! minutes long and never held resident.
//!
//! # Why this is not tutti-export's encoder
//!
//! `tutti-export` is the OFFLINE edge and has its own hound-backed WAV writer.
//! The two stay separate for a structural reason: **this one is pushed, that one
//! pulls.** An `AudioOut` is fed blocks by whoever owns the loop; an export
//! `Encoder` *is* the loop (it takes the source, so flacenc can be handed it
//! directly) and is driven by a `RenderPlan` carrying a total frame count. A
//! live capture has no total — it ends when someone stops it — so it cannot be
//! expressed as an export encode.
//!
//! They do share the quantization: this sink and tutti-export's WAV encoder both
//! dispatch through [`BitDepth::quantize`](tutti_types::pcm::BitDepth::quantize),
//! so a recorded and an exported WAV agree sample-for-sample at a given depth by
//! construction. AIFF and FLAC quantize their own way over the same `pcm`
//! primitives, so they agree in fact — just not structurally.
//!
//! One difference is deliberate: export dithers before quantizing and a live
//! capture does not, because noise shaping is a decision a capture path should
//! not make silently.
//!
//! The live driver is [`Recorder`](crate::Recorder), which pumps any live source
//! ([`AudioIn`](tutti_types::AudioIn)) into this sink ([`AudioOut`])
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use tutti_types::AudioOut;
use tutti_types::SampleRate;
use tutti_types::pcm::{BitDepth, Sample};
use tutti_types::wav::{self, DataLimitExceeded};
use tutti_types::{ChannelLayout, ChannelTopology};

/// A WAV file sink: the [`AudioOut`] a recording writes into.
///
/// Writes incrementally through a buffered file writer, so a take of any
/// length is never held in memory, at any [`BitDepth`] (integer depths are
/// quantized without dither). Frames are written at the header's width with
/// [`write_interleaved`](Self::write_interleaved) (or [`AudioOut::write`]).
///
/// Writes do blocking file I/O: drive the sink from a pump thread such as
/// [`Recorder`](crate::Recorder)'s, never from the audio thread. A write
/// failure (a full disk, say) stops the sink and is reported from
/// [`finalize`](AudioOut::finalize), which must run for the file to be
/// readable: it back-patches the header's lengths.
///
/// # Limits
///
/// A WAV file's sizes are 32-bit, so a file holds at most
/// [`wav::max_data_frames`] frames (just under 4 GiB: about 62 minutes of
/// 8-channel 24-bit audio at 48 kHz). The write that would cross it stops the
/// sink like any other failure: [`finalize`](AudioOut::finalize) closes the
/// file at the last whole frame under the limit, which stays readable, and
/// returns a [`DataLimitExceeded`] inside its `io::Error`. RF64 is not written.
///
/// A header with more than two channels also declares which speakers they
/// are. Given only a width the sink uses the engine's SMPTE order
/// ([`wav::default_channel_mask`]): mono, stereo and 5.1 (FL FR C LFE SL SR)
/// are named, and every other width (4 may be quad or B-format; tutti's 7.1 is
/// not in WAVE's order) is written unassigned. A caller that knows its 4
/// channels are quad says so with [`with_topology`](Self::with_topology).
///
/// # Examples
///
/// ```
/// use tutti_io::{AudioOut, BitDepth, WavOut};
///
/// # fn main() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let mut wav = WavOut::create(dir.path().join("tone.wav"), 48_000.0, 2u16, BitDepth::Int24)?;
/// wav.write_interleaved(&[0.5, -0.5, 0.25, -0.25]); // two stereo frames
/// wav.finalize()?;
/// # Ok(())
/// # }
/// ```
pub struct WavOut {
    writer: WavWriter<BufWriter<File>>,
    layout: ChannelLayout,
    depth: BitDepth,
    /// The rate written into the header, kept so a caller can check it against
    /// the source it is about to pump in. Nothing here can validate that pairing
    /// — `AudioIn` deliberately carries no rate — so the best this type can do
    /// is report what it promised the file.
    sample_rate: SampleRate,
    /// The first write failure, kept until [`finalize`](AudioOut::finalize) can
    /// report it.
    ///
    /// [`AudioOut::write`] returns `()` — a block sink has nowhere to put an
    /// error mid-stream — so without this a disk-full or I/O fault silently ends
    /// the recording and hands back a short file that reports success. Keeping
    /// the *first* one is what makes the report meaningful: later failures are
    /// usually the same cause repeated.
    first_error: Option<std::io::Error>,
    /// The speakers the caller declared; `None` means the width's SMPTE order.
    topology: Option<ChannelTopology>,
    /// Where the file is, to patch the speaker mask into once `hound` has
    /// finalized it.
    path: PathBuf,
    /// Whole frames written so far.
    frames_written: u64,
    /// The frames the file may hold: [`wav::max_data_frames`], lowered by tests.
    max_frames: u64,
}

// Hand-rolled: `hound::WavWriter` isn't `Debug`. Print the channel count +
// on-disk format; the writer itself is opaque.
impl std::fmt::Debug for WavOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavOut")
            .field("layout", &self.layout)
            .field("sample_rate", &self.sample_rate)
            .field("depth", &self.depth)
            .field("failed", &self.first_error.is_some())
            .finish_non_exhaustive()
    }
}

impl WavOut {
    /// Creates (or truncates) the file at `file_path` and writes a WAV header
    /// for `channels` at `sample_rate` (rounded to whole Hz) and `depth`.
    ///
    /// # Errors
    ///
    /// The `io::Error` from creating the file (a missing directory, a
    /// permission denial, a full disk) or from writing the header.
    pub fn create(
        file_path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
        channels: impl Into<ChannelLayout>,
        depth: BitDepth,
    ) -> std::io::Result<Self> {
        let sample_rate = sample_rate.into();
        let sample_format = if depth.is_integer() {
            SampleFormat::Int
        } else {
            SampleFormat::Float
        };
        let layout: ChannelLayout = channels.into();
        let spec = WavSpec {
            channels: layout.count(),
            // The WAV header field is an integer rate: types stop here.
            sample_rate: sample_rate.get().round() as u32,
            bits_per_sample: depth.bits(),
            sample_format,
        };

        let path = file_path.as_ref().to_path_buf();
        let file = File::create(&path)?;
        let buf_writer = BufWriter::new(file);
        // Same `hound::Error` -> io error carry as `finalize`: the message is
        // the only part a caller can act on, and dropping it here would be the
        // information loss this signature exists to stop.
        let writer =
            WavWriter::new(buf_writer, spec).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            writer,
            layout,
            depth,
            sample_rate,
            first_error: None,
            topology: None,
            path,
            frames_written: 0,
            max_frames: wav::max_data_frames(layout, depth),
        })
    }

    /// Declares which speaker each channel feeds, for the file's speaker mask
    /// (see [`wav::channel_mask`]: `0`, unassigned, when WAVE cannot state the
    /// order). The samples are written as given, never reordered.
    ///
    /// # Errors
    ///
    /// `InvalidInput` if `topology`'s width is not this sink's.
    pub fn with_topology(mut self, topology: ChannelTopology) -> std::io::Result<Self> {
        if topology.layout() != self.layout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "a topology of {:?} cannot describe a sink of {:?}",
                    topology.layout(),
                    self.layout
                ),
            ));
        }
        self.topology = Some(topology);
        Ok(self)
    }

    /// The mask the finished file declares.
    fn mask(&self) -> u32 {
        self.topology
            .as_ref()
            .map_or_else(|| wav::default_channel_mask(self.layout), wav::channel_mask)
    }

    /// A sink whose next loud write is guaranteed to fail, for tests that need
    /// a *real* mid-stream write failure rather than a simulated one.
    ///
    /// The header declares 8-bit integer samples while the sink quantizes at
    /// [`BitDepth::Int16`]: creation succeeds, and `hound` then rejects the
    /// first `i16` too wide for the stream with `TooWide` on the **write**.
    /// That is the shape a disk-full has, and it is deterministic — no timing,
    /// no filesystem trickery, no permission games that behave differently
    /// under a CI runner or as root.
    ///
    /// Crate-internal because the fields it sets are module-private:
    /// `wav_out.rs`'s own `a_failed_write_surfaces_from_finalize` builds this
    /// literal inline, and `recorder.rs` needs the same fixture to reach the
    /// Drop-path finalize failure. Two copies of a struct literal over private
    /// fields is exactly the duplication that drifts, so it lives here once.
    #[cfg(test)]
    pub(crate) fn narrow_header_for_test(file_path: impl AsRef<Path>) -> Self {
        let spec = WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 8,
            sample_format: SampleFormat::Int,
        };
        let file = File::create(file_path.as_ref()).expect("create");
        Self {
            writer: WavWriter::new(BufWriter::new(file), spec).expect("header"),
            layout: ChannelLayout::STEREO,
            depth: BitDepth::Int16,
            sample_rate: SampleRate::SR_48K,
            first_error: None,
            topology: None,
            path: file_path.as_ref().to_path_buf(),
            frames_written: 0,
            max_frames: u64::MAX,
        }
    }

    #[cfg(test)]
    fn write_interleaved_and_finish_for_test(mut self) {
        self.write_interleaved(&vec![0.25; 5 * self.stride()]);
        self.finalize().unwrap();
    }

    /// Lowers the frame limit so a test reaches it without writing 4 GiB.
    #[cfg(test)]
    pub(crate) fn with_max_frames_for_test(mut self, max_frames: u64) -> Self {
        self.max_frames = max_frames;
        self
    }

    /// Returns the declared channel layout — what the WAV header says, and
    /// therefore how many samples per frame
    /// [`write_interleaved`](Self::write_interleaved) expects.
    ///
    /// The same as [`AudioOut::layout`], callable without importing the
    /// trait.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride as a plain `usize`, clamped to at least 1.
    ///
    /// Kept as a separate accessor rather than making every call site write
    /// `layout().count().max(1) as usize`, because that expression is the one
    /// piece of arithmetic that must be derived ONCE per call and hoisted above
    /// any per-frame loop.
    fn stride(&self) -> usize {
        self.layout.count().max(1) as usize
    }

    /// Returns the sample rate written into the header.
    ///
    /// Compare it with the source's before pumping: feeding 48 kHz frames into
    /// a sink that declared 8 kHz produces a valid WAV that plays back six
    /// times too slow, and nothing downstream can detect it.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Writes flat interleaved frames at this sink's own declared width.
    ///
    /// `samples` is a flat interleaved buffer holding `frames *
    /// layout().count()` samples. Emits exactly `layout().count()` samples per
    /// frame — no more, no fewer. A short trailing frame is ignored; a frame
    /// wider than the header is truncated to it.
    ///
    /// Emitting anything else corrupts the file: a width the header disagrees
    /// with makes every reader interleave-misalign, rotating channels each
    /// frame, and can leave `hound` unable to finalize on a non-integral frame
    /// count.
    ///
    /// A write failure stops the sink and is reported once, from
    /// [`finalize`](AudioOut::finalize). Swallowing it would leave a disk-full
    /// mid-take looking like a complete recording.
    pub fn write_interleaved(&mut self, samples: &[f32]) {
        if self.first_error.is_some() {
            return; // already failed; the file is being abandoned
        }
        // Derived ONCE, above the loop. See `stride`.
        let ch = self.stride();
        for frame in samples.chunks_exact(ch) {
            if !self.begin_frame() {
                return;
            }
            for &s in frame {
                if !self.emit(s) {
                    return;
                }
            }
        }
    }

    /// Admits one more whole frame, or latches [`DataLimitExceeded`] and
    /// returns `false` when the file holds [`max_frames`](Self::max_frames)
    /// already. Called once per frame, before its first sample, so the file
    /// always ends on a frame boundary under the limit.
    fn begin_frame(&mut self) -> bool {
        if self.first_error.is_some() {
            return false;
        }
        if self.frames_written >= self.max_frames {
            self.first_error = Some(std::io::Error::other(DataLimitExceeded {
                limit_frames: self.max_frames,
            }));
            return false;
        }
        self.frames_written += 1;
        true
    }

    /// Quantize and write one sample, returning `false` once the sink has
    /// failed. The single place a sample reaches `hound`, so every write path
    /// shares one quantization and one error-latching rule.
    ///
    /// The depth dispatch is `tutti-types`'; only the writer call is ours, so
    /// this sink cannot drift from the export encoders' quantization.
    fn emit(&mut self, s: f32) -> bool {
        if self.first_error.is_some() {
            return false;
        }
        let written = match self.depth.quantize(s) {
            Sample::I16(v) => self.writer.write_sample(v),
            Sample::I24(v) => self.writer.write_sample(v),
            Sample::F32(v) => self.writer.write_sample(v),
        };
        if let Err(e) = written {
            self.first_error = Some(std::io::Error::other(e));
            return false;
        }
        true
    }
}

impl AudioOut for WavOut {
    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Write flat interleaved samples **already at this sink's own width**.
    ///
    /// `frames` holds `frames * layout().count()` samples; the trait speaks a
    /// runtime [`ChannelLayout`] and the sink's layout is the header's, so this
    /// and [`write_interleaved`](WavOut::write_interleaved) are the same
    /// operation. A caller feeding some *other* width folds first, through
    /// [`tutti_types::fold_frame`].
    fn write(&mut self, frames: &[f32]) {
        self.write_interleaved(frames);
    }

    fn is_failed(&self) -> bool {
        self.first_error.is_some()
    }

    fn finalize(self) -> std::io::Result<()> {
        let mask = self.mask();
        let Self {
            writer,
            first_error,
            path,
            ..
        } = self;
        // A write that failed mid-stream is reported here, ahead of the
        // finalize itself: the header may well back-patch cleanly over a
        // truncated recording, so finalizing `Ok` would hide the real fault. The
        // caller learns the take is short, which is the whole point of keeping
        // it.
        if let Some(error) = first_error {
            // Still finalize, so the bytes that did land are readable.
            if writer.finalize().is_ok() {
                let _ = wav::patch_channel_mask_at(&path, mask);
            }
            return Err(error);
        }
        // `hound::Error` -> io error: finalize flushes the sample buffer and
        // back-patches the RIFF/data chunk sizes. On failure the file is left
        // with a stale header and is unreadable — a real integrity loss.
        writer
            .finalize()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // `hound` writes the lowest-N-bits speaker mask and has no way to
        // change it; the right one goes in over the finished header.
        wav::patch_channel_mask_at(&path, mask)
    }
}

#[cfg(test)]
mod tests;
