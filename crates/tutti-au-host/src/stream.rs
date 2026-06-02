//! Stream-format configuration (sample rate, block size, channel layout).

#![cfg(target_os = "macos")]

use crate::error::Result;
use crate::ffi::{get_property, set_property};
use crate::handle::AuHandle;
use crate::types::*;

/// Input/output channel counts for an AU.
///
/// `inputs` is `0` for generators and instruments.
#[derive(Debug, Clone, Copy)]
pub struct ChannelLayout {
    /// Number of input channels (may be 0).
    pub inputs: u32,
    /// Number of output channels.
    pub outputs: u32,
}

/// Aggregate stream configuration applied to an AU before initialization.
///
/// This bundles sample rate, maximum block size, and channel layout so they
/// can be applied atomically to the AU before initialization.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Sample rate in Hz.
    pub sample_rate: f64,
    /// Maximum frames the AU will be asked to render in a single `process()` call.
    pub block_size: u32,
    /// Channel layout (input + output counts).
    pub channels: ChannelLayout,
}

impl StreamConfig {
    /// Build a config from explicit values.
    pub fn new(sample_rate: f64, block_size: u32, channels: ChannelLayout) -> Self {
        Self {
            sample_rate,
            block_size,
            channels,
        }
    }

    /// Query the AU's current stream format to discover its channel layout.
    ///
    /// Falls back to stereo out / no input if the AU refuses the queries.
    pub(crate) fn probe(handle: &AuHandle) -> ChannelLayout {
        let unit = handle.raw_unit();
        let outputs = unsafe {
            get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
            )
        }
        .map(|asbd| asbd.channels_per_frame)
        .unwrap_or(2);

        let inputs = unsafe {
            get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
            )
        }
        .map(|asbd| asbd.channels_per_frame)
        .unwrap_or(0);

        ChannelLayout { inputs, outputs }
    }

    /// Write this configuration onto the AU.
    ///
    /// Sets `MaximumFramesPerSlice`, then the input/output stream formats.
    /// Stream-format sets are best-effort (ignored on failure) because many
    /// AUs refuse mono formats, and the AU's native layout is acceptable.
    pub(crate) fn apply(&self, handle: &AuHandle) -> Result<()> {
        let unit = handle.raw_unit();

        unsafe {
            set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &self.block_size,
            )?;

            let out_asbd = AudioStreamBasicDescription::float32(
                self.sample_rate,
                self.channels.outputs.max(2),
            );
            let _ = set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &out_asbd,
            );

            if self.channels.inputs > 0 {
                let in_asbd =
                    AudioStreamBasicDescription::float32(self.sample_rate, self.channels.inputs);
                let _ = set_property(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &in_asbd,
                );
            }
        }

        Ok(())
    }
}
