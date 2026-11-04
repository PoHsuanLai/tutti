//! WAV (hound). Streams; hound back-patches the RIFF sizes on `finalize`, and the
//! speaker mask of a multichannel file is patched in after it (`tutti_core::wav`).
//!
//! A WAV file's sizes are 32-bit: an export that would pass them fails with
//! [`Error::WavTooLarge`] before the render starts when the plan already shows it,
//! and otherwise at the frame that crosses the limit.
//!
//! The offline twin of `tutti_io::WavOut`, and the two stay separate because
//! this one **pulls** (it drives the render to a planned frame count) while a
//! live sink is **pushed** blocks by whoever owns the loop. They share the
//! quantization — both dispatch through
//! [`BitDepth::quantize`](tutti_core::pcm::BitDepth::quantize) — so a bounced
//! and a recorded WAV agree sample-for-sample at a given depth. The one
//! deliberate difference: an export dithers first, a live capture does not.

use crate::config::ExportConfig;
use crate::encode::{pump_blocks, Encoder};
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use tutti_core::pcm::Sample;
use tutti_core::wav;
use tutti_core::ChannelLayout;

pub(crate) struct WavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
    path: PathBuf,
    layout: ChannelLayout,
    /// The frames the file may hold: [`wav::max_data_frames`], lowered by tests.
    max_frames: u64,
}

impl WavEncoder {
    pub(crate) fn create(path: &Path, config: &ExportConfig) -> Result<Self> {
        let (bits_per_sample, sample_format) = match config.encode.bit_depth {
            BitDepth::Int16 => (16, SampleFormat::Int),
            BitDepth::Int24 => (24, SampleFormat::Int),
            BitDepth::Float32 => (32, SampleFormat::Float),
        };
        let writer = WavWriter::create(
            path,
            WavSpec {
                channels: config.encode.channels.count(),
                sample_rate: crate::encode::encoder_rate(config),
                bits_per_sample,
                sample_format,
            },
        )
        .map_err(io_err)?;
        Ok(Self {
            writer,
            bit_depth: config.encode.bit_depth,
            path: path.to_path_buf(),
            layout: config.encode.channels,
            max_frames: wav::max_data_frames(config.encode.channels, config.encode.bit_depth),
        })
    }

    /// Frames the encoder will write for `plan` at the output rate, rounded
    /// up: the plan's kept frames, resampled when the rates differ.
    fn planned_frames(
        plan: &RenderPlan,
        source_rate: tutti_core::SampleRate,
        config: &ExportConfig,
    ) -> u64 {
        let kept = u128::from(plan.output_length.get() as u64);
        let (out, src) = (
            f64::from(crate::encode::encoder_rate(config)),
            source_rate.get().round(),
        );
        if src <= 0.0 || out == src {
            return kept as u64;
        }
        (kept as f64 * out / src).ceil() as u64
    }
}

/// Counts the frames written against the file's limit.
struct FrameBudget {
    written: u64,
    max: u64,
}

impl FrameBudget {
    /// Takes `frames` more, or fails with [`Error::WavTooLarge`] if the file
    /// would then hold more than `max`.
    fn admit(&mut self, frames: u64) -> Result<()> {
        self.written = self.written.saturating_add(frames);
        if self.written > self.max {
            return Err(Error::WavTooLarge {
                limit_frames: self.max,
            });
        }
        Ok(())
    }
}

impl Encoder for WavEncoder {
    fn encode(
        mut self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        // Refuse before rendering when the plan alone shows the file cannot
        // fit; the loop below still counts, since a resampler's output length
        // is only known to within a frame or two.
        if Self::planned_frames(plan, source_rate, config) > self.max_frames {
            return Err(Error::WavTooLarge {
                limit_frames: self.max_frames,
            });
        }
        let mut budget = FrameBudget {
            written: 0,
            max: self.max_frames,
        };
        let width = u64::from(self.layout.count().max(1));
        pump_blocks(src, source_rate, plan, config, |frames| {
            budget.admit(frames.samples().len() as u64 / width)?;
            // Already interleaved — hound wants a flat sample stream, which is
            // exactly what the render hands over. No de-interleave step to undo.
            for &s in frames.samples() {
                // The depth dispatch is `tutti-types`', shared with the live
                // `WavOut` sink; only the writer call per variant is local. An
                // offline render and a live capture therefore quantize a given
                // sample identically by construction, not by two hand-written
                // matches happening to agree.
                match self.bit_depth.quantize(s) {
                    Sample::I16(v) => self.writer.write_sample(v),
                    Sample::I24(v) => self.writer.write_sample(v),
                    Sample::F32(v) => self.writer.write_sample(v),
                }
                .map_err(io_err)?;
            }
            Ok(())
        })?;
        self.writer.finalize().map_err(io_err)?;
        // `hound` writes the lowest-N-bits speaker mask and has no way to change
        // it; the one for the engine's channel order goes in over the header.
        wav::patch_channel_mask_at(&self.path, self.layout)?;
        Ok(())
    }
}

