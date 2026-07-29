//! [`WavOut`] — the live WAV implementation of [`AudioOut`].
//!
//! An [`AudioOut`] is "push frames → destination"; this is that
//! destination for a WAV file. It writes any
//! [`BitDepth`], INCREMENTALLY — a recording is
//! minutes long and never held resident.
//!
//! # Why this is not tutti-export's encoder
//!
//! tutti-export has its own hound-backed WAV writer, and the two stay separate
//! for a structural reason: **this one is pushed, that one pulls.** An
//! `AudioOut` is fed blocks by whoever owns the loop; an export `Encoder` *is*
//! the loop (it takes the source, so flacenc can be handed it directly) and is
//! driven by a `RenderPlan` carrying a total frame count. A live capture has no
//! total — it ends when someone stops it — so it cannot be expressed as an
//! export encode.
//!
//! They do share the quantization: this sink and tutti-export's WAV encoder
//! both dispatch through
//! [`BitDepth::quantize`](tutti_core::pcm::BitDepth::quantize), so a recorded
//! and an exported WAV agree sample-for-sample at a given depth by
//! construction. (AIFF and FLAC quantize their own way; they call the same
//! `pcm` primitives, so they agree in fact — just not structurally.)
//!
//! One difference is deliberate: export dithers before quantizing and a live
//! capture does not, because noise shaping is a decision a capture path should
//! not make silently.
//!
//! The live driver is [`Recorder`](crate::Recorder), which pumps any live source
//! ([`AudioIn`](tutti_core::io::AudioIn)) into this sink ([`AudioOut`])
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use tutti_core::io::AudioOut;
use tutti_core::pcm::{BitDepth, Sample};
use tutti_core::ChannelLayout;

/// Live WAV [`AudioOut`]. Owns the `hound` writer plus the channel count and
/// depth needed to encode each frame.
pub struct WavOut {
    writer: WavWriter<BufWriter<File>>,
    layout: ChannelLayout,
    depth: BitDepth,
    /// The rate written into the header, kept so a caller can check it against
    /// the source it is about to pump in. Nothing here can validate that pairing
    /// — `AudioIn` deliberately carries no rate — so the best this type can do
    /// is report what it promised the file.
    sample_rate: f64,
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
    /// Create the file and WAV header for `file_path`. Returns `None` if the
    /// file can't be created or the header can't be written.
    pub fn create(
        file_path: &PathBuf,
        sample_rate: f64,
        channels: usize,
        depth: BitDepth,
    ) -> Option<Self> {
        let sample_format = if depth.is_integer() {
            SampleFormat::Int
        } else {
            SampleFormat::Float
        };
        let layout = ChannelLayout::from(channels);
        let spec = WavSpec {
            channels: layout.count(),
            sample_rate: sample_rate as u32,
            bits_per_sample: depth.bits(),
            sample_format,
        };

        let file = File::create(file_path).ok()?;
        let buf_writer = BufWriter::new(file);
        let writer = WavWriter::new(buf_writer, spec).ok()?;
        Some(Self {
            writer,
            layout,
            depth,
            sample_rate,
            first_error: None,
        })
    }

    /// Declared channel count — what the WAV header says, and therefore exactly
    /// how many samples per frame [`write_interleaved`](Self::write_interleaved)
    /// must emit.
    pub fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Sample rate written into the header.
    ///
    /// Exposed so a caller pairing this sink with a source can compare the two:
    /// feeding 48 kHz frames into a sink that declared 8 kHz produces a
    /// perfectly valid WAV that plays back six times too slow, and nothing
    /// downstream can detect it. Only the caller holds both halves.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Write flat interleaved frames at this sink's own declared width.
    ///
    /// Emits exactly `channels()` samples per frame — no more, no fewer. A short
    /// trailing frame is ignored; a frame wider than the header is truncated to
    /// it.
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
        let ch = self.channels().max(1);
        for frame in samples.chunks_exact(ch) {
            for &s in frame {
                // The depth dispatch is `tutti-types`'; only the writer call is
                // ours, so this sink cannot drift from the export encoders'
                // quantization.
                let written = match self.depth.quantize(s) {
                    Sample::I16(v) => self.writer.write_sample(v),
                    Sample::I24(v) => self.writer.write_sample(v),
                    Sample::F32(v) => self.writer.write_sample(v),
                };
                if let Err(e) = written {
                    self.first_error = Some(std::io::Error::other(e));
                    return;
                }
            }
        }
    }
}

impl AudioOut for WavOut {
    /// Stereo shim over [`write_interleaved`](WavOut::write_interleaved).
    ///
    /// A mono sink drops the right channel; a sink wider than stereo zero-fills
    /// the channels this stereo-framed input cannot supply, so the data still
    /// matches the declared header width.
    fn write(&mut self, frames: &[[f32; 2]]) {
        let ch = self.channels().max(1);
        match ch {
            1 => {
                for &[left, _] in frames {
                    self.write_interleaved(&[left]);
                }
            }
            2 => self.write_interleaved(frames.as_flattened()),
            _ => {
                let mut frame = vec![0.0f32; ch];
                for &[left, right] in frames {
                    frame[0] = left;
                    frame[1] = right;
                    self.write_interleaved(&frame);
                }
            }
        }
    }

