//! [`WavOut`] — the live WAV implementation of [`AudioOut`].
//!
//! An [`AudioOut`] is "push frames → destination"; this is that
//! destination for a WAV file. It writes any
//! [`BitDepth`], INCREMENTALLY — a recording is
//! minutes long and never held resident.
//!
//! # Why this is not tutti-export's encoder
//!
//! `tutti-export` is the OFFLINE edge and has its own hound-backed WAV writer.
//! The two stay separate for a structural reason: **this one is pushed, that one
//! pulls.** An `AudioOut` is fed blocks by whoever owns the loop; an export
//! `Encoder` *is* the loop (it takes the source, so flacenc can be handed it
//! directly) and is driven by a `RenderPlan` carrying a total frame count. A
//! live capture has no total — it ends when someone stops it — so it cannot be
//! expressed as an export encode.
//!
//! They do share the quantization: this sink and tutti-export's WAV encoder both
//! dispatch through [`BitDepth::quantize`](tutti_core::pcm::BitDepth::quantize),
//! so a recorded and an exported WAV agree sample-for-sample at a given depth by
//! construction. AIFF and FLAC quantize their own way over the same `pcm`
//! primitives, so they agree in fact — just not structurally.
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
use std::path::Path;
use tutti_core::io::AudioOut;
use tutti_core::pcm::{BitDepth, Sample};
use tutti_core::ChannelLayout;
use tutti_core::SampleRate;

/// Widest frame [`WavOut::write_folding`] folds *into* without allocating.
///
/// Aliases `tutti_core::MAX_ROOT_CHANNELS` (8, mono through 7.1) so the
/// fold ceiling on the capture edge matches the render root's. A sink declared
/// wider still writes whole frames — the channels past this are silence — so
/// this bounds fidelity, never alignment.
pub const MAX_WAV_FOLD_CHANNELS: usize = tutti_core::MAX_ROOT_CHANNELS;