fn io_err(e: hound::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EncodeConfig, RenderConfig};
    use crate::options::AudioFormat;
    use crate::render::PlaneSource;
    use tutti_core::SampleRate;
    use tutti_types::Samples;

    fn config(channels: u16, depth: BitDepth) -> ExportConfig {
        ExportConfig {
            render: RenderConfig {
                sample_rate: SampleRate::from(1_000.0),
                duration_seconds: 0.008,
                ..Default::default()
            },
            encode: EncodeConfig {
                format: AudioFormat::Wav,
                bit_depth: depth,
                channels: ChannelLayout::from(channels),
            },
            ..Default::default()
        }
    }

    fn encode(
        path: &Path,
        config: &ExportConfig,
        max_frames: u64,
        planes: &[Vec<f32>],
    ) -> Result<()> {
        let plan = RenderPlan::new(&config.render);
        assert_eq!(plan.output_length, Samples(8));
        let mut enc = WavEncoder::create(path, config)?;
        enc.max_frames = max_frames;
        enc.encode(
            &mut PlaneSource::new(planes),
            config.render.sample_rate,
            &plan,
            config,
        )
    }

    /// An export at exactly the limit succeeds, and its 6-channel header
    /// carries the 5.1 (side) mask rather than `hound`'s `0x3F`.
    ///
    /// Mutation: dropping the `patch_channel_mask_at` call in `encode` leaves
    /// `0x3F` and fails the mask assertion; `admit` using `>=` fails the
    /// limit-exact case.
    #[test]
    fn an_export_at_the_limit_succeeds_and_declares_its_mask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("six.wav");
        let planes = vec![vec![0.5f32; 8]; 6];
        encode(&path, &config(6, BitDepth::Int24), 8, &planes).expect("8 frames fit a limit of 8");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0x60F);
        assert_eq!(hound::WavReader::open(&path).unwrap().len(), 8 * 6);
    }

    /// A plan that cannot fit is refused before a frame is pulled: the source
    /// is never asked for one.
    ///
    /// Mutation: deleting the `planned_frames` check lets the loop pull the
    /// first block before `admit` fails, and the source's fill count is then non-zero.
    #[test]
    fn a_plan_that_cannot_fit_is_refused_before_rendering() {
        struct Counting(usize);
        impl FrameSource for Counting {
            fn fill(&mut self, out: tutti_types::InterleavedMut<'_>) -> usize {
                self.0 += 1;
                out.len()
            }
            fn produced(&self) -> Samples {
                Samples(0)
            }
            fn max_block(&self) -> usize {
                4
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(2, BitDepth::Int16);
        let plan = RenderPlan::new(&config.render);
        let mut enc = WavEncoder::create(&dir.path().join("big.wav"), &config).unwrap();
        enc.max_frames = 7;
        let mut src = Counting(0);
        let err = enc
            .encode(&mut src, config.render.sample_rate, &plan, &config)
            .expect_err("8 frames do not fit a limit of 7");
        assert!(
            matches!(err, Error::WavTooLarge { limit_frames: 7 }),
            "{err}"
        );
        assert_eq!(src.0, 0, "nothing was rendered");
    }

    /// The running count fails on the frame that crosses the limit, not before.
    ///
    /// Mutation: `>` to `>=` in `admit` fails the second assertion; removing
    /// the `saturating_add` for `+` would panic on overflow in debug.
    #[test]
    fn the_running_count_fails_on_the_crossing_frame() {
        let mut budget = FrameBudget { written: 0, max: 5 };
        budget.admit(3).unwrap();
        budget.admit(2).expect("exactly the limit");
        let err = budget.admit(1).expect_err("one past");
        assert!(matches!(err, Error::WavTooLarge { limit_frames: 5 }));
        assert!(FrameBudget { written: 0, max: 5 }.admit(u64::MAX).is_err());
    }
}
