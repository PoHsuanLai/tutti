//! [`FileOut`]: an [`AudioOut`] that writes a file of any [`Format`](crate::Format).

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use tutti_types::{AudioOut, ChannelLayout, Interleaved, SampleRate};

use crate::encoder::Encoder;
use crate::spec::Spec;

/// A file sink: the [`AudioOut`] a recording writes into, as a file of any
/// [`Format`](crate::Format) this build can write.
///
/// The write edge, the mirror of `FileIn` (with a `decode-*` feature): `FileIn` is the
/// codec's [`AudioIn`](tutti_types::AudioIn) and this is its
/// [`AudioOut`]. It is an [`Encoder`] behind that contract, the same writer
/// an export bounces through, so a recorded and an exported file agree
/// sample-for-sample and byte-for-byte for a [`Spec`]. One difference is
/// deliberate: an export dithers before quantizing and a live capture does
/// not, because noise shaping is a decision a capture path should not make
/// silently.
///
/// It writes incrementally through a buffered file writer, so a take of any
/// length is never held in memory. Frames are whole: a trailing part of a
/// frame in a [`write`](AudioOut::write) is ignored, so a caller's odd slice
/// cannot rotate the file's channels.
///
/// Writes do blocking file I/O: drive the sink from a thread of its own
/// (`tutti-session`'s `Recorder` is one), never from the audio thread.
///
/// # Failure
///
/// [`AudioOut::write`] returns `()`, so a failure has nowhere to go
/// mid-stream: the **first** write error (a full disk, or a WAV reaching its
/// 4 GiB limit with [`Error::WavTooLarge`](crate::Error::WavTooLarge)) is kept,
/// [`is_failed`](AudioOut::is_failed) turns `true`, and later writes are
/// skipped. [`finalize`](AudioOut::finalize) must run for the file to be
/// readable: it closes what was written even after a failure (a recording
/// keeps its readable prefix) and returns the first error, else the result of
/// closing.
///
/// The file is written in place, not staged beside its path like
/// [`Encoder::create_file`]: a take cut short by a fault or a crash is a
/// recording worth keeping, which a staged file removes. For WAV, the writer's drop patches the header, so a
/// take dropped without finalizing (a pump thread that panicked) still
/// opens.
///
/// The sample rate is the [`Spec`]'s: [`AudioIn`](tutti_types::AudioIn)
/// carries none, so build the spec at the source's rate or the file plays at
/// the wrong speed.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use tutti_codec::{FileOut, Format, Pcm, Spec};
/// use tutti_types::{AudioOut, ChannelLayout, SampleRate};
///
/// # fn main() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let spec = Spec::new(Format::Wav(Pcm::Int24), ChannelLayout::STEREO, SampleRate(48_000.0));
/// let mut out = FileOut::create(dir.path().join("tone.wav"), &spec)?;
/// out.write(&[0.5, -0.5, 0.25, -0.25]); // two stereo frames
/// out.finalize()?;
/// # Ok(())
/// # }
/// # }
/// ```
pub struct FileOut {
    encoder: Encoder<BufWriter<File>>,
    spec: Spec,
    /// The first write failure, kept until [`finalize`](AudioOut::finalize)
    /// can report it. Keeping the *first* is what makes the report meaningful:
    /// later failures are usually the same cause repeated.
    first_error: Option<std::io::Error>,
}

impl std::fmt::Debug for FileOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileOut")
            .field("layout", &self.spec.layout)
            .field("sample_rate", &self.spec.rate)
            .field("format", &self.spec.format)
            .field("failed", &self.first_error.is_some())
            .finish_non_exhaustive()
    }
}

impl FileOut {
    /// Creates (or truncates) the file at `path` and writes the header of
    /// `spec`.
    ///
    /// # Errors
    ///
    /// The `io::Error` from creating the file (a missing directory, a
    /// permission denial), or the codec's error from the header (the
    /// [`Spec`] cannot be written, or the disk is full) as an `io::Error`.
    pub fn create(path: impl AsRef<Path>, spec: &Spec) -> std::io::Result<Self> {
        let sink = BufWriter::new(File::create(path.as_ref())?);
        Ok(Self {
            encoder: Encoder::create(spec, sink)?,
            spec: spec.clone(),
            first_error: None,
        })
    }

    /// What the file was created as.
    #[must_use]
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The sample rate the header declares.
    ///
    /// Compare it with the source's before pumping: 48 kHz frames fed to a
    /// sink that declared 8 kHz make a valid file that plays six times too
    /// slow, and nothing downstream can detect it.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.rate
    }

    /// Lowers the number of frames a WAV file may hold to `frames`
    /// ([`Encoder::limit_frames`]): for a caller that records into a space it
    /// knows, and for a test that reaches the limit without writing 4 GiB.
    /// The write that would cross it fails the sink like any other write
    /// failure.
    #[must_use]
    pub fn limit_frames(self, frames: u64) -> Self {
        Self {
            encoder: self.encoder.limit_frames(frames),
            ..self
        }
    }
}

impl AudioOut for FileOut {
    fn layout(&self) -> ChannelLayout {
        self.spec.layout
    }

    /// Writes flat interleaved samples **already at this sink's own width**:
    /// `frames.len() / layout().count()` whole frames, a short trailing frame
    /// ignored. A caller feeding another width folds first, through
    /// [`tutti_types::Fold`].
    ///
    /// A failure stops the sink and is reported once, from
    /// [`finalize`](AudioOut::finalize). Swallowing it would leave a disk-full
    /// mid-take looking like a complete recording.
    fn write(&mut self, frames: &[f32]) {
        if self.first_error.is_some() {
            return; // already failed; the file is being abandoned
        }
        // `Interleaved` views whole frames only: a trailing part of one is
        // ignored, so an odd slice cannot rotate the file's channels.
        let block = Interleaved::new(frames, self.spec.layout);
        if let Err(e) = self.encoder.write(block) {
            self.first_error = Some(e.into());
        }
    }

    fn is_failed(&self) -> bool {
        self.first_error.is_some()
    }

    fn finalize(self) -> std::io::Result<()> {
        // A write that failed mid-stream is reported ahead of the finish
        // itself: the header may well back-patch cleanly over a truncated
        // recording, so finishing `Ok` would hide the real fault. The encoder
        // is still finished, so the bytes that did land are readable.
        let finished = self.encoder.finish();
        match self.first_error {
            Some(error) => Err(error),
            None => finished.map(drop).map_err(Into::into),
        }
    }
}

#[cfg(all(test, feature = "encode-wav"))]
mod tests;