/// Live WAV [`AudioOut`]. Owns the `hound` writer plus the channel layout and
/// depth needed to encode each frame.
pub struct WavOut {
    writer: WavWriter<BufWriter<File>>,
    layout: ChannelLayout,
    depth: BitDepth,
    /// The rate written into the header, kept so a caller can check it against
    /// the source it is about to pump in. Nothing here can validate that pairing
    /// — `AudioIn` deliberately carries no rate — so the best this type can do
    /// is report what it promised the file.
    sample_rate: SampleRate,
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
    /// Create the file and WAV header for `file_path`.
    ///
    /// # Errors
    ///
    /// The `io::Error` from creating the file (a missing directory, a
    /// permission denial, a full disk) or from writing the header. Carried
    /// rather than flattened to a sentinel, because those cases want different
    /// responses from a caller and only the error distinguishes them —
    /// [`Recorder::start`](crate::Recorder::start), which consumes this type,
    /// already returns `io::Result` for the same reason.
    pub fn create(
        file_path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
        channels: impl Into<ChannelLayout>,
        depth: BitDepth,
    ) -> std::io::Result<Self> {
        let sample_rate = sample_rate.into();
        let sample_format = if depth.is_integer() {
            SampleFormat::Int
        } else {
            SampleFormat::Float
        };
        let layout: ChannelLayout = channels.into();
        let spec = WavSpec {
            channels: layout.count(),
            // The WAV header field is an integer rate: types stop here.
            sample_rate: sample_rate.get().round() as u32,
            bits_per_sample: depth.bits(),
            sample_format,
        };

        let file = File::create(file_path)?;
        let buf_writer = BufWriter::new(file);
        // Same `hound::Error` -> io error carry as `finalize`: the message is
        // the only part a caller can act on, and dropping it here would be the
        // information loss this signature exists to stop.
        let writer =
            WavWriter::new(buf_writer, spec).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            writer,
            layout,
            depth,
            sample_rate,
            first_error: None,
        })
    }

    /// Declared channel layout — what the WAV header says, and therefore
    /// exactly how many samples per frame
    /// [`write_interleaved`](Self::write_interleaved) must emit.
    ///
    /// This is also [`AudioOut::layout`]; the inherent copy exists so a caller
    /// holding a concrete `WavOut` can ask without importing the trait.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride as a plain `usize`, clamped to at least 1.
    ///
    /// Kept as a separate accessor rather than making every call site write
    /// `layout().count().max(1) as usize`, because that expression is the one
    /// piece of arithmetic that must be derived ONCE per call and hoisted above
    /// any per-frame loop. Naming it is what makes a stray `.count()` inside a
    /// loop stand out as the review failure it is.
    fn stride(&self) -> usize {
        self.layout.count().max(1) as usize
    }

    /// Sample rate written into the header.
    ///
    /// Exposed so a caller pairing this sink with a source can compare the two:
    /// feeding 48 kHz frames into a sink that declared 8 kHz produces a
    /// perfectly valid WAV that plays back six times too slow, and nothing
    /// downstream can detect it. Only the caller holds both halves.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Write flat interleaved frames at this sink's own declared width.
    ///
    /// `samples` is a flat interleaved buffer holding `frames *
    /// layout().count()` samples. Emits exactly `layout().count()` samples per
    /// frame — no more, no fewer. A short trailing frame is ignored; a frame
    /// wider than the header is truncated to it.
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
        // Derived ONCE, above the loop. See `stride`.
        let ch = self.stride();
        for frame in samples.chunks_exact(ch) {
            for &s in frame {
                if !self.emit(s) {
                    return;
                }
            }
        }
    }

    /// Quantize and write one sample, returning `false` once the sink has
    /// failed. The single place a sample reaches `hound`, so every write path
    /// shares one quantization and one error-latching rule.
    ///
    /// The depth dispatch is `tutti-types`'; only the writer call is ours, so
    /// this sink cannot drift from the export encoders' quantization.
    fn emit(&mut self, s: f32) -> bool {
        if self.first_error.is_some() {
            return false;
        }
        let written = match self.depth.quantize(s) {
            Sample::I16(v) => self.writer.write_sample(v),
            Sample::I24(v) => self.writer.write_sample(v),
            Sample::F32(v) => self.writer.write_sample(v),
        };
        if let Err(e) = written {
            self.first_error = Some(std::io::Error::other(e));
            return false;
        }
        true
    }

    /// Write flat interleaved frames of some **other** width, folding each one
    /// to this sink's declared width on the way in.
    ///
    /// `src` is a flat interleaved buffer at `src_layout`'s width; the fold runs
    /// once per `src` FRAME and emits one whole destination frame. A trailing
    /// partial `src` frame is ignored.
    ///
    /// The fold is [`fold_frame`](tutti_core::fold_frame), the engine's single
    /// ITU/Dolby implementation, so a mono sink **averages** `(l + r) * 0.5` and
    /// a 5.1 source keeps its centre and surrounds instead of being truncated to
    /// the front pair. The policy lives in exactly one place; this sink has no
    /// private copy of it to get wrong.
    ///
    /// Allocation-free: the destination frame is a fixed stack scratch used as a
    /// prefix, per the engine's RT pattern.
    ///
    /// The scratch caps the width this can *fold into* at
    /// [`MAX_WAV_FOLD_CHANNELS`]. A sink declared wider than that still gets a
    /// full, correctly-aligned frame every time — the channels past the ceiling
    /// are written as silence. Emitting a short frame instead would misalign
    /// the interleave for the whole rest of the file, which is a far worse
    /// failure than a few silent channels on an implausibly wide sink.
    pub fn write_folding(&mut self, src: &[f32], src_layout: ChannelLayout) {
        if self.first_error.is_some() {
            return;
        }
        // Both strides derived ONCE, above the loop.
        let src_ch = src_layout.count().max(1) as usize;
        let dst_ch = self.stride();
        let folded = dst_ch.min(MAX_WAV_FOLD_CHANNELS);

        let mut frame = [0.0f32; MAX_WAV_FOLD_CHANNELS];
        for chunk in src.chunks_exact(src_ch) {
            tutti_core::fold_frame(chunk, &mut frame[..folded]);
            for &s in &frame[..folded] {
                if !self.emit(s) {
                    return;
                }
            }
            // Pad out to the declared width so every frame stays whole.
            for _ in folded..dst_ch {
                if !self.emit(0.0) {
                    return;
                }
            }
        }
    }
}

