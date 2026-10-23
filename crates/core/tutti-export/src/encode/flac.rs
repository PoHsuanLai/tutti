//! FLAC (flacenc). Streams both ways: PCM in a block at a time, frames out to
//! the file as they are encoded.
//!
//! flacenc's own entry point, `encode_with_fixed_block_size`, is a pull loop
//! over a `Source` that returns the whole compressed `Stream` at the end, and
//! it zero-pads the last block to the full block size. That gives the file
//! frames past the end of the audio (a decoder that does not trim to
//! STREAMINFO's sample count plays them), an MD5 over that padding, and, for
//! mono, padding that is stale samples from a reused buffer rather than zeros.
//!
//! So this drives flacenc's frame encoder, `encode_fixed_size_frame`, itself:
//! [`pump_blocks`](crate::encode::pump_blocks) pushes the gated, resampled
//! and dithered render; every full [`BLOCK_SIZE`] block is encoded on a worker
//! and written in order as soon as it and every frame before it are done; the
//! last block is encoded at its own length. The STREAMINFO is written as a
//! placeholder first and rewritten once the totals and the MD5 are known. Every
//! full-size frame is the one `encode_with_fixed_block_size` writes (the same
//! function, block, frame number and stream info).

use crate::config::ExportConfig;
use crate::encode::Encoder;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::constant::MIN_BLOCK_SIZE;
use flacenc::encode_fixed_size_frame;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};
use md5::{Digest, Md5};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;

/// Frames per FLAC block, every block but the last.
const BLOCK_SIZE: usize = 4096;

pub(crate) struct FlacEncoder {
    path: PathBuf,
    compression_level: u8,
    bit_depth: BitDepth,
}

