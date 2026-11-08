//! Bounce-time and buffer-strategy properties, plus the push-model render path.
//!
//! Four AUv2 facilities that a live-only host never needs and an exporting host
//! cannot do without. Grouped here rather than in [`crate::stream`] because they
//! share one duty: how the host intends to *drive* the render, as opposed to
//! what the stream looks like ([`crate::stream`]) or what the topology is
//! ([`crate::bus`]).
//!
//! * **Offline render** — tells the AU this block is part of a
//!   faster-than-real-time bounce, so it may take the slow high-quality path.
//! * **In-place processing** — whether the AU tolerates input and output
//!   aliasing the same memory, which is a copy per block per plugin the host can
//!   skip.
//! * **Render quality** — a 0–127 knob some AUs honour.
//! * **`AudioUnitProcess`** — the *push* render model, where the host hands
//!   input buffers in directly instead of waiting to be pulled through a render
//!   callback.
//!
//! ## What was measured, and what therefore is not here
//!
//! `AudioUnitProcessMultiple` is the only AUv2 way a sidechain reaches a
//! compressor, and **it is not usable against anything installed on this
//! machine** — see [`process_push_multiple`]'s docs for the numbers.
//! [`process_push_multiple`] is kept as a thin, tested wrapper anyway: it is the
//! only way to reach the sidechain contract at all, and the day an AU implements
//! it the host needs no new code. The single-list [`process_push`] is the
//! feature that is real: measured bit-identical to `AudioUnitRender` on AUDelay
//! across 8 blocks (`0.35350975` peak from both paths, every block).
//!
//! ## Why the push path is a separate module and not a `process` variant
//!
//! [`AuActive::process`](crate::instance::AuActive::process) is a *pull* render:
//! it stages input into the heap-pinned `RenderScratch`, then `AudioUnitRender`
//! calls back into that scratch to fetch it. The push path never installs a
//! callback — with `AudioUnitProcess` the `ioData` buffer list carries the
//! input **in** and the output back **out** in the same call. Keeping the two
//! apart (each needs differently-shaped scratch: one list per bus for push,
//! exactly one for pull) is what stops the pull path's callback from firing
//! during a push render and overwriting the input the host just handed in.

#![cfg(target_os = "macos")]

use tutti_au_ffi::Unit;

use crate::error::{AuError, Result};
use crate::types::*;

/// `kAudioUnitProperty_OfflineRender` (37).
///
/// Aliased here rather than in [`crate::types`] with its siblings: keeping the
/// id beside its only two call sites means the property number and the code
/// that interprets the AU's answer cannot drift apart.
pub(crate) const K_AUDIO_UNIT_PROPERTY_OFFLINE_RENDER: u32 =
    coreaudio_sys::kAudioUnitProperty_OfflineRender;

/// The inclusive maximum this host will write to
/// `kAudioUnitProperty_RenderQuality`.
///
/// 127 — the range Apple's `AudioUnitProperties.h` documents (`kRenderQuality_Max`).
/// Enforced host-side because of what was measured: of the four Apple units on
/// macOS 15.6 that implement the property, **only AUDistortion actually rejects
/// an out-of-range value** (`-50`, `paramErr`, above 127). AUMatrixReverb,
/// DLSMusicDevice and AUMultiChannelMixer all accept a write of `999` with
/// `noErr` **and read it back**; the mixer round-trips `u32::MAX` too. So the AU
/// neither rejects nor normalizes the nonsense — it stores it. See
/// [`set_render_quality`].
pub const RENDER_QUALITY_MAX: u32 = 127;

