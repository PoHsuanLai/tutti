//! FLAC (flacenc). Streams both ways: PCM in a block at a time, frames out to
//! the sink as they are encoded.
//!
//! flacenc's own entry point, `encode_with_fixed_block_size`, is a pull loop
//! over a `Source` that returns the whole compressed `Stream` at the end, and
//! it zero-pads the last block to the full block size. That gives the file
//! frames past the end of the audio (a decoder that does not trim to
//! STREAMINFO's sample count plays them), an MD5 over that padding, and, for
//! mono, padding that is stale samples from a reused buffer rather than zeros.
//!
//! So this drives flacenc's frame encoder, `encode_fixed_size_frame`, itself:
//! every full [`BLOCK_SIZE`] block is encoded on a worker and written in order
//! as soon as it and every frame before it are done; the last block is encoded
//! at its own length. The STREAMINFO is written as a placeholder first and
//! rewritten once the totals and the MD5 are known. Every full-size frame is
//! the one `encode_with_fixed_block_size` writes (the same function, block,
//! frame number and stream info).

use crate::error::{Codec, Error, Result};
use crate::format::FlacLevel;
use crate::pcm::IntDepth;
use crate::spec::Spec;
use crate::writer::FormatWriter;
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::constant::MIN_BLOCK_SIZE;
use flacenc::encode_fixed_size_frame;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};
use md5::{Digest, Md5};
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tutti_types::Interleaved;

/// Frames per FLAC block, every block but the last.
const BLOCK_SIZE: usize = 4096;

/// A FLAC file being written.
/// Writes a FLAC file through the [`FormatWriter`] contract.
pub struct FlacWriter<W: Write + Seek> {
    sink: W,
    frames: FrameWriter,
    depth: IntDepth,
    /// The block being filled, interleaved, at most [`BLOCK_SIZE`] frames.
    pcm: Vec<i32>,
    channels: usize,
}

impl<W: Write + Seek> std::fmt::Debug for FlacWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacWriter").finish_non_exhaustive()
    }
}

impl<W: Write + Seek> FlacWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub fn create(spec: &Spec, depth: IntDepth, level: FlacLevel, mut sink: W) -> Result<Self> {
        let rate = spec.header_rate()?.get() as usize;
        let channels = usize::from(spec.channels()?);
        let bits = usize::from(depth.bits());

        let verify = |c: EncoderConfig| {
            c.into_verified()
                .map_err(|e| Error::encoding(Codec::Flac, format!("invalid config: {e:?}")))
        };
        let configs = Configs {
            full: verify(encoder_config(level))?,
            short: verify(short_block_config(level))?,
            #[cfg(test)]
            panic_at: None,
        };
        let info = StreamInfo::new(rate, channels, bits)
            .map_err(|e| Error::encoding(Codec::Flac, format!("invalid stream: {e:?}")))?;

        // A placeholder of the final header's size (STREAMINFO is fixed
        // length), rewritten once the totals and the MD5 are known.
        sink.write_all(&header_bytes(&info)?)?;

        let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
        let frames = FrameWriter::start(workers, Arc::new(configs), info.clone(), channels, bits);
        Ok(Self {
            sink,
            frames,
            depth,
            pcm: Vec::with_capacity(BLOCK_SIZE * channels),
            channels,
        })
    }
}

impl<W: Write + Seek> FormatWriter for FlacWriter<W> {
    type Sink = W;

    fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
        let mut samples = block.samples();
        while !samples.is_empty() {
            let take = (BLOCK_SIZE * self.channels - self.pcm.len()).min(samples.len());
            let depth = self.depth;
            self.pcm
                .extend(samples[..take].iter().map(|&s| depth.quantize(s)));
            samples = &samples[take..];
            if self.pcm.len() == BLOCK_SIZE * self.channels {
                let queued = self.frames.block(&self.pcm, &mut self.sink);
                // Cleared whatever came of it: the block is in the writer's
                // hands now, and a failed one must not be queued again.
                self.pcm.clear();
                queued?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Self::Sink> {
        if !self.pcm.is_empty() && !self.frames.failed {
            self.frames.block(&self.pcm, &mut self.sink)?;
        }
        let info = self.frames.finish(&mut self.sink)?;
        self.sink.flush()?;
        self.sink.seek(SeekFrom::Start(0))?;
        self.sink.write_all(&header_bytes(&info)?)?;
        self.sink.seek(SeekFrom::End(0))?;
        self.sink.flush()?;
        Ok(self.sink)
    }
}

/// `fLaC` and the STREAMINFO block, as flacenc writes a `Stream` with no
/// frames. Always 42 bytes, which is what lets the final one overwrite the
/// placeholder.
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>> {
    let mut sink = ByteSink::new();
    Stream::with_stream_info(info.clone())
        .write(&mut sink)
        .map_err(|e| Error::encoding(Codec::Flac, format!("failed to write header: {e:?}")))?;
    Ok(sink.into_inner())
}

/// The coding options, for a block of flacenc's minimum length or more, and
/// for a last block shorter than that.
struct Configs {
    full: Verified<EncoderConfig>,
    short: Verified<EncoderConfig>,
    /// Test seam: the worker encoding this frame number panics.
    #[cfg(test)]
    panic_at: Option<usize>,
}

/// What a worker hands back: the frame's number, the frame, and the buffer
/// it was encoded from, for reuse.
type Encoded = (usize, std::result::Result<Frame, String>, FrameBuf);

/// Encodes blocks on `workers` threads and writes their frames in order.
///
/// The caller hands it one interleaved block at a time ([`block`]); a full
/// one is [`BLOCK_SIZE`] frames, and a shorter one is the last. Frames are
/// written to the file as soon as every frame before them is, so at most
/// `2 × workers` blocks are in flight and nothing accumulates.
///
/// [`block`]: Self::block
struct FrameWriter {
    threads: Vec<JoinHandle<()>>,
    jobs: Option<mpsc::SyncSender<(usize, FrameBuf)>>,
    done: mpsc::Receiver<Encoded>,
    /// Buffers back from the workers, ready to refill.
    free: Vec<FrameBuf>,
    /// Encoded frames waiting for an earlier one.
    pending: BTreeMap<usize, Frame>,
    /// Frames handed out, and frames written.
    sent: usize,
    written: usize,
    in_flight_cap: usize,
    /// A frame failed to encode or to reach the sink: the frame is lost, so
    /// the stream cannot be completed and nothing waits for it.
    failed: bool,
    info: StreamInfo,
    md5: Md5,
    md5_bytes: Vec<u8>,
    bytes_per_sample: usize,
    channels: usize,
    sink: ByteSink,
}

impl FrameWriter {
    fn start(
        workers: usize,
        configs: Arc<Configs>,
        info: StreamInfo,
        channels: usize,
        bits: usize,
    ) -> Self {
        let workers = workers.max(1);
        let (job_tx, job_rx) = mpsc::sync_channel::<(usize, FrameBuf)>(workers);
        let (done_tx, done_rx) = mpsc::channel::<Encoded>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let threads = (0..workers)
            .map(|_| {
                let job_rx = Arc::clone(&job_rx);
                let configs = Arc::clone(&configs);
                let done_tx = done_tx.clone();
                // Every frame is encoded against the stream info as it stood
                // before any frame, as flacenc's own parallel encoder does: a
                // frame's encoding reads only its rate, width and depth.
                let info = info.clone();
                std::thread::spawn(move || {
                    loop {
                        // The lock is held only to take one job; a closed queue ends
                        // the worker.
                        let job = job_rx.lock().map(|rx| rx.recv());
                        let Ok(Ok((n, buf))) = job else { return };
                        let config = if buf.size() < MIN_BLOCK_SIZE {
                            &configs.short
                        } else {
                            &configs.full
                        };
                        // A panic is caught and sent back as an error: the producer
                        // waits on `done` while this worker's sender is alive in its
                        // siblings, so a worker that unwound silently would leave it
                        // waiting forever.
                        let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            #[cfg(test)]
                            if configs.panic_at == Some(n) {
                                panic!("injected at frame {n}");
                            }
                            encode_fixed_size_frame(config, &buf, n, &info).map(|mut f| {
                                f.precompute_bitstream();
                                f
                            })
                        }))
                        .map_err(|payload| {
                            let msg = payload
                                .downcast_ref::<&str>()
                                .map(|s| (*s).to_owned())
                                .or_else(|| payload.downcast_ref::<String>().cloned())
                                .unwrap_or_default();
                            format!("panicked: {msg}")
                        })
                        .and_then(|r| r.map_err(|e| format!("{e:?}")));
                        if done_tx.send((n, frame, buf)).is_err() {
                            return;
                        }
                    }
                })
            })
            .collect();
        Self {
            threads,
            jobs: Some(job_tx),
            done: done_rx,
            free: Vec::new(),
            pending: BTreeMap::new(),
            sent: 0,
            written: 0,
            in_flight_cap: 2 * workers,
            failed: false,
            info,
            md5: Md5::new(),
            md5_bytes: Vec::new(),
            bytes_per_sample: bits.div_ceil(8),
            channels,
            sink: ByteSink::new(),
        }
    }

    /// Queue one interleaved block: [`BLOCK_SIZE`] frames, or fewer for the
    /// last.
    fn block<W: Write>(&mut self, pcm: &[i32], out: &mut W) -> Result<()> {
        if self.failed {
            return Err(lost_frame());
        }
        let frames = pcm.len() / self.channels;
        // The MD5 is over the samples as the decoder will return them: the
        // first `bytes_per_sample` bytes of each, little-endian, in
        // interleaved order (and, unlike flacenc's, over no padding).
        self.md5_bytes.clear();
        for v in pcm {
            self.md5_bytes
                .extend_from_slice(&v.to_le_bytes()[..self.bytes_per_sample]);
        }
        self.md5.update(&self.md5_bytes);

        let mut buf = match self.free.pop() {
            Some(buf) => buf,
            None => FrameBuf::with_size(self.channels, BLOCK_SIZE)
                .map_err(|e| Error::encoding(Codec::Flac, format!("frame buffer: {e:?}")))?,
        };
        // The last block is encoded at its own length rather than padded.
        // `resize` is unchecked, which is what lets that length be under
        // flacenc's 64-frame minimum: FLAC allows any length for the last
        // block.
        buf.resize(frames);
        buf.fill_interleaved(pcm)
            .map_err(|e| Error::encoding(Codec::Flac, format!("frame buffer: {e:?}")))?;

        while self.sent - self.written >= self.in_flight_cap {
            if !self.receive(out, true)? {
                return Err(Error::encoding(Codec::Flac, "a worker stopped"));
            }
        }
        let jobs = self
            .jobs
            .as_ref()
            .expect("blocks are queued before `finish`");
        jobs.send((self.sent, buf))
            .map_err(|_| Error::encoding(Codec::Flac, "a worker stopped"))?;
        self.sent += 1;
        while self.receive(out, false)? {}
        Ok(())
    }

    /// Take one encoded frame back (waiting for it when `wait`), then write
    /// every frame that is next in order. Returns whether one was taken.
    fn receive<W: Write>(&mut self, out: &mut W, wait: bool) -> Result<bool> {
        let taken = self.take_one(out, wait);
        if taken.is_err() {
            self.failed = true;
        }
        taken
    }

    fn take_one<W: Write>(&mut self, out: &mut W, wait: bool) -> Result<bool> {
        let got = if wait {
            self.done.recv().ok()
        } else {
            self.done.try_recv().ok()
        };
        let Some((n, frame, mut buf)) = got else {
            return Ok(false);
        };
        let frame =
            frame.map_err(|e| Error::encoding(Codec::Flac, format!("encoding failed: {e}")))?;
        buf.resize(BLOCK_SIZE);
        self.free.push(buf);
        self.pending.insert(n, frame);
        while let Some(frame) = self.pending.remove(&self.written) {
            self.info.update_frame_info(&frame);
            self.sink.clear();
            frame.write(&mut self.sink).map_err(|e| {
                Error::encoding(Codec::Flac, format!("failed to write frame: {e:?}"))
            })?;
            out.write_all(self.sink.as_slice())?;
            self.written += 1;
        }
        Ok(true)
    }

    /// Close the queue, write every remaining frame, and return the final
    /// STREAMINFO.
    fn finish<W: Write>(mut self, out: &mut W) -> Result<StreamInfo> {
        // Dropping the sender is what ends the workers.
        self.jobs = None;
        if self.failed {
            return Err(lost_frame());
        }
        while self.written < self.sent {
            if !self.receive(out, true)? {
                return Err(Error::encoding(Codec::Flac, "a worker stopped"));
            }
        }
        for thread in self.threads.drain(..) {
            // A worker's panic was caught and reported as a frame error.
            let _ = thread.join();
        }
        // A fixed-blocksize stream's STREAMINFO states the block size every
        // block but the last has, whatever the last one's; an empty stream
        // states it too, rather than `update_frame_info`'s unset 65535 and 0.
        self.info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|e| Error::encoding(Codec::Flac, format!("block size: {e:?}")))?;
        if self.written == 0 {
            // Zero is "unknown"; unset, the two would read u32::MAX and 0.
            self.info
                .set_frame_sizes(0, 0)
                .map_err(|e| Error::encoding(Codec::Flac, format!("frame size: {e:?}")))?;
        }
        // `update_frame_info` has summed every block's length, the last at its
        // own, so the sample count is already exact.
        self.info.set_md5_digest(&self.md5.finalize().into());
        Ok(self.info)
    }
}

