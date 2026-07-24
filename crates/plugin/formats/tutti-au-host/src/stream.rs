//! Stream-format configuration (sample rate, block size, channel layout).

#![cfg(target_os = "macos")]

use tutti_plugin_types::ChannelLayout;

use crate::error::Result;
use crate::ffi::{get_property, set_property};
use crate::handle::AuHandle;
use crate::types::*;

/// Input/output channel layouts for an AU.
///
/// [`ChannelLayout`] cannot represent a zero channel count, so the
/// "has an input bus at all" distinction (effect vs. generator/instrument) is
/// carried separately in [`AuBusLayout::has_input`]; `inputs` is meaningful
/// only when that flag is set.
#[derive(Debug, Clone, Copy)]
pub struct AuBusLayout {
    /// Input channel layout. Meaningful only when [`AuBusLayout::has_input`] is
    /// `true`; generators and instruments have no input bus.
    pub inputs: ChannelLayout,
    /// Output channel layout.
    pub outputs: ChannelLayout,
    /// Whether the AU has an input bus at all (effects do; generators /
    /// instruments do not). AU probing reports `0` input channels for the
    /// latter, which [`ChannelLayout`] cannot encode, so the presence bit is
    /// tracked here.
    pub has_input: bool,
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
    /// Channel layout (input + output buses).
    pub channels: AuBusLayout,
}

impl StreamConfig {
    /// Build a config from explicit values.
    pub fn new(sample_rate: f64, block_size: u32, channels: AuBusLayout) -> Self {
        Self {
            sample_rate,
            block_size,
            channels,
        }
    }

    /// Query the AU's current stream format to discover its channel layout.
    ///
    /// Falls back to stereo out / no input if the AU refuses the queries.
    pub(crate) fn probe(handle: &AuHandle) -> AuBusLayout {
        let unit = handle.raw_unit();
        let outputs = unsafe {
            get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
            )
        }
        .map(|asbd| ChannelLayout::from(asbd.mChannelsPerFrame))
        .unwrap_or(ChannelLayout::Stereo);

        let input_count = unsafe {
            get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
            )
        }
        .map(|asbd| asbd.mChannelsPerFrame)
        .unwrap_or(0);

        AuBusLayout {
            inputs: ChannelLayout::from(input_count),
            outputs,
            has_input: input_count > 0,
        }
    }

    /// Write this configuration onto the AU and return the *effective* channel
    /// layout the AU actually accepted.
    ///
    /// Sets `MaximumFramesPerSlice`, then the input/output stream formats.
    /// Stream-format sets are best-effort (a rejection is not fatal) because
    /// many AUs refuse mono/non-native formats and keep their own layout. When
    /// that happens we must not assume the requested channel counts stuck: we
    /// re-`get_property` the accepted format and report the layout the AU is
    /// really running, so the caller sizes `RenderScratch` to match. Sizing the
    /// scratch to a rejected (larger) layout is a topology mismatch that reads
    /// out-of-bounds during render.
    pub(crate) fn apply(&self, handle: &AuHandle) -> Result<AuBusLayout> {
        let unit = handle.raw_unit();

        let effective = unsafe {
            set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &self.block_size,
            )?;

            let out_asbd = AudioStreamBasicDescription::float32(
                self.sample_rate,
                self.channels.outputs.count().max(2) as u32,
            );
            let _ = set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &out_asbd,
            );
            // Read back what the AU actually accepted for the output scope.
            let effective_outputs = get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
            )
            .map(|asbd| ChannelLayout::from(asbd.mChannelsPerFrame))
            .unwrap_or(self.channels.outputs);

            let mut effective_inputs = self.channels.inputs;
            if self.channels.has_input {
                let in_asbd = AudioStreamBasicDescription::float32(
                    self.sample_rate,
                    self.channels.inputs.count() as u32,
                );
                let _ = set_property(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &in_asbd,
                );
                // Read back what the AU actually accepted for the input scope.
                effective_inputs = get_property::<AudioStreamBasicDescription>(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                )
                .map(|asbd| ChannelLayout::from(asbd.mChannelsPerFrame))
                .unwrap_or(self.channels.inputs);
            }

            AuBusLayout {
                inputs: effective_inputs,
                outputs: effective_outputs,
                has_input: self.channels.has_input,
            }
        };

        Ok(effective)
    }
}