/// Whether the AU has been told it is rendering offline.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_OfflineRender`, which on macOS 15.6 is **every Apple
/// effect and mixer** — only the instruments (AUSampler, DLSMusicDevice)
/// implement it at all. Propagated rather than flattened to `false`: "this AU is
/// in real-time mode" (changeable) and "this AU has no offline mode" (the export
/// may differ from the audition, and nothing can be done) are different facts a
/// bouncing host must distinguish, and the second is the common case here.
pub(crate) fn is_offline_render(unit: &Unit) -> Result<bool> {
    let value: u32 = unit.get(
        K_AUDIO_UNIT_PROPERTY_OFFLINE_RENDER,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    Ok(value != 0)
}

/// Declare to the AU whether this render is a faster-than-real-time bounce.
///
/// ## What a DAW loses without this
///
/// The render calls are identical either way; the property changes what the AU
/// is *permitted* to do. Apple's header names the case outright: an AU "that
/// normally operates within a general real-time calling model" may behave
/// differently once it knows the result is going to a file. Three concrete
/// divergences a host that never sets it will ship:
///
/// * **Dropout protection stays armed** — a real-time AU may drop to a cheaper
///   algorithm or repeat the previous block to avoid missing its deadline; during
///   a bounce there is no deadline, so that protection is pure quality loss,
///   worst on the heaviest parts of the mix.
/// * **Cheap resampling** — an AU that resamples internally (pitch shift,
///   time-stretch, oversampled saturators) picks a short interpolation kernel in
///   real time and a longer one offline; without the flag the bounce carries
///   more aliasing than the monitor path did.
/// * **Non-deterministic dither** — an AU may seed dither from the clock in real
///   time and from a fixed seed offline, so two bounces of the same project stop
///   being bit-identical, breaking mastering-workflow checks and the host's own
///   render comparisons.
///
/// The symptom users report is "the export doesn't sound like the mix", with
/// nothing in the project changed.
///
/// ## What was measured
///
/// On macOS 15.6 only the two instruments implement the property, and neither
/// audibly changes: AUSampler and DLSMusicDevice render the same peak
/// (`0.246602` / `0.095300` over 16 blocks of a held middle C) flag set or
/// clear. So setting it buys nothing on this corpus — the plugins it's designed
/// for are third-party resamplers/ditherers, and TDR Nova / TAL-Reverb-4 both
/// expose it (reading `0` by default) with no way here to prove they honour it.
///
/// ## Set it before `initialize`
///
/// Legal in either state (measured), but an AU that sizes an internal
/// oversampling buffer from the flag can only do so at `AudioUnitInitialize` —
/// setting it after is accepted yet may silently not take effect. Set the flag,
/// then `initialize`, the same order as `MaximumFramesPerSlice`.
///
/// ## Width: 4 bytes, and a narrower write also works here
///
/// `AudioUnitGetPropertyInfo` reports size 4 (`UInt32`), and — unlike
/// [`AuInstance::set_bypass`](crate::instance::AuInstance::set_bypass)'s
/// `BypassEffect`, which refuses a 1-byte write with `-10851` — a 1-byte write to
/// `OfflineRender` is accepted with `noErr` by both instruments. The `u32` is
/// used anyway since that's what the header declares; the leniency is one
/// unit's quirk, not a contract.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU has no offline-render property. See
/// [`is_offline_render`] for why that is an error rather than a silent no-op.
pub(crate) fn set_offline_render(unit: &Unit, offline: bool) -> Result<()> {
    let value: u32 = u32::from(offline);
    Ok(unit.set(
        K_AUDIO_UNIT_PROPERTY_OFFLINE_RENDER,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &value,
    )?)
}

/// Whether the AU is willing to render with input and output aliasing the same
/// buffer.
///
/// A host that gets `true` here may hand [`process_push`] one buffer list and
/// let the AU overwrite it, saving a copy per block per plugin — on a 64-plugin
/// session at 48 kHz / 64-frame blocks, ~48 000 stereo copies a second avoided.
///
/// Measured on macOS 15.6: six Apple effects advertise it and all six report
/// **1** (capable) — AUDelay, AUDynamicsProcessor, AUDistortion, AULowpass,
/// AUSampleDelay, AUMultibandCompressor. AUMatrixReverb, AUReverb2 and AUNBandEQ
/// do not implement the property, nor does any instrument, mixer, or third-party
/// unit. No unit on the system reports `0`.
///
/// ## Read-only, deliberately, even though the property is writable
///
/// Apple declares this Read/**Write** — the write direction lets a host whose
/// buffer management would be *defeated* by in-place operation set `0` to forbid
/// it. This crate exposes only the read: tutti has no such strategy to defend,
/// writing `0` can only make the AU do more work, and the only host for which
/// that write is correct is one already committed to holding the pre-effect
/// signal (a dry/wet mix computed outside the plugin, a look-ahead peek).
/// Nothing here does, so a setter would offer only the pessimal setting.
///
/// The read, by contrast, is load-bearing: it decides whether [`process_push`]
/// may alias its buffers or must copy.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_InPlaceProcessing`. Not flattened to `false`: `false`
/// means "the AU says no, do not alias", a refusal means "the AU did not say".
/// Both lead the host to copy, but only the first is a fact about the AU —
/// flattening would make the in-place census read "9 units forbid it" when the
/// truth is "6 permit it and 6 never said" (no unit measured reports `0`).
pub(crate) fn supports_in_place(unit: &Unit) -> Result<bool> {
    let value: u32 = unit.get(
        K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    Ok(value != 0)
}

/// The AU's current render-quality setting.
///
/// Measured defaults on macOS 15.6: AUDistortion `64`, AUMatrixReverb `127`,
/// DLSMusicDevice `127`, AUMultiChannelMixer `64`. Every other unit on the
/// system refuses the property.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU has no `kAudioUnitProperty_RenderQuality`,
/// which is the common case — see [`set_render_quality`].
pub(crate) fn render_quality(unit: &Unit) -> Result<u32> {
    Ok(unit.get(
        K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?)
}

/// Sets the AU's render quality, `0` (cheapest) to [`RENDER_QUALITY_MAX`] (best).
///
/// Not a substitute for [`set_offline_render`]: the offline flag says *why* the
/// host is rendering and leaves the AU to choose; this says what the host wants
/// regardless of context. A bounce sets both — offline so deterministic paths
/// engage, quality at maximum so an AU that exposes the knob also cooperates.
///
/// ## Why the range is enforced here rather than left to the AU
///
/// Three of the four units implementing the property do not clamp. Measured on
/// macOS 15.6:
///
/// | unit | write 128 | write 999 | write `u32::MAX` |
/// |---|---|---|---|
/// | AUDistortion | `-50`, reads 127 | `-50`, reads 127 | `-50`, reads 127 |
/// | AUMatrixReverb | `noErr`, reads **128** | `noErr`, reads **999** | `noErr`, reads 127 |
/// | DLSMusicDevice | `noErr`, reads **128** | `noErr`, reads **999** | `noErr`, reads 127 |
/// | AUMultiChannelMixer | `noErr`, reads 128 | `noErr`, reads 999 | `noErr`, reads **`u32::MAX`** |
///
/// Only AUDistortion refuses with `paramErr` and keeps its previous value; the
/// other three *store the nonsense and hand it back*, indistinguishable from a
/// legitimate setting by reading the property back. Rejecting host-side turns
/// that silent acceptance into a visible [`AuError::InvalidBuffer`]. The
/// `u32::MAX` row rules out "write it and read it back to check" as a fix —
/// AUMultiChannelMixer round-trips that value faithfully too.
///
/// # Errors
/// * [`AuError::InvalidBuffer`] for a `quality` above [`RENDER_QUALITY_MAX`].
/// * [`AuError::OsStatus`] when the AU has no render-quality property. That is
///   the majority: 8 of the 12 corpus units refuse it with
///   `kAudioUnitErr_InvalidProperty` (-10879), including AUDelay, AUReverb2,
///   AUNBandEQ and AUSampler.
pub(crate) fn set_render_quality(unit: &Unit, quality: u32) -> Result<()> {
    if quality > RENDER_QUALITY_MAX {
        return Err(AuError::InvalidBuffer(format!(
            "render quality {quality} exceeds the maximum {RENDER_QUALITY_MAX}"
        )));
    }
    Ok(unit.set(
        K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &quality,
    )?)
}

pub use tutti_au_ffi::{PushScratch, output_byte_sizes};

/// Runs one block through `AudioUnitProcess`: the input staged in `scratch`
/// goes in, the processed block comes out in `scratch`, and the unit's render
/// flags are returned. See `tutti_au_ffi::process_push` for the contract.
///
/// # Errors
/// [`AuError::InvalidBuffer`] if `frames` exceeds the scratch's block size, or
/// [`AuError::RenderFailed`] with the unit's `LastRenderError` attached.
pub fn process_push(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    Ok(tutti_au_ffi::process_push(unit, scratch, frames)?)
}

/// As [`process_push`], through `AudioUnitProcessMultiple`: every input and
/// output bus in one call.
///
/// # Errors
/// As [`process_push`].
pub fn process_push_multiple(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    Ok(tutti_au_ffi::process_push_multiple(unit, scratch, frames)?)
}
