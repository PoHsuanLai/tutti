//! Ogg Vorbis (`vorbis_rs`). Streams via `encode_audio_block`.
//!
//! `vorbis_rs` takes planar per-channel slices, so each block is deinterleaved
//! on the way through. Vorbis's channel mappings cover mono, stereo, and 3 to 8
//! surround; a width past 8 is vorbis's own limit, reported by its builder.
//!
//! # Channel order
//!
//! The engine's channels are in its own order (SMPTE / WAVE: `FL FR FC LFE ..`);
//! Vorbis I section 4.3.9 fixes a different one for each width from 3 to 8
//! (`FL FC FR` for 3, `FL FC FR RL RR LFE` for 5.1, and so on), and a decoder
//! places stream channel `i` by it. So the writer permutes the planes from the
//! file's [`Spec::topology`] (the engine's default order where it has one) to
//! Vorbis's, by speaker, before encoding. A rear channel and a side channel
//! stand for each other (the engine's 5.1 has sides where Vorbis says rear).
//! Mono and stereo are the same in every order and are not touched.
//!
//! A permutation, not a [`Fold`](tutti_types::Fold): a fold mixes a speaker the
//! destination lacks into its neighbours, and an encoder must never change the
//! signal. Where the topology is not a rearrangement of Vorbis's layout for
//! that width (or is absent for a width the engine has no default order for:
//! 3, 4, 5 and 7) the file is refused ([`SpecError::VorbisLayout`]), because a
//! decoder would read the channels as the speakers Vorbis names and place them
//! wrongly. A width past 8 is "defined by the application" in the spec: its
//! channels are written as they come, and the topology is not consulted.
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
use tutti_types::{ChannelLayout, ChannelTopology, Interleaved, Speaker};
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
        let order = vorbis_order(spec)?;
        let (jobs, queue) = mpsc::sync_channel(QUEUE);
        let (back, spare) = mpsc::channel();
        let (built, ready) = mpsc::sync_channel(1);
        let failure = Arc::new(Mutex::new(None));
        let failed = Arc::clone(&failure);
        // Built on the thread that uses it: the encoder cannot be sent.
        let thread = std::thread::spawn(move || match build(rate, ch, quality, sink) {
            Ok(encoder) => {
                let _ = built.send(Ok(()));
                run(encoder, layout, order.as_deref(), queue, back, &failed)
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

/// Vorbis I section 4.3.9: the speaker of each stream channel, for the widths
/// it defines from 3 to 8.
fn vorbis_speakers(width: u16) -> Option<&'static [Speaker]> {
    use Speaker::*;
    Some(match width {
        3 => &[FrontLeft, FrontCenter, FrontRight],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontCenter, FrontRight, BackLeft, BackRight],
        6 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        7 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackCenter,
            LowFrequency,
        ],
        8 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        _ => return None,
    })
}

/// A rear speaker for the side one and the reverse: the pair a bed may use
/// either of.
fn counterpart(speaker: Speaker) -> Option<Speaker> {
    match speaker {
        Speaker::SideLeft => Some(Speaker::BackLeft),
        Speaker::SideRight => Some(Speaker::BackRight),
        Speaker::BackLeft => Some(Speaker::SideLeft),
        Speaker::BackRight => Some(Speaker::SideRight),
        _ => None,
    }
}

/// Which file channel feeds each Vorbis stream channel: stream channel `i` is
/// file channel `order[i]`. `None` when the stream is the file's own order
/// (mono, stereo, a width past 8, or a file already in Vorbis's order).
///
/// # Errors
///
/// [`SpecError::TopologyWidth`] for a topology of another width than the file,
/// [`SpecError::VorbisLayout`] for a width Vorbis defines whose speakers the
/// topology does not name, or does not have.
fn vorbis_order(spec: &Spec) -> Result<Option<Box<[usize]>>> {
    let width = spec.channels()?;
    let Some(target) = vorbis_speakers(width) else {
        return Ok(None);
    };
    let stated = match &spec.topology {
        Some(t) if t.layout() != spec.layout => {
            return Err(SpecError::TopologyWidth {
                topology: t.layout(),
                file: spec.layout,
            }
            .into());
        }
        Some(t) => Some(t.clone()),
        None => ChannelTopology::smpte(spec.layout),
    };
    let refused = || SpecError::VorbisLayout {
        channels: width,
        topology: spec.topology.clone(),
    };
    let source = stated.ok_or_else(refused)?;
    let have = source.positions();
    let mut taken = vec![false; have.len()];
    let mut order: Vec<Option<usize>> = vec![None; target.len()];
    // Exact speakers first, so a rear never takes the place of a side that is
    // present; then the stand-ins.
    for stand_in in [false, true] {
        for (slot, &want) in target.iter().enumerate() {
            if order[slot].is_some() {
                continue;
            }
            let want = if stand_in {
                counterpart(want)
            } else {
                Some(want)
            };
            let found = want.and_then(|want| {
                have.iter()
                    .enumerate()
                    .position(|(i, &p)| p == want && !taken[i])
            });
            if let Some(i) = found {
                taken[i] = true;
                order[slot] = Some(i);
            }
        }
    }
    let order: Option<Vec<usize>> = order.into_iter().collect();
    let order = order.ok_or_else(refused)?;
    Ok(order
        .iter()
        .enumerate()
        .any(|(slot, &i)| slot != i)
        .then(|| order.into_boxed_slice()))
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

/// The encoder thread: deinterleave each block, put the planes in Vorbis's
/// order, encode it, and finish the
/// stream when the queue closes. After a failure it keeps draining the queue
/// (so the caller is never left blocked) and still finishes, to hand the sink
/// back. `VorbisEncoder::finish` does not flush the sink, so this does.
fn run<W: Write>(
    mut encoder: VorbisEncoder<W>,
    layout: ChannelLayout,
    order: Option<&[usize]>,
    queue: mpsc::Receiver<Vec<f32>>,
    back: mpsc::Sender<Vec<f32>>,
    failure: &Mutex<Option<Error>>,
) -> Result<W> {
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); usize::from(layout.count())];
    // The planes in stream order, moved (not copied) out of `planes`.
    let mut ordered: Vec<Vec<f32>> =
        vec![Vec::new(); if order.is_some() { planes.len() } else { 0 }];
    let mut failed = false;
    for samples in queue {
        if !failed {
            Interleaved::new(&samples, layout).deinterleave_into(&mut planes);
            let result = match order {
                None => encoder.encode_audio_block(&planes),
                Some(order) => {
                    for (slot, &channel) in ordered.iter_mut().zip(order) {
                        *slot = std::mem::take(&mut planes[channel]);
                    }
                    let result = encoder.encode_audio_block(&ordered);
                    for (slot, &channel) in ordered.iter_mut().zip(order) {
                        planes[channel] = std::mem::take(slot);
                    }
                    result
                }
            };
            if let Err(e) = result {
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