fn lost_frame() -> Error {
    Error::encoding(
        Codec::Flac,
        "an earlier frame failed, so the stream cannot be completed",
    )
}

/// flacenc's coding options for a level, modelled loosely on libFLAC's
/// presets. [`Fastest`](FlacLevel::Fastest) codes with fixed predictors only;
/// the next adds LPC at an exact order (flacenc's `lpc_order` is the order used,
/// not a searched maximum): 6, 8, then 12 at [`Best`](FlacLevel::Best). Every
/// level but the first tries mid/side and left/right-side stereo.
///
/// Where this departs from libFLAC: its level 3 has no stereo decorrelation,
/// and its levels 0 to 2 use 1152-sample blocks, while the block size here is
/// [`BLOCK_SIZE`] at every level. Every level is lossless.
fn encoder_config(level: FlacLevel) -> EncoderConfig {
    let mut config = EncoderConfig::default();
    let side = level != FlacLevel::Fastest;
    config.stereo_coding.use_leftside = side;
    config.stereo_coding.use_rightside = side;
    config.stereo_coding.use_midside = side;
    config.subframe_coding.use_lpc = !matches!(level, FlacLevel::Fastest | FlacLevel::Fast);
    config.subframe_coding.qlpc.lpc_order = match level {
        FlacLevel::Fastest | FlacLevel::Fast | FlacLevel::Medium => 6,
        FlacLevel::Standard => 8,
        FlacLevel::Best => 12,
    };
    config
}

/// The coding options for a last block under flacenc's [`MIN_BLOCK_SIZE`]
/// (64 frames): `level`'s, with only constant and verbatim subframes.
///
/// flacenc's predictive coders assume at least that many frames (its Rice
/// partition search underflows below it), but FLAC allows any length for the
/// last block, and a verbatim subframe of under 64 samples costs at most a
/// few hundred bytes once per file.
fn short_block_config(level: FlacLevel) -> EncoderConfig {
    let mut config = encoder_config(level);
    config.subframe_coding.use_fixed = false;
    config.subframe_coding.use_lpc = false;
    config
}

#[cfg(test)]
mod tests;
