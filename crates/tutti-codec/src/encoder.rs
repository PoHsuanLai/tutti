//! The [`Encoder`]: one type over every format.

use crate::error::{Error, Result};
#[cfg(encode)]
use crate::format::Format;
use crate::spec::Spec;
use crate::staged::Staged;
#[cfg(encode)]
use crate::writer::FormatWriter;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_types::{ChannelLayout, Interleaved, Samples};

/// Writes blocks of audio as one file of a [`Format`](crate::Format): the
/// [`FormatWriter`](crate::FormatWriter) of
/// whichever format the [`Spec`] names, behind one type.
///
/// An encoder takes blocks already at the [`Spec`]'s rate and width: it does not
/// pull, resample or dither, and nothing here stops a caller from handing it
/// blocks that were not. The pipeline in front of it is the caller's. [`write`](Self::write) is fallible, so a full disk in the middle
/// of a take is an error at the block that hit it, not a short file that
/// reports success; [`finish`](Self::finish) closes the file exactly once.
///
/// The sink is any `Write + Seek + Send + 'static`: a file, or memory. (`Send`
/// and `'static` because Ogg Vorbis encodes on a thread that takes the sink;
/// the bound is the same whichever features are on, since bounds that followed
/// the features would be worse. In return an `Encoder` is `Send`, so a
/// recording can move it to its pump thread.) A file made with
/// [`create_file`](Encoder::create_file) is written beside its path and moved
/// into place by `finish`; an encoder dropped before that, whether after an
/// error or by [`abort`](Self::abort), removes it and leaves what was at the
/// path alone.
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
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
    /// The first failure of a `write`, kept for `finish` to report.
    first_error: Option<Error>,
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
    #[cfg(feature = "encode-wav")]
    Wav(crate::WavWriter<W>),
    #[cfg(feature = "encode-aiff")]
    Aiff(Box<crate::AiffWriter<W>>),
    #[cfg(feature = "encode-flac")]
    Flac(Box<crate::FlacWriter<W>>),
    #[cfg(feature = "encode-vorbis")]
    Vorbis(Box<crate::VorbisWriter<W>>),
    // No format feature: `Format` has no value, so neither has this.
    #[cfg(not(encode))]
    Never(std::convert::Infallible, std::marker::PhantomData<W>),
}