impl FlacEncoder {
    pub(crate) fn create(
        path: &std::path::Path,
        config: &ExportConfig,
        opts: crate::options::Flac,
    ) -> Result<Self> {
        if config.encode.bit_depth == BitDepth::Float32 {
            return Err(Error::UnsupportedFormat(
                "FLAC does not support 32-bit float".into(),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            compression_level: opts.compression_level,
            bit_depth: config.encode.bit_depth,
        })
    }
}

impl Encoder for FlacEncoder {
    fn encode(
        self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        let bits = bits_for(self.bit_depth);
        let bit_depth = self.bit_depth;
        // Once, at entry: the stride is a runtime value.
        let ch = config.encode.channels.count() as usize;
        let rate = crate::encode::encoder_rate(config) as usize;

        let verify = |c: EncoderConfig| {
            c.into_verified()
                .map_err(|e| Error::Encoding(format!("Invalid FLAC config: {e:?}")))
        };
        let configs = Configs {
            full: verify(encoder_config(self.compression_level))?,
            short: verify(short_block_config(self.compression_level))?,
            #[cfg(test)]
            panic_at: None,
        };
        let info = StreamInfo::new(rate, ch, bits)
            .map_err(|e| Error::Encoding(format!("Invalid FLAC stream: {e:?}")))?;

        let mut file = BufWriter::new(File::create(&self.path)?);
        // A placeholder of the final header's size (STREAMINFO is fixed
        // length), rewritten once the totals and the MD5 are known.
        file.write_all(&header_bytes(&info)?)?;

        let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
        let info = std::thread::scope(|scope| {
            let mut writer = FrameWriter::start(scope, workers, &configs, info, ch, bits);
            let mut pcm: Vec<i32> = Vec::with_capacity(BLOCK_SIZE * ch);
            // Through `pump_blocks`, like every other format: that is what
            // applies the gate, the resample and the dither.
            let pumped = crate::encode::pump_blocks(src, source_rate, plan, config, |frames| {
                let mut samples = frames.samples();
                while !samples.is_empty() {
                    let take = (BLOCK_SIZE * ch - pcm.len()).min(samples.len());
                    pcm.extend(samples[..take].iter().map(|&s| f32_to_i32(s, bit_depth)));
                    samples = &samples[take..];
                    if pcm.len() == BLOCK_SIZE * ch {
                        writer.block(&pcm, &mut file)?;
                        pcm.clear();
                    }
                }
                Ok(())
            });
            // Finish the workers even when the pump failed, so the scope's
            // join does not wait on a worker blocked on a full queue.
            let tail = match pumped {
                Ok(()) if !pcm.is_empty() => writer.block(&pcm, &mut file),
                other => other,
            };
            let finished = writer.finish(&mut file);
            tail.and(finished)
        })?;

        let mut file = file.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.rewind()?;
        file.write_all(&header_bytes(&info)?)?;
        file.flush()?;
        Ok(())
    }
}

/// `fLaC` and the STREAMINFO block, as flacenc writes a `Stream` with no
/// frames. Always 42 bytes, which is what lets the final one overwrite the
/// placeholder.
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>> {
    let mut sink = ByteSink::new();
    Stream::with_stream_info(info.clone())
        .write(&mut sink)
        .map_err(|e| Error::Encoding(format!("Failed to write FLAC header: {e:?}")))?;
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
    info: StreamInfo,
    md5: Md5,
    md5_bytes: Vec<u8>,
    bytes_per_sample: usize,
    channels: usize,
    sink: ByteSink,
}

impl FrameWriter {
    fn start<'scope, 'env>(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        workers: usize,
        configs: &'env Configs,
        info: StreamInfo,
        channels: usize,
        bits: usize,
    ) -> Self {
        let workers = workers.max(1);
        let (job_tx, job_rx) = mpsc::sync_channel::<(usize, FrameBuf)>(workers);
        let (done_tx, done_rx) = mpsc::channel::<Encoded>();
        let job_rx = std::sync::Arc::new(Mutex::new(job_rx));
        for _ in 0..workers {
            let job_rx = std::sync::Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            // Every frame is encoded against the stream info as it stood
            // before any frame, as flacenc's own parallel encoder does: a
            // frame's encoding reads only its rate, width and depth.
            let info = info.clone();
            scope.spawn(move || loop {
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
            });
        }
        Self {
            jobs: Some(job_tx),
            done: done_rx,
            free: Vec::new(),
            pending: BTreeMap::new(),
            sent: 0,
            written: 0,
            in_flight_cap: 2 * workers,
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
                .map_err(|e| Error::Encoding(format!("FLAC frame buffer: {e:?}")))?,
        };
        // The last block is encoded at its own length rather than padded.
        // `resize` is unchecked, which is what lets that length be under
        // flacenc's 64-frame minimum: FLAC allows any length for the last
        // block.
        buf.resize(frames);
        buf.fill_interleaved(pcm)
            .map_err(|e| Error::Encoding(format!("FLAC frame buffer: {e:?}")))?;

        while self.sent - self.written >= self.in_flight_cap {
            if !self.receive(out, true)? {
                return Err(Error::Encoding("a FLAC worker stopped".into()));
            }
        }
        let jobs = self
            .jobs
            .as_ref()
            .expect("blocks are queued before `finish`");
        jobs.send((self.sent, buf))
            .map_err(|_| Error::Encoding("a FLAC worker stopped".into()))?;
        self.sent += 1;
        while self.receive(out, false)? {}
        Ok(())
    }