impl AudioOut for WavOut {
    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Write flat interleaved samples **already at this sink's own width**.
    ///
    /// `frames` holds `frames * layout().count()` samples; the trait speaks a
    /// runtime [`ChannelLayout`] and the sink's layout is the header's, so this
    /// and [`write_interleaved`](WavOut::write_interleaved) are the same
    /// operation. A caller feeding some *other* width folds first — see
    /// [`write_folding`](WavOut::write_folding).
    fn write(&mut self, frames: &[f32]) {
        // Straight delegation, deliberately: a private re-fit here is what
        // dropped the right channel at mono and allocated a `vec![0.0; ch]` per
        // frame. Any width adaptation belongs in `write_folding`, over the
        // engine's single `fold_frame`.
        self.write_interleaved(frames);
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

    /// A failed open reports *why*, and takes a path without ceremony.
    ///
    /// Both halves of the signature change in one assertion. The kind matters:
    /// a caller distinguishing "make the directory and retry" from "give up"
    /// can only do so from the error, and the `Option` this used to return
    /// collapsed every cause into `None`.
    #[test]
    fn a_failed_open_reports_the_cause() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir").join("take.wav");

        let err = WavOut::create(&missing, 48_000.0, 2u16, BitDepth::Float32)
            .expect_err("a file under a missing directory cannot be created");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "the cause must survive to the caller, not flatten to a sentinel"
        );
    }

    /// The sink writes INCREMENTALLY: feeding frames across many `write` calls
    /// and finalizing must yield a valid WAV whose frame count is the sum of
    /// every block — the sink never has to see the whole recording at once.
    #[test]
    fn wav_out_writes_incrementally_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");

        let mut sink =
            WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink should open");

        // Flat interleaved stereo: 256 frames, 512 samples.
        let block: Vec<f32> = (0..256)
            .flat_map(|i| [i as f32 / 256.0, -(i as f32) / 256.0])
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
        assert_eq!(reader.len() as usize, block.len() * blocks);
    }

    /// A stereo source folded into a mono sink must AVERAGE `(l + r) * 0.5`,
    /// not drop the right channel.
    ///
    /// Asymmetric inputs are what make the two distinguishable — with
    /// `[0.5, 0.5]` a dropped channel and an average agree, which is exactly how
    /// a channel-dropping mono fold survives a suite that already covers mono.
    #[test]
    fn folding_to_mono_averages_rather_than_dropping_the_right_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fold_mono.wav");

        let mut sink = WavOut::create(&path, 48_000.0, ChannelLayout::MONO, BitDepth::Float32)
            .expect("sink should open");
        // L=0.5 R=0.9 → average 0.7. Dropping R would write 0.5.
        sink.write_folding(&[0.5, 0.9, 1.0, 0.0], ChannelLayout::STEREO);
        sink.finalize().unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 2, "two stereo frames → two mono samples");
        assert!(
            (samples[0] - 0.7).abs() < 1e-6,
            "expected the AVERAGE 0.7, got {} — the right channel was dropped",
            samples[0]
        );
        assert!(
            (samples[1] - 0.5).abs() < 1e-6,
            "expected 0.5, got {}",
            samples[1]
        );
    }

    /// A 5.1 frame folded to stereo or mono must keep the centre and surrounds,
    /// not truncate to channels 0/1.
    ///
    /// A centre-only frame is the sharpest case: truncation writes pure
    /// silence, so the assertion is "any signal at all reached the file". This
    /// is the same fold the mic callback applies, exercised at the one seam
    /// that needs no hardware.
    #[test]
    fn folding_a_surround_frame_keeps_the_centre_and_surrounds() {
        let dir = tempfile::tempdir().unwrap();

        // 5.1, energy ONLY in the centre (idx 2). FL FR C LFE SL SR.
        let centre_only = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0];

        // → stereo: centre must reach BOTH sides at −3 dB.
        let stereo_path = dir.path().join("surround_to_stereo.wav");
        let mut stereo = WavOut::create(
            &stereo_path,
            48_000.0,
            ChannelLayout::STEREO,
            BitDepth::Float32,
        )
        .expect("sink should open");
        stereo.write_folding(&centre_only, ChannelLayout::from(6u16));
        stereo.finalize().unwrap();

        let mut reader = hound::WavReader::open(&stereo_path).unwrap();
        let s: Vec<f32> = reader.samples::<f32>().map(|x| x.unwrap()).collect();
        assert_eq!(s.len(), 2);
        assert!(
            s[0] > 0.1 && s[1] > 0.1,
            "the centre channel (dialogue) was truncated away: {s:?}"
        );
        assert!(
            (s[0] - s[1]).abs() < 1e-6,
            "a centre source must fold symmetrically"
        );

        // → mono: still non-silent. A rear-only 7.1 frame likewise.
        let mono_path = dir.path().join("surround_to_mono.wav");
        let mut mono = WavOut::create(&mono_path, 48_000.0, ChannelLayout::MONO, BitDepth::Float32)
            .expect("sink should open");
        // 7.1 with energy only in the rears (idx 6, 7) — front-pair truncation
        // would write silence here too.
        let rears_only = [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0];
        mono.write_folding(&rears_only, ChannelLayout::from(8u16));
        mono.finalize().unwrap();

        let mut reader = hound::WavReader::open(&mono_path).unwrap();
        let m: Vec<f32> = reader.samples::<f32>().map(|x| x.unwrap()).collect();
        assert_eq!(m.len(), 1);
        assert!(
            m[0] > 0.1,
            "the surround channels were truncated away: got {}",
            m[0]
        );
    }

    /// `write_folding` never emits a short frame, whatever the widths.
    ///
    /// Alignment is the invariant a fold must not trade away: a frame short by
    /// even one sample rotates every channel for the rest of the file, which
    /// reads as "the recording is subtly wrong" rather than as an error.
    #[test]
    fn folding_always_emits_whole_frames() {
        let dir = tempfile::tempdir().unwrap();
        for (src_w, dst_w) in [(2u16, 6u16), (6, 2), (1, 4), (6, 1), (2, 12)] {
            let path = dir.path().join(format!("align_{src_w}_{dst_w}.wav"));
            let dst = ChannelLayout::from(dst_w);
            let mut sink =
                WavOut::create(&path, 48_000.0, dst, BitDepth::Float32).expect("sink should open");

            const FRAMES: usize = 7; // odd, so a stride slip cannot alias
            let src = vec![0.25f32; FRAMES * src_w as usize];
            sink.write_folding(&src, ChannelLayout::from(src_w));
            sink.finalize().unwrap();

            let reader = hound::WavReader::open(&path).unwrap();
            assert_eq!(reader.spec().channels, dst_w);
            assert_eq!(
                reader.len() as usize,
                FRAMES * dst_w as usize,
                "{src_w}ch → {dst_w}ch must write whole frames"
            );
        }
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
            WavOut::create(&path, 48_000.0, 6u16, BitDepth::Float32).expect("create 6ch sink");
        assert_eq!(sink.layout(), ChannelLayout::from(6u16));

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

    /// A stereo source folded into a wider declared width produces a well-formed
    /// file: it zero-fills the channels it cannot supply rather than emitting
    /// short frames, and never synthesises an upmix.
    #[test]
    fn stereo_write_into_a_wide_sink_stays_frame_aligned() {
        let dir = std::env::temp_dir();
        let path = dir.join("tutti_wav_out_stereo_into_quad.wav");
        let _ = std::fs::remove_file(&path);

        let mut sink =
            WavOut::create(&path, 48_000.0, 4u16, BitDepth::Float32).expect("create 4ch sink");
        let frames: Vec<f32> = std::iter::repeat_n([0.25f32, -0.25], 16)
            .flatten()
            .collect();
        sink.write_folding(&frames, ChannelLayout::STEREO);
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
            sample_rate: SampleRate::SR_48K,
            first_error: None,
        };

        // Full scale at Int16 is 32767 — far too wide for the 8-bit stream.
        sink.write(&[1.0, -1.0, 1.0, -1.0]);

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

    /// 16-bit capture round-trips through a real file, so the header and the
    /// data have to agree.
    #[test]
    fn sixteen_bit_capture_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i16.wav");

        let mut sink =
            WavOut::create(&path, 44_100.0, 2u16, BitDepth::Int16).expect("sink should open");
        sink.write(&[1.0, -1.0, 0.0, 0.0]);
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
            sample_rate: SampleRate::SR_48K,
            first_error: None,
        };

        // Quiet enough to fit in 8 bits: these land.
        sink.write(&[0.001, -0.001, 0.001, -0.001]);
        // Full scale at Int16 is 32767 — too wide, so this fails.
        sink.write(&[1.0, -1.0]);
        assert!(
            sink.first_error.is_some(),
            "the loud frame must have failed"
        );

        // Everything after the failure is ignored rather than retried.
        sink.write(&[0.001, -0.001, 0.001, -0.001, 0.001, -0.001]);

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

    /// `Int24` — the default depth, and so the one a caller gets by writing
    /// `BitDepth::default()` — round-trips through a real file.
    #[test]
    fn twenty_four_bit_capture_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i24.wav");

        let mut sink =
            WavOut::create(&path, 48_000.0, 2u16, BitDepth::Int24).expect("sink should open");
        sink.write(&[1.0, -1.0, 0.0, 0.0]);
        sink.finalize().expect("finalize");

        let mut reader = hound::WavReader::open(&path).expect("readable");
        assert_eq!(reader.spec().bits_per_sample, 24);
        assert_eq!(reader.spec().sample_format, SampleFormat::Int);

        let samples: Vec<i32> = reader.samples::<i32>().map(|s| s.unwrap()).collect();
        // Full scale through the shared `f32_to_i24`, not a local copy.
        assert_eq!(samples, vec![8_388_607, -8_388_607, 0, 0]);
    }
}
