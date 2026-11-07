//! The [`Encoder`]: one type over every format.

use crate::error::{Error, Result};
#[cfg(codec)]
use crate::format::Format;
use crate::spec::Spec;
use crate::staged::Staged;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_types::{ChannelLayout, Interleaved};

/// Writes blocks of audio as one file of a [`Format`].
///
/// An encoder takes what it is given: the pipeline that feeds it pulls,
/// resamples and dithers, and hands over blocks already at the [`Spec`]'s rate
/// and width. [`write`](Self::write) is fallible, so a full disk in the middle
/// of a take is an error at the block that hit it, not a short file that
/// reports success; [`finish`](Self::finish) closes the file exactly once.
///
/// The sink is any `Write + Seek + Send + 'static`: a file, or memory. (`Send`
/// and `'static` because Ogg Vorbis encodes on a thread that takes the sink; in
/// return an `Encoder` is `Send`, so a recording can move it to its pump
/// thread.) A file made with
/// [`create_file`](Encoder::create_file) is written beside its path and moved
/// into place by `finish`; an encoder dropped before that, whether after an
/// error or by [`abort`](Self::abort), removes it and leaves what was at the
/// path alone.
///
/// ```
/// # #[cfg(feature = "wav")] {
/// use std::io::Cursor;
/// use tutti_codec::{Encoder, Format, Pcm, Spec};
/// use tutti_types::{ChannelLayout, Interleaved, SampleRate};
///
/// let spec = Spec::new(Format::Wav(Pcm::Int16), ChannelLayout::STEREO, SampleRate(48_000.0));
/// let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new()))?;
/// encoder.write(Interleaved::new(&[0.5, -0.5, 0.25, -0.25], ChannelLayout::STEREO))?;
/// let wav = encoder.finish()?.into_inner();
/// assert_eq!(&wav[..4], b"RIFF");
/// assert_eq!(wav.len(), 44 + 2 * 2 * 2); // a header, two stereo frames of 16 bits
/// # }
/// # Ok::<(), tutti_codec::Error>(())
/// ```
pub struct Encoder<W: Write + Seek + Send + 'static> {
    inner: Inner<W>,
    layout: ChannelLayout,
    /// The unfinished file, when the encoder made one.
    staged: Option<Staged>,
}

impl<W: Write + Seek + Send + 'static> std::fmt::Debug for Encoder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("layout", &self.layout)
            .field("file", &self.staged.is_some())
            .finish_non_exhaustive()
    }
}

enum Inner<W: Write + Seek + Send + 'static> {
    #[cfg(feature = "wav")]
    Wav(crate::wav::Writer<W>),
    #[cfg(feature = "aiff")]
    Aiff(Box<crate::aiff::Writer<W>>),
    #[cfg(feature = "flac")]
    Flac(Box<crate::flac::Writer<W>>),
    #[cfg(feature = "vorbis")]
    Vorbis(Box<crate::vorbis::Writer<W>>),
    // No format feature: `Format` has no value, so neither has this.
    #[cfg(not(codec))]
    Never(std::convert::Infallible, std::marker::PhantomData<W>),
}

impl<W: Write + Seek + Send + 'static> Inner<W> {
    #[cfg_attr(not(codec), expect(unused_variables))]
    fn open(spec: &Spec, sink: W) -> Result<Self> {
        match spec.format {
            #[cfg(feature = "wav")]
            Format::Wav(pcm) => Ok(Self::Wav(crate::wav::Writer::create(spec, pcm, sink)?)),
            #[cfg(feature = "aiff")]
            Format::Aiff(pcm) => Ok(Self::Aiff(Box::new(crate::aiff::Writer::create(
                spec, pcm, sink,
            )?))),
            #[cfg(feature = "flac")]
            Format::Flac { depth, level } => Ok(Self::Flac(Box::new(crate::flac::Writer::create(
                spec, depth, level, sink,
            )?))),
            #[cfg(feature = "vorbis")]
            Format::Vorbis { quality } => Ok(Self::Vorbis(Box::new(
                crate::vorbis::Writer::create(spec, quality, sink)?,
            ))),
        }
    }
}