    /// Take one encoded frame back (waiting for it when `wait`), then write
    /// every frame that is next in order. Returns whether one was taken.
    fn receive<W: Write>(&mut self, out: &mut W, wait: bool) -> Result<bool> {
        let got = if wait {
            self.done.recv().ok()
        } else {
            self.done.try_recv().ok()
        };
        let Some((n, frame, mut buf)) = got else {
            return Ok(false);
        };
        let frame = frame.map_err(|e| Error::Encoding(format!("FLAC encoding failed: {e}")))?;
        buf.resize(BLOCK_SIZE);
        self.free.push(buf);
        self.pending.insert(n, frame);
        while let Some(frame) = self.pending.remove(&self.written) {
            self.info.update_frame_info(&frame);
            self.sink.clear();
            frame
                .write(&mut self.sink)
                .map_err(|e| Error::Encoding(format!("Failed to write FLAC frame: {e:?}")))?;
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
        while self.written < self.sent {
            if !self.receive(out, true)? {
                return Err(Error::Encoding("a FLAC worker stopped".into()));
            }
        }
        // A fixed-blocksize stream's STREAMINFO states the block size every
        // block but the last has, whatever the last one's; an empty stream
        // states it too, rather than `update_frame_info`'s unset 65535 and 0.
        self.info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|e| Error::Encoding(format!("FLAC block size: {e:?}")))?;
        if self.written == 0 {
            // Zero is "unknown"; unset, the two would read u32::MAX and 0.
            self.info
                .set_frame_sizes(0, 0)
                .map_err(|e| Error::Encoding(format!("FLAC frame size: {e:?}")))?;
        }
        // `update_frame_info` has summed every block's length, the last at its
        // own, so the sample count is already exact.
        self.info.set_md5_digest(&self.md5.finalize().into());
        Ok(self.info)
    }
}

/// flacenc's coding options for a level 0 to 8, modelled loosely on
/// libFLAC's presets. Levels 0 to 2 code with fixed predictors only; 3 and
/// up add LPC at an exact order (flacenc's `lpc_order` is the order used,
/// not a searched maximum): 6, then 8 from level 4, 12 from level 7. Every
/// level but 0 tries mid/side and left/right-side stereo.
///
/// Where this departs from libFLAC: its level 3 has no stereo
/// decorrelation, and its levels 0 to 2 use 1152-sample blocks, while the
/// block size here is [`BLOCK_SIZE`] at every level. Only five configs are
/// distinct: levels 1 and 2, 4 to 6, and 7 and 8 encode identically. Above
/// 8 reads as 8. Every level is lossless.
fn encoder_config(level: u8) -> EncoderConfig {
    let level = level.min(8);
    let mut config = EncoderConfig::default();
    let side = level >= 1;
    config.stereo_coding.use_leftside = side;
    config.stereo_coding.use_rightside = side;
    config.stereo_coding.use_midside = side;
    config.subframe_coding.use_lpc = level >= 3;
    config.subframe_coding.qlpc.lpc_order = match level {
        0..=3 => 6,
        4..=6 => 8,
        _ => 12,
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
fn short_block_config(level: u8) -> EncoderConfig {
    let mut config = encoder_config(level);
    config.subframe_coding.use_fixed = false;
    config.subframe_coding.use_lpc = false;
    config
}

fn bits_for(bit_depth: BitDepth) -> usize {
    match bit_depth {
        BitDepth::Int16 => 16,
        BitDepth::Int24 => 24,
        // Rejected in `create`.
        BitDepth::Float32 => 24,
    }
}

/// Quantize to the integer sample flacenc takes, via the engine's canonical
/// converters.
///
/// **Delegation is the whole point.** A local `(clamped * 32767.0) as i32`
/// **truncates** where [`tutti_core::pcm`] rounds: the two agree on 0.0 and ±1.0
/// and diverge by one LSB everywhere else, so 0.7 encodes as 22936 in a FLAC and
/// 22937 in a WAV from the same render. `pcm` exists so a recorded and an
/// exported file quantize a given sample identically, and its docs call bare
/// truncation biased toward zero — a consistent negative DC error on the
/// negative half.
#[inline]
fn f32_to_i32(sample: f32, bit_depth: BitDepth) -> i32 {
    match bit_depth {
        BitDepth::Int16 => tutti_core::pcm::f32_to_i16(sample) as i32,
        // Float32 is rejected in `create`, so this is the Int24 path.
        _ => tutti_core::pcm::f32_to_i24(sample),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_out_of_range_input() {
        assert_eq!(f32_to_i32(2.0, BitDepth::Int16), 32767);
        assert_eq!(f32_to_i32(-2.0, BitDepth::Int16), -32767);
    }

    /// FLAC must quantize exactly as the rest of the engine does.
    ///
    /// The two tests above pass under truncation *and* under rounding, because
    /// 0.0 and ±1.0 land on exact integers either way — which is how a private
    /// truncating converter survives unnoticed. These levels have a fractional
    /// part above 0.5, so the two disagree: 0.7 × 32767 = 22936.9, which
    /// truncates to 22936 and rounds to 22937.
    #[test]
    fn quantization_matches_the_engines_canonical_converter() {
        for level in [0.7f32, -0.7, 0.3, -0.3, 0.123_45, 0.999] {
            assert_eq!(
                f32_to_i32(level, BitDepth::Int16),
                tutti_core::pcm::f32_to_i16(level) as i32,
                "Int16: FLAC and tutti_core::pcm disagree on {level}"
            );
            assert_eq!(
                f32_to_i32(level, BitDepth::Int24),
                tutti_core::pcm::f32_to_i24(level),
                "Int24: FLAC and tutti_core::pcm disagree on {level}"
            );
        }
    }

    fn configs(panic_at: Option<usize>) -> Configs {
        Configs {
            full: encoder_config(5).into_verified().unwrap(),
            short: short_block_config(5).into_verified().unwrap(),
            panic_at,
        }
    }

    /// A worker that panics ends the export with an error naming the panic,
    /// within bounded time, instead of leaving the producer waiting on a
    /// frame that never comes. 200 blocks keep the queue full, so the
    /// producer is blocked on `done` when the panic lands.
    ///
    /// Mutation (run): drop the `catch_unwind` (encode directly, the hook's
    /// `panic!` unwinding the worker) → the export never returns and the
    /// 30 s guard below fails the test.
    #[test]
    #[cfg_attr(miri, ignore = "spawns encoder threads and waits on a clock")]
    fn a_panicking_worker_ends_the_export_with_an_error() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let configs = configs(Some(1));
            let info = StreamInfo::new(48_000, 2, 16).unwrap();
            let pcm = vec![0i32; BLOCK_SIZE * 2];
            let mut out = Vec::new();
            let result = std::thread::scope(|scope| {
                let mut writer = FrameWriter::start(scope, 4, &configs, info, 2, 16);
                let pumped = (0..200).try_for_each(|_| writer.block(&pcm, &mut out));
                let finished = writer.finish(&mut out);
                pumped.and(finished)
            });
            let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the export hung on a panicked worker");
        let err = result.expect_err("a panicked frame cannot be written");
        assert!(err.contains("injected at frame 1"), "{err}");
    }

    /// An empty export states valid block sizes (the stream's, 4 096 both)
    /// and "unknown" frame sizes, not flacenc's unset 65 535 / 0 and
    /// `u32::MAX` / 0.
    ///
    /// Mutation (run): set the block sizes only when a frame was written
    /// (`if self.written > 0`) → min 65 535 → fails.
    #[test]
    #[cfg_attr(miri, ignore = "spawns encoder threads")]
    fn an_empty_stream_states_valid_sizes() {
        let configs = configs(None);
        let info = StreamInfo::new(48_000, 2, 16).unwrap();
        let mut out = Vec::new();
        let info = std::thread::scope(|scope| {
            FrameWriter::start(scope, 2, &configs, info, 2, 16).finish(&mut out)
        })
        .unwrap();
        assert!(out.is_empty(), "no frames");
        assert_eq!(
            (info.min_block_size(), info.max_block_size()),
            (BLOCK_SIZE, BLOCK_SIZE)
        );
        assert_eq!((info.min_frame_size(), info.max_frame_size()), (0, 0));
        assert_eq!(info.total_samples(), 0);
        assert_eq!(header_bytes(&info).unwrap().len(), 42);
    }
}
