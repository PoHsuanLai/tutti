//! WAV (hound). Streams; hound back-patches the RIFF sizes on `finalize`, and the
//! speaker mask of a multichannel file is patched in after it (`tutti_core::wav`).
//!
//! A WAV file's sizes are 32-bit: an export that would pass them fails with
//! [`Error::WavTooLarge`] before the file is created when the plan already shows
//! it (an existing file at the path is left alone), and otherwise at the frame
//! that crosses the limit, which removes the partial file it had begun.
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
    /// The speaker mask the finished file declares.
    mask: u32,
    layout: ChannelLayout,
    /// The frames the file may hold: [`wav::max_data_frames`], lowered by tests.
    max_frames: u64,
}

impl WavEncoder {
    /// Opens the file for `plan`, or fails with [`Error::WavTooLarge`] without
    /// touching `path` when the plan cannot fit.
    pub(crate) fn create(
        path: &Path,
        config: &ExportConfig,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
    ) -> Result<Self> {
        let max_frames = wav::max_data_frames(config.encode.channels, config.encode.bit_depth);
        Self::create_with_limit(path, config, source_rate, plan, max_frames)
    }

    /// [`create`](Self::create) with the frame limit a test lowers.
    fn create_with_limit(
        path: &Path,
        config: &ExportConfig,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        max_frames: u64,
    ) -> Result<Self> {
        let mask = match &config.encode.topology {
            Some(t) if t.layout() != config.encode.channels => {
                return Err(Error::InvalidConfig(format!(
                    "the topology is {:?} wide but the file has {:?}",
                    t.layout(),
                    config.encode.channels
                )));
            }
            Some(t) => wav::channel_mask(t),
            None => wav::default_channel_mask(config.encode.channels),
        };
        // Before `WavWriter::create`, which truncates an existing file: a
        // refused export must leave what is at `path` alone.
        if Self::planned_frames(plan, source_rate, config) > max_frames {
            return Err(Error::WavTooLarge {
                limit_frames: max_frames,
            });
        }
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
            mask,
            layout: config.encode.channels,
            max_frames,
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
        self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        let mut budget = FrameBudget {
            written: 0,
            max: self.max_frames,
        };
        let Self {
            mut writer,
            bit_depth,
            path,
            mask,
            layout,
            ..
        } = self;
        let width = u64::from(layout.count().max(1));
        let pumped = pump_blocks(src, source_rate, plan, config, |frames| {
            budget.admit(frames.samples().len() as u64 / width)?;
            // Already interleaved — hound wants a flat sample stream, which is
            // exactly what the render hands over. No de-interleave step to undo.
            for &s in frames.samples() {
                // The depth dispatch is `tutti-types`', shared with the live
                // `WavOut` sink; only the writer call per variant is local. An
                // offline render and a live capture therefore quantize a given
                // sample identically by construction, not by two hand-written
                // matches happening to agree.
                match bit_depth.quantize(s) {
                    Sample::I16(v) => writer.write_sample(v),
                    Sample::I24(v) => writer.write_sample(v),
                    Sample::F32(v) => writer.write_sample(v),
                }
                .map_err(io_err)?;
            }
            Ok(())
        });
        if let Err(e @ Error::WavTooLarge { .. }) = pumped {
            // The file holds a truncated render, which the error says is not an
            // export: close it, then take it away rather than leave it looking
            // like one.
            drop(writer);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        pumped?;
        writer.finalize().map_err(io_err)?;
        // `hound` writes the lowest-N-bits speaker mask and has no way to change
        // it; the one for the channels' speakers goes in over the header.
        wav::patch_channel_mask_at(&path, mask)?;
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
                topology: None,
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
        let rate = config.render.sample_rate;
        WavEncoder::create_with_limit(path, config, rate, &plan, max_frames)?.encode(
            &mut PlaneSource::new(planes),
            rate,
            &plan,
            config,
        )
    }

    /// An export at exactly the limit succeeds, and its 6-channel header
    /// carries the 5.1 (side) mask rather than `hound`'s `0x3F`; four channels
    /// are unassigned unless the config says they are quad.
    ///
    /// Mutation: dropping the `patch_channel_mask_at` call in `encode` leaves
    /// `0x3F` and fails the mask assertion; `admit` using `>=` fails the
    /// limit-exact case; ignoring `config.encode.topology` fails the quad row.
    #[test]
    fn an_export_at_the_limit_succeeds_and_declares_its_mask() {
        let dir = tempfile::tempdir().unwrap();
        for (channels, topology, mask) in [
            (6u16, None, 0x60Fu32),
            (4, None, 0),
            (4, Some(tutti_core::ChannelTopology::quad()), 0x33),
        ] {
            let path = dir.path().join(format!("w{channels}_{mask}.wav"));
            let planes = vec![vec![0.5f32; 8]; channels as usize];
            let mut cfg = config(channels, BitDepth::Int24);
            cfg.encode.topology = topology;
            encode(&path, &cfg, 8, &planes).expect("8 frames fit a limit of 8");
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), mask);
            assert_eq!(
                hound::WavReader::open(&path).unwrap().len(),
                8 * u32::from(channels)
            );
        }
        let mut cfg = config(2, BitDepth::Int24);
        cfg.encode.topology = Some(tutti_core::ChannelTopology::quad());
        let err = encode(&dir.path().join("bad.wav"), &cfg, 8, &vec![vec![0.0; 8]; 2])
            .expect_err("a quad topology is not stereo");
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
    }

    /// A plan that cannot fit is refused before the file is created: bytes
    /// already at the path survive.
    ///
    /// Mutation: moving the `planned_frames` check below `WavWriter::create`
    /// (the order before this fix) truncates the sentinel to a 44-byte empty
    /// WAV and fails the equality.
    #[test]
    fn a_plan_that_cannot_fit_is_refused_before_the_file_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(2, BitDepth::Int16);
        let plan = RenderPlan::new(&config.render);
        let rate = config.render.sample_rate;
        let path = dir.path().join("keep.wav");
        std::fs::write(&path, b"sentinel").unwrap();
        let err = WavEncoder::create_with_limit(&path, &config, rate, &plan, 7)
            .err()
            .expect("8 frames do not fit a limit of 7");
        assert!(
            matches!(err, Error::WavTooLarge { limit_frames: 7 }),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"sentinel",
            "a refused export must not touch the file at the path"
        );
    }

    /// A render that crosses the limit mid-way leaves no file behind. The plan
    /// handed to `create` is a short one so the up-front check passes; the
    /// render itself is 8 frames against a limit of 6.
    ///
    /// Mutation: deleting the `remove_file` call leaves a finalized prefix and
    /// fails the `exists` assertion.
    #[test]
    fn a_render_that_crosses_the_limit_removes_its_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(2, BitDepth::Int16);
        let rate = config.render.sample_rate;
        let mut short = config.clone();
        short.render.duration_seconds = 0.004;
        let short_plan = RenderPlan::new(&short.render);
        let plan = RenderPlan::new(&config.render);
        let path = dir.path().join("partial.wav");
        let enc = WavEncoder::create_with_limit(&path, &config, rate, &short_plan, 6).unwrap();
        let planes = vec![vec![0.5f32; 8]; 2];
        let err = enc
            .encode(&mut PlaneSource::new(&planes), rate, &plan, &config)
            .expect_err("8 frames cross a limit of 6");
        assert!(
            matches!(err, Error::WavTooLarge { limit_frames: 6 }),
            "{err}"
        );
        assert!(!path.exists(), "the truncated file must be removed");
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
