//! Ogg Vorbis (`vorbis_rs`). Streams via `encode_audio_block`.
//!
//! `vorbis_rs` takes planar per-channel slices, so each block is deinterleaved
//! on the way through. Vorbis's channel mappings cover mono, stereo, and 3 to 8
//! surround; a width past 8 is vorbis's own limit, reported by its builder.
//!
//! Vorbis is lossy and float internally: the format has no sample width, and
//! the stream is not dithered.
//!
//! # A thread of its own
//!
//! `VorbisEncoder` holds raw pointers into libvorbis and so is not `Send`,
//! which would make every [`Encoder`](crate::Encoder) unsendable the moment this
//! format is compiled in, and a live recording cannot move its sink to the
//! thread that pumps it. So the encoder lives on a thread it owns, fed whole
//! blocks over a bounded queue, and takes the sink with it; `finish` joins it
//! and hands the sink back. The cost is that a failure on the thread (the sink
//! refusing a page, say) reaches the caller at the next `write` or at `finish`,
//! not at the block that caused it.

use crate::error::{Codec, Error, Result, SpecError};
use crate::format::Quality;
use crate::spec::Spec;
use crate::writer::FormatWriter;
use std::io::{Seek, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use tutti_types::{ChannelLayout, Interleaved};
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

/// Blocks in flight between the caller and the encoder thread.
const QUEUE: usize = 4;

/// Writes an Ogg Vorbis file through the [`FormatWriter`] contract.
///
/// The encoder lives on a thread of its own. A sink that stops accepting data
/// blocks `write` once four blocks are queued, and blocks dropping the writer,
/// which closes the queue and joins the thread before it returns. (That is
/// pump-thread behaviour; nothing here runs on the audio thread.)
pub struct VorbisWriter<W> {
    /// Closed to end the thread.
    jobs: Option<mpsc::SyncSender<Vec<f32>>>,
    /// Block buffers back from the thread, for reuse.
    spare: mpsc::Receiver<Vec<f32>>,
    /// The thread's first failure, until a `write` or `finish` reports it.
    failure: Arc<Mutex<Option<Error>>>,
    thread: Option<JoinHandle<Result<W>>>,
}

impl<W> std::fmt::Debug for VorbisWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VorbisWriter").finish_non_exhaustive()
    }
}

impl<W> Drop for VorbisWriter<W> {
    /// Stops the thread before returning: the queue is closed and the thread
    /// joined, so the sink is dropped (a file closed) by then, which a staged
    /// file's removal needs on Windows.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl<W: Write + Seek + Send + 'static> VorbisWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub fn create(spec: &Spec, quality: Quality, sink: W) -> Result<Self> {
        let rate = spec.header_rate()?;
        let channels = spec.channels()?;
        let ch = u8::try_from(channels).ok().and_then(NonZeroU8::new).ok_or(
            SpecError::TooManyChannels {
                channels,
                max: u16::from(u8::MAX),
            },
        )?;
        let layout = spec.layout;
        let (jobs, queue) = mpsc::sync_channel(QUEUE);
        let (back, spare) = mpsc::channel();
        let (built, ready) = mpsc::sync_channel(1);
        let failure = Arc::new(Mutex::new(None));
        let failed = Arc::clone(&failure);
        // Built on the thread that uses it: the encoder cannot be sent.
        let thread = std::thread::spawn(move || match build(rate, ch, quality, sink) {
            Ok(encoder) => {
                let _ = built.send(Ok(()));
                run(encoder, layout, queue, back, &failed)
            }
            Err(e) => {
                let _ = built.send(Err(()));
                Err(e)
            }
        });
        let mut writer = Self {
            jobs: Some(jobs),
            spare,
            failure,
            thread: Some(thread),
        };
        match ready.recv() {
            Ok(Ok(())) => Ok(writer),
            // The thread holds why it did not start.
            _ => Err(writer.join().err().unwrap_or_else(stopped)),
        }
    }

    fn join(&mut self) -> Result<W> {
        self.jobs = None;
        let thread = self.thread.take().ok_or_else(stopped)?;
        thread
            .join()
            .map_err(|_| Error::encoding(Codec::Ogg, "the encoder thread panicked"))?
    }

    /// The thread's failure, once.
    fn take_failure(&self) -> Option<Error> {
        self.failure.lock().ok()?.take()
    }
}

impl<W: Write + Seek + Send + 'static> FormatWriter for VorbisWriter<W> {
    type Sink = W;

    /// A failure on the encoder thread reaches the first `write` after it, or
    /// `finish`; after it the thread discards blocks.
    fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        // An EMPTY block is vorbis's end-of-stream signal, and a resampler
        // legitimately flushes one after its last real block. Passing it
        // through closes the stream early and leaves the encoder writing
        // garbage pages. Dropping it here is load-bearing.
        if block.is_empty() {
            return Ok(());
        }
        let mut samples = self.spare.try_recv().unwrap_or_default();
        samples.clear();
        samples.extend_from_slice(block.samples());
        match self.jobs.as_ref().map(|jobs| jobs.send(samples)) {
            Some(Ok(())) => Ok(()),
            _ => Err(self.take_failure().unwrap_or_else(stopped)),
        }
    }

    /// Closes the stream and hands the sink back, flushed, even after a failed
    /// `write` that reported it. A failure no `write` has reported yet is
    /// returned instead, and the sink is dropped with it.
    fn finish(mut self) -> Result<Self::Sink> {
        let sink = self.join()?;
        match self.take_failure() {
            Some(e) => Err(e),
            None => Ok(sink),
        }
    }
}

fn stopped() -> Error {
    Error::encoding(Codec::Ogg, "the encoder thread stopped")
}

fn build<W: Write>(
    rate: NonZeroU32,
    channels: NonZeroU8,
    quality: Quality,
    sink: W,
) -> Result<VorbisEncoder<W>> {
    let mut builder = VorbisEncoderBuilder::new(rate, channels, sink)
        .map_err(|e| Error::encoding(Codec::Ogg, format!("failed to create encoder: {e}")))?;
    builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: quality.get(),
    });
    builder
        .build()
        .map_err(|e| Error::encoding(Codec::Ogg, format!("failed to build encoder: {e}")))
}

/// The encoder thread: deinterleave each block, encode it, and finish the
/// stream when the queue closes. After a failure it keeps draining the queue
/// (so the caller is never left blocked) and still finishes, to hand the sink
/// back. `VorbisEncoder::finish` does not flush the sink, so this does.
fn run<W: Write>(
    mut encoder: VorbisEncoder<W>,
    layout: ChannelLayout,
    queue: mpsc::Receiver<Vec<f32>>,
    back: mpsc::Sender<Vec<f32>>,
    failure: &Mutex<Option<Error>>,
) -> Result<W> {
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); usize::from(layout.count())];
    let mut failed = false;
    for samples in queue {
        if !failed {
            Interleaved::new(&samples, layout).deinterleave_into(&mut planes);
            if let Err(e) = encoder.encode_audio_block(&planes) {
                failed = true;
                let e = Error::encoding(Codec::Ogg, format!("encoding failed: {e}"));
                if let Ok(mut slot) = failure.lock() {
                    slot.get_or_insert(e);
                }
            }
        }
        // The caller may have gone; the buffer is then just dropped.
        let _ = back.send(samples);
    }
    let mut sink = encoder
        .finish()
        .map_err(|e| Error::encoding(Codec::Ogg, format!("finalization failed: {e}")))?;
    sink.flush()?;
    Ok(sink)
}

#[cfg(test)]
mod tests;