impl<W: Write + Seek + Send + 'static> Inner<W> {
    #[cfg_attr(not(encode), expect(unused_variables))]
    fn open(spec: &Spec, sink: W) -> Result<Self> {
        match spec.format {
            #[cfg(feature = "encode-wav")]
            Format::Wav(pcm) => Ok(Self::Wav(crate::WavWriter::create(spec, pcm, sink)?)),
            #[cfg(feature = "encode-aiff")]
            Format::Aiff(pcm) => Ok(Self::Aiff(Box::new(crate::AiffWriter::create(
                spec, pcm, sink,
            )?))),
            #[cfg(feature = "encode-flac")]
            Format::Flac { depth, level } => Ok(Self::Flac(Box::new(crate::FlacWriter::create(
                spec, depth, level, sink,
            )?))),
            #[cfg(feature = "encode-vorbis")]
            Format::Vorbis { quality } => Ok(Self::Vorbis(Box::new(crate::VorbisWriter::create(
                spec, quality, sink,
            )?))),
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
            first_error: None,
        })
    }

    /// Lowers the number of frames a WAV file may hold to `frames`; it never
    /// raises the limit the format's 32-bit sizes set, and other formats are
    /// unbounded. For a caller that records into a space it knows.
    #[must_use]
    #[cfg_attr(
        not(feature = "encode-wav"),
        allow(unused_mut, unused_variables, unreachable_code)
    )]
    pub fn limit_frames(mut self, frames: u64) -> Self {
        match &mut self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.limit_frames(frames),
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(_) => {}
            #[cfg(feature = "encode-flac")]
            Inner::Flac(_) => {}
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(_) => {}
            #[cfg(not(encode))]
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
        let written = self.write_block(block);
        if let Err(e) = &written {
            self.first_error.get_or_insert_with(|| e.clone());
        }
        written
    }

    fn write_block(&mut self, block: Interleaved<'_>) -> Result<()> {
        if block.layout() != self.layout {
            return Err(Error::BlockWidth {
                expected: self.layout,
                got: block.layout(),
            });
        }
        match &mut self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.write(block),
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(w) => w.write(block),
            #[cfg(feature = "encode-flac")]
            Inner::Flac(w) => w.write(block),
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(w) => w.write(block),
            #[cfg(not(encode))]
            Inner::Never(never, _) => match *never {},
        }
    }

    /// Closes the file: back-patches its sizes and flushes the sink. Returns
    /// the sink, positioned at the end of the file.
    ///
    /// After a failed [`write`](Self::write) a plain sink gets what was written
    /// closed and handed back (a recording keeps its readable prefix; the
    /// failure was the `write`'s to report). A file made by
    /// [`create_file`](Encoder::create_file) is a take that must be whole, so it
    /// is removed instead, what was at its path is left alone, and the first
    /// failure is returned. Otherwise a file made by `create_file` is moved
    /// into place once the sink has been flushed. This is
    /// [`finalize`](Self::finalize) then [`Finalized::commit`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Encoding`] when closing fails (the file is then
    /// removed if the encoder made it), or the first failed `write` of a file
    /// the encoder made.
    pub fn finish(self) -> Result<W> {
        self.finalize()?.commit()
    }

    /// Closes the file as [`finish`](Self::finish) does, but leaves it where it
    /// was written: a file made by [`create_file`](Encoder::create_file) is
    /// complete beside its path and not yet moved onto it. [`Finalized::commit`]
    /// moves it; dropping the [`Finalized`] removes it and leaves what was at
    /// the path alone.
    ///
    /// The split is for a caller that writes several files and wants every one
    /// closed (the work that can fail for the file's own sake) before any is
    /// put in place.
    ///
    /// # Errors
    ///
    /// As [`finish`](Self::finish), for everything but the move into place.
    pub fn finalize(mut self) -> Result<Finalized<W>> {
        let mut sink: W = match self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.finish()?,
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(w) => w.finish()?,
            #[cfg(feature = "encode-flac")]
            Inner::Flac(w) => w.finish()?,
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(w) => w.finish()?,
            #[cfg(not(encode))]
            Inner::Never(never, _) => match never {},
        };
        // A buffered sink reports a failed write only when it is flushed, and
        // a writer that does not flush would lose it to the sink's `Drop`.
        sink.flush()?;
        let staged = self.staged.take();
        if staged.is_some()
            && let Some(error) = self.first_error.take()
        {
            // Dropping `staged` removes the partial file.
            return Err(error);
        }
        Ok(Finalized { sink, staged })
    }

    /// Discards the file: a file made by [`create_file`](Encoder::create_file)
    /// is removed, and what was at its path is left alone. Dropping the
    /// encoder does the same.
    pub fn abort(self) {}
}

/// An encoder's closed file, not yet put in place ([`Encoder::finalize`]).
///
/// A file made by [`Encoder::create_file`] is complete beside its path and
/// [`commit`](Self::commit) renames it over the path; dropping a `Finalized`
/// first removes the file and leaves what was at the path alone. For a plain
/// sink there is nothing to move and `commit` returns the sink.
#[must_use = "a finalized file is removed when dropped; commit it"]
pub struct Finalized<W> {
    sink: W,
    staged: Option<Staged>,
}

impl<W> std::fmt::Debug for Finalized<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Finalized")
            .field("file", &self.staged.is_some())
            .finish_non_exhaustive()
    }
}

impl<W> Finalized<W> {
    /// Moves the file onto its path (replacing what is there, on Windows too)
    /// and returns the sink. A rename that fails removes the file and
    /// leaves what was at the path alone.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the rename fails.
    pub fn commit(self) -> Result<W> {
        if let Some(staged) = self.staged {
            staged.commit()?;
        }
        Ok(self.sink)
    }
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

    /// Starts a file at `path` that will hold `frames` frames, refusing a
    /// count the format's file cannot hold before the temporary exists.
    ///
    /// For a caller that knows the length up front (an export), so it learns
    /// that a WAV is too long before it renders a frame, from the codec's own
    /// limit ([`crate::Format::max_frames`]) and its own error. It does not lower the
    /// limit on the writes.
    ///
    /// # Errors
    ///
    /// [`Error::WavTooLarge`] when `frames` is past [`crate::Format::max_frames`] for
    /// the spec's width; otherwise as [`create_file`](Self::create_file).
    pub fn create_file_planned(
        spec: &Spec,
        path: impl AsRef<Path>,
        frames: Samples,
    ) -> Result<Self> {
        if let Some(limit_frames) = spec.format.max_frames(spec.layout)
            && frames.get() as u64 > limit_frames
        {
            return Err(Error::WavTooLarge { limit_frames });
        }
        Self::create_file(spec, path)
    }
}

#[cfg(all(test, encode))]
mod tests;