impl<W: Write + Seek + Send + 'static> Encoder<W> {
    /// Starts a file of `spec` in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// [`Error::Spec`] when the format cannot be written for `spec` (a zero
    /// rate or width, a width past the format's limit, a topology of another
    /// width); [`Error::Encoding`] when the codec rejects it; [`Error::Io`]
    /// when the sink fails on the header.
    pub fn create(spec: &Spec, sink: W) -> Result<Self> {
        spec.channels()?;
        Ok(Self {
            inner: Inner::open(spec, sink)?,
            layout: spec.layout,
            staged: None,
        })
    }

    /// Lowers the number of frames a WAV file may hold to `frames`; it never
    /// raises the limit the format's 32-bit sizes set, and other formats are
    /// unbounded. For a caller that records into a space it knows.
    #[must_use]
    #[cfg_attr(
        not(feature = "wav"),
        allow(unused_mut, unused_variables, unreachable_code)
    )]
    pub fn limit_frames(mut self, frames: u64) -> Self {
        match &mut self.inner {
            #[cfg(feature = "wav")]
            Inner::Wav(w) => w.limit_frames(frames),
            #[cfg(feature = "aiff")]
            Inner::Aiff(_) => {}
            #[cfg(feature = "flac")]
            Inner::Flac(_) => {}
            #[cfg(feature = "vorbis")]
            Inner::Vorbis(_) => {}
            #[cfg(not(codec))]
            Inner::Never(never, _) => match *never {},
        }
        self
    }

    /// Writes one block.
    ///
    /// An empty block writes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::BlockWidth`] for a block of another width than the spec's;
    /// [`Error::Io`] when the sink fails (a full disk); [`Error::WavTooLarge`]
    /// when a WAV file reaches its size limit, after the frames that fit were
    /// written; [`Error::Encoding`] when the codec rejects the block. (Ogg
    /// Vorbis encodes on a thread of its own, so its failure may reach the
    /// next `write` or `finish` instead.) After an error the file is not a complete take: [`finish`](Self::finish) still
    /// closes what was written for a caller that keeps it, and
    /// [`abort`](Self::abort) discards it.
    pub fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
        if block.layout() != self.layout {
            return Err(Error::BlockWidth {
                expected: self.layout,
                got: block.layout(),
            });
        }
        match &mut self.inner {
            #[cfg(feature = "wav")]
            Inner::Wav(w) => w.write(block),
            #[cfg(feature = "aiff")]
            Inner::Aiff(w) => w.write(block),
            #[cfg(feature = "flac")]
            Inner::Flac(w) => w.write(block),
            #[cfg(feature = "vorbis")]
            Inner::Vorbis(w) => w.write(block),
            #[cfg(not(codec))]
            Inner::Never(never, _) => match *never {},
        }
    }

    /// Closes the file: back-patches its sizes, flushes the sink and, for a
    /// file made by [`create_file`](Encoder::create_file), moves it into place.
    /// Returns the sink, positioned at the end of the file.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Encoding`] when closing fails; the file is
    /// then removed if the encoder made it.
    pub fn finish(mut self) -> Result<W> {
        let sink = match self.inner {
            #[cfg(feature = "wav")]
            Inner::Wav(w) => w.finish()?,
            #[cfg(feature = "aiff")]
            Inner::Aiff(w) => w.finish()?,
            #[cfg(feature = "flac")]
            Inner::Flac(w) => w.finish()?,
            #[cfg(feature = "vorbis")]
            Inner::Vorbis(w) => w.finish()?,
            #[cfg(not(codec))]
            Inner::Never(never, _) => match never {},
        };
        if let Some(staged) = self.staged.take() {
            staged.commit()?;
        }
        Ok(sink)
    }

    /// Discards the file: a file made by [`create_file`](Encoder::create_file)
    /// is removed, and what was at its path is left alone. Dropping the
    /// encoder does the same.
    pub fn abort(self) {}
}

impl Encoder<BufWriter<File>> {
    /// Starts a file at `path`.
    ///
    /// The file is written to a temporary beside `path` and renamed over it by
    /// [`finish`](Self::finish), so `path` holds either what it held before or
    /// a complete new file, never a partial take. The rename replaces an
    /// existing file (on Windows too) and a symlink at `path` rather than
    /// writing through it; the new file has the default permissions.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create), and [`Error::Io`] when the temporary cannot
    /// be created.
    pub fn create_file(spec: &Spec, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let staged = Staged::beside(path);
        let sink = BufWriter::new(File::create(staged.temp())?);
        // Created before the header is written: a failure from here on drops
        // `staged`, which removes the temporary.
        let mut encoder = Self::create(spec, sink)?;
        encoder.staged = Some(staged);
        Ok(encoder)
    }
}