    fn finalize(self) -> std::io::Result<()> {
        // A write that failed mid-stream is reported here, ahead of the
        // finalize itself: the header may well back-patch cleanly over a
        // truncated recording, so finalizing `Ok` would hide the real fault. The
        // caller learns the take is short, which is the whole point of keeping
        // it.
        if let Some(error) = self.first_error {
            // Still finalize, so the bytes that did land are readable.
            let _ = self.writer.finalize();
            return Err(error);
        }
        // `hound::Error` -> io error: finalize flushes the sample buffer and
        // back-patches the RIFF/data chunk sizes. On failure the file is left
        // with a stale header and is unreadable — a real integrity loss.
        self.writer
            .finalize()
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink writes INCREMENTALLY: feeding frames across many `write` calls
    /// and finalizing must yield a valid WAV whose frame count is the sum of
    /// every block — the sink never has to see the whole recording at once.
    #[test]
    fn wav_out_writes_incrementally_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");

        let mut sink =
            WavOut::create(&path, 48_000.0, 2, BitDepth::Float32).expect("sink should open");

        let block: Vec<[f32; 2]> = (0..256)
            .map(|i| [i as f32 / 256.0, -(i as f32) / 256.0])
            .collect();
        let blocks = 5;
        for _ in 0..blocks {
            sink.write(&block);
        }
        sink.finalize()
            .expect("finalize should back-patch the header");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(reader.len() as usize, block.len() * blocks * 2);
    }

    /// Mono capture writes one sample per frame (the right channel is dropped).
    #[test]
    fn wav_out_mono_writes_one_sample_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");

