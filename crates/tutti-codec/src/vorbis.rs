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
use std::io::{Seek, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::sync::mpsc;
use std::thread::JoinHandle;
use tutti_types::{ChannelLayout, Interleaved};
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

/// Blocks in flight between the caller and the encoder thread.
const QUEUE: usize = 4;

pub(crate) struct Writer<W> {
    /// Closed to end the thread.
    jobs: Option<mpsc::SyncSender<Vec<f32>>>,
    /// Block buffers back from the thread, for reuse.
    spare: mpsc::Receiver<Vec<f32>>,
    thread: Option<JoinHandle<Result<W>>>,
}

impl<W: Write + Seek + Send + 'static> Writer<W> {
    pub(crate) fn create(spec: &Spec, quality: Quality, sink: W) -> Result<Self> {
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
        // Built on the thread that uses it: the encoder cannot be sent.
        let thread = std::thread::spawn(move || match build(rate, ch, quality, sink) {
            Ok(encoder) => {
                let _ = built.send(Ok(()));
                run(encoder, layout, queue, back)
            }
            Err(e) => {
                let _ = built.send(Err(()));
                Err(e)
            }
        });
        let mut writer = Self {
            jobs: Some(jobs),
            spare,
            thread: Some(thread),
        };
        match ready.recv() {
            Ok(Ok(())) => Ok(writer),
            // The thread holds why it did not start.
            _ => Err(writer.join().err().unwrap_or_else(stopped)),
        }
    }

    pub(crate) fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
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
        let sent = self.jobs.as_ref().map(|jobs| jobs.send(samples));
        match sent {
            Some(Ok(())) => Ok(()),
            // The thread ended on a failure, which it holds for us.
            _ => Err(self.join().err().unwrap_or_else(stopped)),
        }
    }

    pub(crate) fn finish(mut self) -> Result<W> {
        self.jobs = None;
        self.join()
    }

    fn join(&mut self) -> Result<W> {
        self.jobs = None;
        let thread = self.thread.take().ok_or_else(stopped)?;
        thread
            .join()
            .map_err(|_| Error::encoding(Codec::Ogg, "the encoder thread panicked"))?
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
/// stream when the queue closes.
fn run<W: Write>(
    mut encoder: VorbisEncoder<W>,
    layout: ChannelLayout,
    queue: mpsc::Receiver<Vec<f32>>,
    back: mpsc::Sender<Vec<f32>>,
) -> Result<W> {
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); usize::from(layout.count())];
    for samples in queue {
        Interleaved::new(&samples, layout).deinterleave_into(&mut planes);
        encoder
            .encode_audio_block(&planes)
            .map_err(|e| Error::encoding(Codec::Ogg, format!("encoding failed: {e}")))?;
        // The caller may have gone; the buffer is then just dropped.
        let _ = back.send(samples);
    }
    encoder
        .finish()
        .map_err(|e| Error::encoding(Codec::Ogg, format!("finalization failed: {e}")))
}
