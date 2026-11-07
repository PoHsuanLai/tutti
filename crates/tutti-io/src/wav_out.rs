//! [`WavOut`] — the live WAV implementation of [`AudioOut`].
//!
//! An [`AudioOut`] is "push frames → destination"; this is that destination
//! for a WAV file. It writes any [`Pcm`] width INCREMENTALLY: a recording is
//! minutes long and never held resident.
//!
//! It is `tutti-codec`'s WAV [`Encoder`] behind the [`AudioOut`] contract, the
//! same writer an export bounces through, so a recorded and an exported WAV
//! agree sample-for-sample and byte-for-byte at a given width. One difference
//! is deliberate: an export dithers before quantizing and a live capture does
//! not, because noise shaping is a decision a capture path should not make
//! silently.
//!
//! The live driver is [`Recorder`](crate::Recorder), which pumps any live source
//! ([`AudioIn`](tutti_types::AudioIn)) into this sink ([`AudioOut`])
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use tutti_codec::{Encoder, Format, Pcm, Spec};
use tutti_types::AudioOut;
use tutti_types::SampleRate;
use tutti_types::{ChannelLayout, ChannelTopology, Interleaved};

/// A WAV file sink: the [`AudioOut`] a recording writes into.
///
/// Writes incrementally through a buffered file writer, so a take of any
/// length is never held in memory, at any [`Pcm`] width (integer widths are
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
/// [`Format::max_frames`] frames (just under 4 GiB: about 62 minutes of
/// 8-channel 24-bit audio at 48 kHz). The write that would cross it stops the
/// sink like any other failure: [`finalize`](AudioOut::finalize) closes the
/// file at the last whole frame under the limit, which stays readable, and
/// returns [`tutti_codec::Error::WavTooLarge`] inside its `io::Error`. RF64 is
/// not written.
///
/// A header with more than two channels also declares which speakers they
/// are. Given only a width the sink uses the engine's SMPTE order: mono,
/// stereo and 5.1 (FL FR C LFE SL SR) are named, and every other width (4 may
/// be quad or B-format; tutti's 7.1 is not in WAVE's order) is written
/// unassigned. A caller that knows its 4 channels are quad says so with
/// [`create_with_topology`](Self::create_with_topology).
///
/// # Examples
///
/// ```
/// use tutti_io::{AudioOut, Pcm, WavOut};
///
/// # fn main() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let mut wav = WavOut::create(dir.path().join("tone.wav"), 48_000.0, 2u16, Pcm::Int24)?;
/// wav.write_interleaved(&[0.5, -0.5, 0.25, -0.25]); // two stereo frames
/// wav.finalize()?;
/// # Ok(())
/// # }
/// ```
pub struct WavOut {
    encoder: Encoder<BufWriter<File>>,
    spec: Spec,
    /// The first write failure, kept until [`finalize`](AudioOut::finalize) can
    /// report it.
    ///
    /// [`AudioOut::write`] returns `()` — a block sink has nowhere to put an
    /// error mid-stream — so without this a disk-full or I/O fault silently ends
    /// the recording and hands back a short file that reports success. Keeping
    /// the *first* one is what makes the report meaningful: later failures are
    /// usually the same cause repeated.
    first_error: Option<std::io::Error>,
}

impl std::fmt::Debug for WavOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavOut")
            .field("layout", &self.spec.layout)
            .field("sample_rate", &self.spec.rate)
            .field("format", &self.spec.format)
            .field("failed", &self.first_error.is_some())
            .finish_non_exhaustive()
    }
}

impl WavOut {
    /// Creates (or truncates) the file at `file_path` and writes a WAV header
    /// for `channels` at `sample_rate` (rounded to whole Hz) and `pcm`.
    ///
    /// # Errors
    ///
    /// The `io::Error` from creating the file (a missing directory, a
    /// permission denial, a full disk) or from writing the header.
    pub fn create(
        file_path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
        channels: impl Into<ChannelLayout>,
        pcm: Pcm,
    ) -> std::io::Result<Self> {
        let spec = Spec::new(Format::Wav(pcm), channels.into(), sample_rate.into());
        Self::open(spec, file_path.as_ref())
    }

    /// Like [`create`](Self::create), for the channels `topology` names: the
    /// file's speaker mask (`0`, unassigned, when WAVE cannot state the order)
    /// is that of `topology`, and its width is the file's. The samples are
    /// written as given, never reordered.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create).
    pub fn create_with_topology(
        file_path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
        topology: ChannelTopology,
        pcm: Pcm,
    ) -> std::io::Result<Self> {
        let layout = topology.layout();
        let spec = Spec {
            topology: Some(topology),
            ..Spec::new(Format::Wav(pcm), layout, sample_rate.into())
        };
        Self::open(spec, file_path.as_ref())
    }

    fn open(spec: Spec, path: &Path) -> std::io::Result<Self> {
        let sink = BufWriter::new(File::create(path)?);
        Ok(Self {
            encoder: Encoder::create(&spec, sink)?,
            spec,
            first_error: None,
        })
    }

    /// Lowers the frame limit so a test reaches it without writing 4 GiB.
    #[cfg(test)]
    pub(crate) fn with_max_frames_for_test(self, max_frames: u64) -> Self {
        Self {
            encoder: self.encoder.limit_frames(max_frames),
            ..self
        }
    }

    #[cfg(test)]
    fn write_interleaved_and_finish_for_test(mut self) {
        self.write_interleaved(&vec![0.25; 5 * self.stride()]);
        self.finalize().unwrap();
    }

    /// Returns the declared channel layout — what the WAV header says, and
    /// therefore how many samples per frame
    /// [`write_interleaved`](Self::write_interleaved) expects.
    ///
    /// The same as [`AudioOut::layout`], callable without importing the
    /// trait.
    pub fn layout(&self) -> ChannelLayout {
        self.spec.layout
    }

    /// The interleave stride as a plain `usize`, clamped to at least 1.
    fn stride(&self) -> usize {
        self.spec.layout.count().max(1) as usize
    }

    /// Returns the sample rate written into the header.
    ///
    /// Compare it with the source's before pumping: feeding 48 kHz frames into
    /// a sink that declared 8 kHz produces a valid WAV that plays back six
    /// times too slow, and nothing downstream can detect it.
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.rate
    }

    /// Writes flat interleaved frames at this sink's own declared width.
    ///
    /// `samples` is a flat interleaved buffer holding `frames *
    /// layout().count()` samples. A short trailing frame is ignored.
    ///
    /// A write failure stops the sink and is reported once, from
    /// [`finalize`](AudioOut::finalize). Swallowing it would leave a disk-full
    /// mid-take looking like a complete recording.
    pub fn write_interleaved(&mut self, samples: &[f32]) {
        if self.first_error.is_some() {
            return; // already failed; the file is being abandoned
        }
        let whole = samples.len() / self.stride() * self.stride();
        let block = Interleaved::new(&samples[..whole], self.spec.layout);
        if let Err(e) = self.encoder.write(block) {
            self.first_error = Some(e.into());
        }
    }
}

impl AudioOut for WavOut {
    fn layout(&self) -> ChannelLayout {
        self.spec.layout
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
        // A write that failed mid-stream is reported here, ahead of the
        // finalize itself: the header may well back-patch cleanly over a
        // truncated recording, so finalizing `Ok` would hide the real fault.
        // The encoder is still finished, so the bytes that did land are
        // readable and the caller learns the take is short.
        let finished = self.encoder.finish();
        match self.first_error {
            Some(error) => Err(error),
            None => finished.map(drop).map_err(Into::into),
        }
    }
}

#[cfg(test)]
mod tests;