        let mut sink =
            WavOut::create(&path, 44_100.0, 1, BitDepth::Float32).expect("sink should open");
        let frames = vec![[0.5f32, 0.9f32]; 128];
        sink.write(&frames);
        sink.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len() as usize, frames.len());
    }

    /// A 6-channel sink must write SIX samples per frame, matching the header it
    /// declared. If the header takes the layout width while `write` emits two,
    /// a reader interleave-misaligns and the channels rotate by `2 mod 6` every
    /// frame.
    #[test]
    fn six_channel_sink_writes_six_samples_per_frame() {
        let dir = std::env::temp_dir();
        let path = dir.join("tutti_wav_out_six_channel.wav");
        let _ = std::fs::remove_file(&path);

        let mut sink =
            WavOut::create(&path, 48_000.0, 6, BitDepth::Float32).expect("create 6ch sink");
        assert_eq!(sink.channels(), 6);

        let frames: Vec<f32> = (0..128)
            .flat_map(|i| (0..6).map(move |c| (i * 6 + c) as f32 * 0.001))
            .collect();
        sink.write_interleaved(&frames);
        AudioOut::finalize(sink).expect("finalize");

        let reader = hound::WavReader::open(&path).expect("reopen");
        assert_eq!(reader.spec().channels, 6, "header must declare 6 channels");
        assert_eq!(
            reader.len() as usize,
            128 * 6,
            "128 frames of 6 channels must write 768 samples, not 256"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The stereo shim must still produce a well-formed file at a wider declared
    /// width: it zero-fills the channels it cannot supply rather than emitting
    /// short frames.
    #[test]
    fn stereo_write_into_a_wide_sink_stays_frame_aligned() {
        let dir = std::env::temp_dir();
        let path = dir.join("tutti_wav_out_stereo_into_quad.wav");
        let _ = std::fs::remove_file(&path);

        let mut sink =
            WavOut::create(&path, 48_000.0, 4, BitDepth::Float32).expect("create 4ch sink");
        let frames = [[0.25f32, -0.25]; 16];
        AudioOut::write(&mut sink, &frames);
        AudioOut::finalize(sink).expect("finalize");

        let mut reader = hound::WavReader::open(&path).expect("reopen");
        assert_eq!(reader.spec().channels, 4);
        assert_eq!(
            reader.len() as usize,
            16 * 4,
            "must be a whole number of frames"
        );
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert!((samples[0] - 0.25).abs() < 1e-6);
        assert!((samples[1] + 0.25).abs() < 1e-6);
        assert_eq!(samples[2], 0.0, "unsupplied channels are silent");
        assert_eq!(samples[3], 0.0);
        let _ = std::fs::remove_file(&path);
    }

    /// A failed write is reported from `finalize`, not swallowed.
    ///
    /// This is what the `first_error` field exists for: `AudioOut::write`
    /// returns `()`, so a fault mid-recording has nowhere else to surface, and
    /// a short file with an `Ok` finalize reads as a complete take.
    ///
    /// The fault is provoked deterministically rather than simulated: an 8-bit
    /// header accepts creation, but `hound` rejects any `i16` too wide for it
    /// with `TooWide` on the *write*. Quantizing at `Int16` into that header
    /// therefore fails at the first loud sample — a real mid-stream write
    /// failure, which is exactly the shape a disk-full has.
    #[test]
    fn a_failed_write_surfaces_from_finalize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.wav");

        let mut sink = WavOut {
            writer: {
                let spec = WavSpec {
                    channels: 2,
                    sample_rate: 48_000,
                    bits_per_sample: 8,
                    sample_format: SampleFormat::Int,
                };
                let file = File::create(&path).expect("create");
                WavWriter::new(BufWriter::new(file), spec).expect("header")
            },
            layout: ChannelLayout::from(2usize),
            depth: BitDepth::Int16,
            sample_rate: 48_000.0,
            first_error: None,
        };

        // Full scale at Int16 is 32767 — far too wide for the 8-bit stream.
        sink.write(&[[1.0, -1.0], [1.0, -1.0]]);

        let err = sink
            .finalize()
            .expect_err("a failed write must be reported, not swallowed");
        // The underlying `hound::Error` is carried, not flattened to a bare
        // "write failed" — a caller can tell what went wrong.
        assert!(
            err.get_ref()
                .is_some_and(|e| e.downcast_ref::<hound::Error>().is_some()),
            "the hound failure must ride out intact, got: {err}"
        );
    }

    /// 16-bit capture, which the old `CaptureFormat` (F32 / I24 only) could not
    /// express at all. Round-trips through the file, so the header and the data
    /// have to agree.
    #[test]
    fn sixteen_bit_capture_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i16.wav");

        let mut sink =
            WavOut::create(&path, 44_100.0, 2, BitDepth::Int16).expect("sink should open");
        sink.write(&[[1.0, -1.0], [0.0, 0.0]]);
        sink.finalize().expect("finalize");

        let mut reader = hound::WavReader::open(&path).expect("readable");
        assert_eq!(reader.spec().bits_per_sample, 16);
        assert_eq!(reader.spec().sample_format, SampleFormat::Int);

        let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        // Full scale quantizes through the shared `f32_to_i16`, not a local copy.
        assert_eq!(samples, vec![32767, -32767, 0, 0]);
    }

    /// After a failure the sink stops writing and the bytes that did land stay
    /// readable — both halves of what `finalize`'s error path promises.
    ///
    /// The short-circuit at the top of `write_interleaved` is otherwise
    /// untested: deleting it would leave every other test green while the sink
    /// kept hammering a writer it already knows is broken.
    #[test]
    fn a_failed_sink_stops_writing_and_leaves_a_readable_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.wav");

        // 8-bit header, Int16 depth: in-range samples write, loud ones fail.
        let mut sink = WavOut {
            writer: {
                let spec = WavSpec {
                    channels: 2,
                    sample_rate: 48_000,
                    bits_per_sample: 8,
                    sample_format: SampleFormat::Int,
                };
                let file = File::create(&path).expect("create");
                WavWriter::new(BufWriter::new(file), spec).expect("header")
            },
            layout: ChannelLayout::from(2usize),
            depth: BitDepth::Int16,
            sample_rate: 48_000.0,
            first_error: None,
        };

        // Quiet enough to fit in 8 bits: these land.
        sink.write(&[[0.001, -0.001], [0.001, -0.001]]);
        // Full scale at Int16 is 32767 — too wide, so this fails.
        sink.write(&[[1.0, -1.0]]);
        assert!(
            sink.first_error.is_some(),
            "the loud frame must have failed"
        );

        // Everything after the failure is ignored rather than retried.
        sink.write(&[[0.001, -0.001], [0.001, -0.001], [0.001, -0.001]]);

        let err = sink.finalize().expect_err("the failure must be reported");
        assert!(err.get_ref().is_some(), "carrying the hound error");

        // The prefix survived: finalize still back-patched the header, so the
        // frames written before the fault are recoverable rather than lost.
        let reader =
            hound::WavReader::open(&path).expect("a partially-written take must still open");
        assert_eq!(
            reader.len(),
            4,
            "exactly the two good frames (2 samples each), and nothing written after the failure"
        );
    }

    /// `Int24` — the default depth — round-trips through a real file.
    ///
    /// Int16 and Float32 were covered; the default was not, which is the one a
    /// caller gets by writing `BitDepth::default()`.
    #[test]
    fn twenty_four_bit_capture_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i24.wav");

        let mut sink =
            WavOut::create(&path, 48_000.0, 2, BitDepth::Int24).expect("sink should open");
        sink.write(&[[1.0, -1.0], [0.0, 0.0]]);
        sink.finalize().expect("finalize");

        let mut reader = hound::WavReader::open(&path).expect("readable");
        assert_eq!(reader.spec().bits_per_sample, 24);
        assert_eq!(reader.spec().sample_format, SampleFormat::Int);

        let samples: Vec<i32> = reader.samples::<i32>().map(|s| s.unwrap()).collect();
        // Full scale through the shared `f32_to_i24`, not a local copy.
        assert_eq!(samples, vec![8_388_607, -8_388_607, 0, 0]);
    }
}
