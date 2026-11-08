//! Latency and tail time: the two durations an AU reports about itself, and the
//! one it is told about the chain after it.
//!
//! The host transport callbacks an AU pulls (tempo, beat, play state) are
//! `tutti-au-ffi`'s [`TransportState`]; this module is the property reads and
//! writes around them.

#![cfg(target_os = "macos")]

use tutti_au_ffi::Unit;
use tutti_types::value::units::Seconds;

/// Seconds of audio an AU keeps producing after its input goes silent.
///
/// Read from `kAudioUnitProperty_TailTime`, which the AU reports in seconds.
///
/// # Why `Seconds` and not samples
///
/// [`get_latency`](crate::instance::AuInstance::get_latency) converts the AU's
/// seconds to samples because latency is consumed as a *delay-line length* —
/// an integer count of frames to compensate. Tail is consumed as a *duration*:
/// the offline bounce asks "how much longer do I keep rendering", and the
/// answer is naturally a span of time that the caller converts at whatever rate
/// it is rendering at, which need not be the rate this instance is configured
/// for. Returning [`Seconds`] keeps that conversion at the call site, where the
/// rate is known, and `Seconds::to_samples_ceil` is the named converter for it
/// — the *allocation* rounding, because a bounce that rounds a tail down
/// truncates it, which is the exact bug this property exists to prevent.
///
/// # Errors
/// Returns [`AuError::OsStatus`](crate::error::AuError::OsStatus) when the AU
/// does not implement the property. That is not an edge case: measured on macOS
/// 15.6, **every** Apple instrument, mixer and generator rejects it with
/// `kAudioUnitErr_InvalidProperty` (-10879), while every Apple effect answers.
///
/// The error is propagated rather than absorbed into `Seconds(0.0)` because the
/// two mean opposite things to a bounce. `AUSampleDelay` genuinely reports a
/// 0.0-second tail; a mixer reports *nothing*. Flattening the second into the
/// first would tell the caller a unit of unknown tail has none, and the bounce
/// would truncate exactly the material the property was read to protect.
pub(crate) fn tail_time(unit: &Unit) -> crate::error::Result<Seconds> {
    use crate::types::{K_AUDIO_UNIT_PROPERTY_TAIL_TIME, K_AUDIO_UNIT_SCOPE_GLOBAL};

    // Apple's width is `Float64`; `Seconds` is f32-backed. The narrowing is
    // safe for every value this property carries — measured maxima are ~21 s
    // (AUDelay) and 10 s (AUMatrixReverb), far inside f32's exact-integer range
    // — and `Seconds` is the crate's unit for exactly this quantity.
    let seconds: f64 = unit.get(
        K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    Ok(Seconds(seconds as f32))
}

/// Tells the AU how far *downstream* of it the listener is, on one bus.
///
/// The direction is the opposite of every other latency call in this crate.
/// [`tail_time`] and [`get_latency`](crate::instance::AuInstance::get_latency)
/// ask the AU what it costs the host; this tells the AU what the rest of the
/// signal chain costs it. Apple's header: "This property is set by a host to
/// describe to the audio unit the presentation latency of both any of its input
/// and/or output audio data … A value of zero means either no latency or an
/// unknown latency."
///
/// A plugin uses it to align a display against audio the user has not heard yet
/// — the header's own example is "metering for an audio unit that is generating
/// output to be presented to the user at a future time". Nothing in the *audio*
/// path depends on it: an AU that ignores this renders identically, which is why
/// a refusal is not fatal.
///
/// # Per bus, not per unit
///
/// The header asks for this "on each active input and output bus (Scope/Element
/// pair)", because a multi-output AU's buses can be presented at different
/// times — one feeding a monitor path, another a bounce. So this takes an
/// explicit [`ParamAddress`](crate::parameters::ParamAddress)-shaped
/// `(direction, bus)` rather than assuming the global scope, and a host with
/// several active buses calls it once per bus. `Input` means "how long ago this
/// audio was captured", `Output` means "how long until it is heard".
///
/// # Why [`Seconds`] and not [`Samples`](tutti_types::Samples)
///
/// Apple's width is `Float64` seconds and this value is never a buffer length —
/// the host does not allocate against it and the AU does not index with it, it
/// is only ever displayed or compared. Converting to samples here would round a
/// quantity nobody counts in frames, and would bake in *this* instance's sample
/// rate at the moment of the call. [`Seconds`] is the crate's unit for a
/// duration on this boundary, the same choice [`tail_time`] makes and for the
/// same reason.
///
/// The f32→f64 widening is exact, so unlike [`tail_time`]'s narrowing there is
/// nothing to justify about precision in this direction.
///
/// # Errors
/// Returns [`AuError::OsStatus`](crate::error::AuError::OsStatus) when the AU
/// does not implement the property, which on this machine is **every AU**.
/// Measured on macOS 15.6 across all 39 instantiable registered units (35 Apple
/// plus TDR Nova, TAL Reverb 4 and TAL-NoiseMaker): every one answers
/// `kAudioUnitErr_InvalidProperty` (-10879), for both scopes, before and after
/// `AudioUnitInitialize`, at both `Float64` and `Float32` width, and
/// `AudioUnitGetPropertyInfo` reports the property as absent rather than as
/// present-but-unwritable. So no corpus unit can witness a success and the
/// positive path is driven only by [the probe
/// AU](../../tests/support/probe_au.rs).
///
/// The refusal is propagated rather than swallowed. A caller that wants to
/// treat "did not implement it" as routine — which is the correct default, since
/// the property is advisory — discards the `Result` deliberately at the call
/// site. Absorbing it here would make the `Result` unable to be `Err` and every
/// caller's handling unreachable, which is the bug
/// [`get_latency`](crate::instance::AuInstance::get_latency) already carried
/// once.
pub(crate) fn set_presentation_latency(
    unit: &Unit,
    direction: crate::bus::BusDirection,
    bus: u32,
    latency: Seconds,
) -> crate::error::Result<()> {
    use crate::types::K_AUDIO_UNIT_PROPERTY_PRESENTATION_LATENCY;

    let seconds = f64::from(latency.0);
    Ok(unit.set(
        K_AUDIO_UNIT_PROPERTY_PRESENTATION_LATENCY,
        direction.scope(),
        bus,
        &seconds,
    )?)
}
