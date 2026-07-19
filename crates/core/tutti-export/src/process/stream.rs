//! Per-chunk mastering for streaming exports.
//!
//! Unlike [`Chain`], which owns the full signal, [`StreamProcessor`] carries
//! persistent state (the noise-shaping dither) across chunks so that a
//! streaming render never needs to materialize the whole signal in memory.
//!
//! Only the operations that are legal without full-signal access live here:
//! dither and optional mono downmix. Normalization and resampling require
//! the whole signal and are enforced upstream.

use crate::options::{BitDepth, ChannelMode, Dither};
use crate::process::{apply_dither, stereo_to_mono, DitherState};

/// Per-chunk configuration. Values are fixed at construction so the processor
/// does not have to dispatch on runtime flags inside the hot path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamConfig {
    pub dither: Dither,
    pub bit_depth: BitDepth,
    pub channels: ChannelMode,
}

/// One processed chunk ready for the encoder.
pub(crate) enum Chunk {
    Stereo { left: Vec<f32>, right: Vec<f32> },
    Mono(Vec<f32>),
}

/// Stateful per-chunk processor. Owns the dither state so noise shaping is
/// continuous across chunk boundaries.
pub(crate) struct StreamProcessor {
    state: DitherState,
    config: StreamConfig,
}

impl StreamProcessor {
    pub fn new(config: StreamConfig) -> Self {
        Self {
            state: DitherState::new(config.dither),
            config,
        }
    }

    /// Apply dither to a chunk (if configured) and return either a stereo
    /// or mono [`Chunk`] according to [`StreamConfig::channels`].
    pub fn process_chunk(&mut self, left: &[f32], right: &[f32]) -> Chunk {
        let mut left_buf = left.to_vec();
        let mut right_buf = right.to_vec();

        if !matches!(self.config.dither, Dither::Off) {
            apply_dither(
                &mut left_buf,
                &mut right_buf,
                self.config.bit_depth.bits(),
                &mut self.state,
            );
        }

        match self.config.channels {
            ChannelMode::Stereo => Chunk::Stereo {
                left: left_buf,
                right: right_buf,
            },
            ChannelMode::Mono => Chunk::Mono(stereo_to_mono(&left_buf, &right_buf)),
        }
    }
}
