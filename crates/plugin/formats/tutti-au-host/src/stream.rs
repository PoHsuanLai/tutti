//! Stream-format configuration (sample rate, block size, channel layout).

#![cfg(target_os = "macos")]

use tutti_plugin_types::ChannelLayout;

use crate::bus::BusDirection;
use crate::error::{AuError, Result};
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

    /// Query the AU's current stream format on bus 0 to discover its channel
    /// layout.
    ///
    /// Bus 0 only, deliberately: this layout is what the render scratch is sized
    /// from, and [`AuInstance::process`](crate::instance::AuInstance::process)
    /// renders bus 0 alone. Multi-bus units are *described* by
    /// [`crate::bus`] — `bus_count` / `bus_layout` — but not yet rendered
    /// per-bus, so widening the probe would size buffers for buses nothing
    /// reads.
    ///
    /// `has_input` is taken from the AU's own input **element count**, not from
    /// whether the stream-format query happened to succeed. Those differ: an AU
    /// can have an input element whose format it declines to report, and the old
    /// "format read failed ⇒ 0 channels ⇒ no input" inference would then skip
    /// installing the render callback on a unit that genuinely needs one. The
    /// element count is the AU's direct answer to "is there an input bus".
    ///
    /// Falls back to stereo out / no input if the AU refuses the queries.
    pub(crate) fn probe(handle: &AuHandle) -> AuBusLayout {
        let unit = handle.raw_unit();
        let outputs = unsafe { crate::bus::bus_layout(unit, BusDirection::Output, 0) }
            .unwrap_or(ChannelLayout::Stereo);

        // An AU with zero input elements has no bus 0 to ask about, so skip the
        // format query entirely rather than reading -10877 and inferring from it.
        let has_input = unsafe { crate::bus::bus_count(unit, BusDirection::Input) } > 0;
        let inputs = if has_input {
            unsafe { crate::bus::bus_layout(unit, BusDirection::Input, 0) }
                .unwrap_or(ChannelLayout::Stereo)
        } else {
            ChannelLayout::Multi(0)
        };

        AuBusLayout {
            inputs,
            outputs,
            has_input,
        }
    }

    /// The largest relative deviation tolerated between the requested sample
    /// rate and the rate the AU reports back.
    ///
    /// The ASBD carries `mSampleRate` as an `f64` and AUs echo it verbatim, so
    /// an accepted rate normally compares bit-exact; this only absorbs the
    /// rounding an AU introduces if it round-trips the rate through a narrower
    /// representation. It is far tighter than any real rate step (44.1k vs 48k
    /// is 8%), so a genuinely rejected rate never slips through.
    const SAMPLE_RATE_TOLERANCE: f64 = 1e-6;

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
    ///
    /// The **sample rate**, unlike the channel layout, is NOT best-effort.
    /// A channel-count rejection is recoverable — we resize the
    /// scratch and carry on — but a rejected sample rate is not: the config
    /// would record a rate the AU is not running at, and `sample_rate()` /
    /// `get_latency()` both trust that number, so the block would be rendered
    /// at the wrong rate (pitch/time drift) with a PDC latency computed against
    /// a phantom rate. So the accepted `mSampleRate` is read back and a
    /// mismatch is a hard error.
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

            // JUCE sets `kAudioUnitProperty_SampleRate` per scope in addition to
            // carrying the rate in the ASBD; some AUs only honour one of the
            // two. Both writes are best-effort here — the read-back below is
            // what actually decides whether the rate took.
            let _ = set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &self.sample_rate,
            );
            if self.channels.has_input {
                let _ = set_property(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &self.sample_rate,
                );
            }

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
            // Unlike the channel count, a sample-rate mismatch is fatal.
            let out_accepted = get_property::<AudioStreamBasicDescription>(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
            );
            if let Ok(asbd) = out_accepted.as_ref() {
                self.check_sample_rate("output", asbd.mSampleRate)?;
            }
            let effective_outputs = out_accepted
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
                let in_accepted = get_property::<AudioStreamBasicDescription>(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                );
                if let Ok(asbd) = in_accepted.as_ref() {
                    self.check_sample_rate("input", asbd.mSampleRate)?;
                }
                effective_inputs = in_accepted
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

    /// Fail loudly when the AU kept a different sample rate than the one
    /// requested, instead of letting the config record a rate the plugin is not
    /// actually running at.
    ///
    /// A rate of `0.0` means "not yet configured" on some AUs (the ASBD is
    /// zero-initialized before any format is set) and is not treated as a
    /// rejection — there is nothing to disagree with yet.
    ///
    /// `scope` names which bus disagreed, and both rates are reported verbatim,
    /// because "the AU is at 44100 while the host thinks 48000" is the only
    /// diagnosable form of this failure.
    fn check_sample_rate(&self, scope: &'static str, accepted: f64) -> Result<()> {
        if accepted == 0.0 {
            return Ok(());
        }
        let deviation = (accepted - self.sample_rate).abs();
        if deviation > self.sample_rate.abs() * Self::SAMPLE_RATE_TOLERANCE {
            return Err(AuError::SampleRateRejected {
                scope,
                requested: self.sample_rate,
                accepted,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_at(sample_rate: f64) -> StreamConfig {
        StreamConfig::new(
            sample_rate,
            512,
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
        )
    }

    /// The old code `let _`'d the stream-format set, re-read only
    /// `mChannelsPerFrame`, and returned `Ok(())` regardless — so a rejected
    /// rate was recorded as if accepted. The read-back must reject it.
    #[test]
    fn a_rate_the_au_did_not_accept_is_an_error() {
        let config = config_at(48_000.0);

        // Exactly what was asked for: accepted.
        assert!(config.check_sample_rate("output", 48_000.0).is_ok());

        // The AU kept its own rate. This is the case that used to pass silently
        // and then render at the wrong rate with a PDC latency computed against
        // a phantom 48 kHz.
        let err = config
            .check_sample_rate("output", 44_100.0)
            .expect_err("a different rate must not be reported as success");
        match err {
            AuError::SampleRateRejected {
                scope,
                requested,
                accepted,
            } => {
                assert_eq!(scope, "output");
                assert_eq!(requested, 48_000.0);
                assert_eq!(accepted, 44_100.0);
            }
            other => panic!("expected SampleRateRejected, got {other:?}"),
        }

        // Both directions, and the input scope is named separately.
        assert!(config_at(44_100.0)
            .check_sample_rate("input", 48_000.0)
            .is_err());
        assert!(matches!(
            config_at(44_100.0).check_sample_rate("input", 96_000.0),
            Err(AuError::SampleRateRejected { scope: "input", .. })
        ));
    }

    /// An unconfigured ASBD reads `mSampleRate == 0.0`; that is "nothing set
    /// yet", not a rejection, and must not fail the apply.
    #[test]
    fn an_unconfigured_rate_is_not_a_rejection() {
        assert!(config_at(48_000.0).check_sample_rate("output", 0.0).is_ok());
    }

    /// The tolerance is for f64 round-tripping only — it must be far too tight
    /// to swallow any real rate step (44.1k vs 48k is 8%).
    #[test]
    fn tolerance_absorbs_rounding_but_not_a_real_rate_change() {
        let config = config_at(48_000.0);
        // Sub-ULP-scale rounding: accepted.
        let rounded = 48_000.0 + 48_000.0 * (StreamConfig::SAMPLE_RATE_TOLERANCE * 0.5);
        assert!(config.check_sample_rate("output", rounded).is_ok());
        // One hertz off is already 20x the tolerance window: rejected.
        assert!(config.check_sample_rate("output", 48_001.0).is_err());
    }
}
