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
//! [`AuReady::process`](crate::instance::AuReady::process) is a *pull* render:
//! it stages input into the heap-pinned `RenderScratch`, then `AudioUnitRender`
//! calls back into that scratch to fetch it. The push path never installs a
//! callback — with `AudioUnitProcess` the `ioData` buffer list carries the
//! input **in** and the output back **out** in the same call. Keeping the two
//! apart (each needs differently-shaped scratch: one list per bus for push,
//! exactly one for pull) is what stops the pull path's callback from firing
//! during a push render and overwriting the input the host just handed in.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use tutti_plugin_types::ChannelLayout;

use crate::buffer::{buffer_list_bytes, iter_buffers_mut};
use crate::error::{AuError, Result};
use crate::ffi::{get_property, set_property};
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
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn is_offline_render(unit: AudioUnit) -> Result<bool> {
    let value: u32 = get_property(
        unit,
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
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn set_offline_render(unit: AudioUnit, offline: bool) -> Result<()> {
    let value: u32 = u32::from(offline);
    set_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_OFFLINE_RENDER,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &value,
    )
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
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn supports_in_place(unit: AudioUnit) -> Result<bool> {
    let value: u32 = get_property(
        unit,
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
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn render_quality(unit: AudioUnit) -> Result<u32> {
    get_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )
}

/// Set the AU's render quality, `0` (cheapest) to [`RENDER_QUALITY_MAX`] (best).
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
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn set_render_quality(unit: AudioUnit, quality: u32) -> Result<()> {
    if quality > RENDER_QUALITY_MAX {
        return Err(AuError::InvalidBuffer(format!(
            "render quality {quality} exceeds the maximum {RENDER_QUALITY_MAX}"
        )));
    }
    set_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &quality,
    )
}

/// Pre-allocated buffer lists for the push render path.
///
/// One `AudioBufferList` slab per bus, each pointing at that bus's own channel
/// storage, all allocated once at construction so [`process_push`] and
/// [`process_push_multiple`] can bind and render without touching the
/// allocator: the push calls run on the audio thread, and a `Vec` grown per
/// block is an allocator acquisition every 1.3 ms at 48 kHz / 64 frames.
///
/// ## Why the fields are `Vec` and the render path still does not allocate
///
/// Every `Vec` here is grown exactly once, in [`PushScratch::new`]. The render
/// methods only write through existing storage: `bind_*` rewrites the `mData` /
/// `mDataByteSize` fields of `AudioBuffer`s that already exist, and the pointer
/// arrays' length never changes, so `as_mut_ptr` on them is a field read.
/// `tests/au_offline.rs` proves it — the property is not something the types
/// can express.
pub struct PushScratch {
    /// One `AudioBufferList` slab per input bus. 8-aligned `u64` backing for the
    /// reason [`crate::buffer`]'s `AblWord` documents: `AudioBufferList` needs
    /// 8-byte alignment and a `Box<[u8]>` gives only 1.
    input_slabs: Vec<Box<[u64]>>,
    /// Per-input-bus planar channel storage: `input_audio[bus][channel][frame]`.
    input_audio: Vec<Vec<Vec<f32>>>,
    /// One slab per output bus, as `input_slabs`.
    output_slabs: Vec<Box<[u64]>>,
    /// Per-output-bus planar channel storage.
    output_audio: Vec<Vec<Vec<f32>>>,
    /// `*const AudioBufferList` per input bus, in bus order — the array
    /// `AudioUnitProcessMultiple` takes as `inInputBufferLists`. A field rather
    /// than a local so the pointer array itself is not built per block.
    input_ptrs: Vec<*const AudioBufferList>,
    /// `*mut AudioBufferList` per output bus, for `ioOutputBufferLists`.
    output_ptrs: Vec<*mut AudioBufferList>,
    /// Channel width of each input bus, parallel to `input_audio`.
    input_widths: Vec<ChannelLayout>,
    /// Channel width of each output bus.
    output_widths: Vec<ChannelLayout>,
    /// Frames each bus's channel vectors were sized for — the bound the render
    /// functions enforce.
    block_size: u32,
    /// Sample cursor for the render timestamp, mirroring the pull path's. The
    /// push path keeps its **own** rather than sharing: they are separate render
    /// sessions, and one shared cursor would make the timestamps of whichever
    /// path was not driving jump forward, which an AU whose internal LFO is
    /// phased off `mSampleTime` hears as a discontinuity.
    ///
    /// Advances by one block per render and returns to zero on
    /// [`reset_position`](PushScratch::reset_position).
    sample_position: f64,
}

impl PushScratch {
    /// Allocate for the given per-bus input and output widths.
    ///
    /// `inputs` and `outputs` are one entry per bus, in bus order — so a
    /// hypothetical sidechain compressor is `&[Stereo, Stereo]` in, `&[Stereo]`
    /// out. Size these from
    /// [`AuInstance::bus_count`](crate::instance::AuInstance::bus_count), never
    /// from what the host wishes were there: an AU handed more input lists than
    /// it has input elements rejects the render outright rather than ignoring
    /// the extra (measured: AUReverb2 answers `kAudioUnitErr_InvalidElement` for
    /// a second list).
    ///
    /// An empty `inputs` is legal and is what an instrument gets; it makes
    /// [`process_push_multiple`] pass a zero-length input array, which is what
    /// AudioToolbox expects for a unit with no input elements.
    ///
    /// # Panics
    /// On a zero `block_size`, which would size every channel vector to empty
    /// and make each render a no-op that still reported success — the same
    /// degenerate value
    /// [`AuInstance::set_block_size`](crate::instance::AuInstance::set_block_size)
    /// refuses.
    pub fn new(inputs: &[ChannelLayout], outputs: &[ChannelLayout], block_size: u32) -> Self {
        assert!(block_size > 0, "PushScratch block_size must be non-zero");
        let frames = block_size as usize;

        let alloc_slab = |layout: ChannelLayout| -> Box<[u64]> {
            let bytes = buffer_list_bytes(layout.count() as usize);
            let words = bytes.div_ceil(std::mem::size_of::<u64>());
            vec![0u64; words].into_boxed_slice()
        };
        let alloc_audio = |layout: ChannelLayout| -> Vec<Vec<f32>> {
            (0..layout.count() as usize)
                .map(|_| vec![0.0f32; frames])
                .collect()
        };

        let mut me = Self {
            input_slabs: inputs.iter().copied().map(alloc_slab).collect(),
            input_audio: inputs.iter().copied().map(alloc_audio).collect(),
            output_slabs: outputs.iter().copied().map(alloc_slab).collect(),
            output_audio: outputs.iter().copied().map(alloc_audio).collect(),
            input_ptrs: vec![std::ptr::null(); inputs.len()],
            output_ptrs: vec![std::ptr::null_mut(); outputs.len()],
            input_widths: inputs.to_vec(),
            output_widths: outputs.to_vec(),
            block_size,
            sample_position: 0.0,
        };
        // Fill the pointer arrays once. The slabs are boxed, so their addresses
        // stay stable across a move of `PushScratch` itself — same property
        // `AuReady::scratch` relies on. Recomputing per block would be correct
        // but pointless work on the RT path.
        for (i, slab) in me.input_slabs.iter_mut().enumerate() {
            me.input_ptrs[i] = slab.as_mut_ptr() as *const AudioBufferList;
        }
        for (i, slab) in me.output_slabs.iter_mut().enumerate() {
            me.output_ptrs[i] = slab.as_mut_ptr() as *mut AudioBufferList;
        }
        me
    }

    /// Frames this scratch was sized for.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// How many input buses this scratch carries.
    pub fn input_bus_count(&self) -> usize {
        self.input_audio.len()
    }

    /// How many output buses this scratch carries.
    pub fn output_bus_count(&self) -> usize {
        self.output_audio.len()
    }

    /// Send this scratch's render cursor back to zero.
    ///
    /// The push twin of the pull path's cursor restart, and it has to be a
    /// separate call because the two cursors are separate objects with separate
    /// owners: the pull cursor lives inside the instance, so
    /// [`AuInstance::reset`](crate::instance::AuInstance::reset) can restart it
    /// directly, while this one is host-owned — the host constructs it and
    /// lends it per render, and `reset` never sees it. A host driving the push
    /// path over a discontinuity therefore calls both, in either order; they
    /// touch disjoint state.
    ///
    /// Not folded into `AuInstance::reset` by having it take an optional
    /// scratch: the two paths deliberately do not share one, for the reason the
    /// module docs give, and a `reset` that silently restarted only the cursor
    /// the caller happened to pass would be a discontinuity applied to half the
    /// render sessions in flight.
    pub fn reset_position(&mut self) {
        self.sample_position = 0.0;
    }

    /// Copy `src` (planar, one slice per channel) into input bus `bus`.
    ///
    /// Extra channels in `src` beyond the bus width are dropped and extra frames
    /// beyond `frames` ignored, matching the pull path's `stage_input`. Channels
    /// and frames the caller does **not** supply are zeroed rather than left
    /// holding the previous block: stale audio in an unfed sidechain channel
    /// makes a compressor duck against a signal that is no longer there, and
    /// that artefact outlives the block that caused it.
    ///
    /// Returns `false` for an out-of-range `bus`, so a caller that miscounts its
    /// buses finds out instead of silently feeding nothing into the render.
    pub fn stage_input(&mut self, bus: usize, src: &[&[f32]], frames: u32) -> bool {
        let Some(channels) = self.input_audio.get_mut(bus) else {
            return false;
        };
        let n = (frames as usize).min(self.block_size as usize);
        for (ch, dst) in channels.iter_mut().enumerate() {
            let limit = n.min(dst.len());
            match src.get(ch) {
                Some(s) => {
                    let len = limit.min(s.len());
                    dst[..len].copy_from_slice(&s[..len]);
                    // A short source is padded, not left stale — same reason an
                    // absent channel is zeroed below.
                    dst[len..limit].fill(0.0);
                }
                None => dst[..limit].fill(0.0),
            }
        }
        true
    }

    /// Copy output bus `bus` out into `dst`.
    ///
    /// Returns `false` for an out-of-range `bus`.
    pub fn emit_output(&self, bus: usize, dst: &mut [&mut [f32]], frames: u32) -> bool {
        let Some(channels) = self.output_audio.get(bus) else {
            return false;
        };
        let n = (frames as usize).min(self.block_size as usize);
        for (ch, out) in dst.iter_mut().enumerate() {
            if let Some(src) = channels.get(ch) {
                let len = n.min(out.len()).min(src.len());
                out[..len].copy_from_slice(&src[..len]);
            }
        }
        true
    }

    /// Point every input bus's `AudioBufferList` at its channel storage.
    ///
    /// # Safety
    /// The slabs and channel vectors are `self`'s own and were sized together in
    /// [`Self::new`], so the [`bind_list`] preconditions hold for every bus.
    /// The pointers written into the lists borrow `self` and are valid only
    /// until the next `&mut self` call.
    unsafe fn bind_inputs(&mut self, frames: u32) {
        for bus in 0..self.input_slabs.len() {
            let width = self.input_widths[bus];
            let ptr = self.input_slabs[bus].as_mut_ptr() as *mut AudioBufferList;
            bind_list(ptr, width, &mut self.input_audio[bus], frames);
        }
    }

    /// Point every output bus's `AudioBufferList` at its channel storage.
    ///
    /// # Safety
    /// As [`Self::bind_inputs`].
    unsafe fn bind_outputs(&mut self, frames: u32) {
        for bus in 0..self.output_slabs.len() {
            let width = self.output_widths[bus];
            let ptr = self.output_slabs[bus].as_mut_ptr() as *mut AudioBufferList;
            bind_list(ptr, width, &mut self.output_audio[bus], frames);
        }
    }

    /// Return the pre-advance sample position and move the cursor on, exactly as
    /// the pull path's `RenderScratch::advance` does — AudioToolbox wants the
    /// block's *start* time, not its end.
    fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }
}

/// Fill one `AudioBufferList` in place from planar channel storage.
///
/// Written here rather than reusing `RenderBufferList::bind` because that type
/// owns its slab and its single width, whereas the push path has one slab per
/// bus with widths that differ per bus. The field math is identical —
/// see `crate::buffer::buffer_list_bytes` for why the slab size is driven by
/// `offset_of!(AudioBufferList, mBuffers)` and not `size_of::<u32>()`, a 4-byte
/// under-allocation that put the last channel's `mData` store past the end.
///
/// # Safety
/// `list` must point at a slab of at least `buffer_list_bytes(width.count())`
/// bytes, aligned for `AudioBufferList`, and `channels` must hold at least
/// `width.count()` entries each at least `frames` long.
unsafe fn bind_list(
    list: *mut AudioBufferList,
    width: ChannelLayout,
    channels: &mut [Vec<f32>],
    frames: u32,
) {
    let count = width.count() as usize;
    (*list).mNumberBuffers = count as u32;
    for (ch, buf) in channels.iter_mut().take(count).enumerate() {
        let audio_buf = &mut *((&mut (*list).mBuffers[0] as *mut AudioBuffer).add(ch));
        audio_buf.mNumberChannels = 1;
        // Multiply in `usize` then narrow, for the reason `RenderBufferList::bind`
        // spells out: `frames * size_of::<f32>() as u32` binds the cast tighter
        // than the multiply, wraps for absurd frame counts, and hands the AU a
        // byte size smaller than the buffer it was given.
        audio_buf.mDataByteSize =
            u32::try_from(frames as usize * std::mem::size_of::<f32>()).unwrap_or(u32::MAX);
        audio_buf.mData = buf.as_mut_ptr() as *mut c_void;
    }
}

/// Push `frames` of audio through the AU with `AudioUnitProcess`.
///
/// The single-buffer-list form: input bus 0 is handed in, and the AU writes its
/// output back over the *same* list — that in-place contract is
/// `AudioUnitProcess`'s own (one `ioData` parameter, named "io" in Apple's
/// header), not a choice made here. So [`PushScratch::stage_input`] must have
/// been called on bus 0 first, and this function copies the AU's result across
/// into output bus 0 itself, so [`PushScratch::emit_output`]`(0, ..)` means the
/// same thing whichever push function produced the audio.
///
/// A unit that reports [`supports_in_place`]` == false` is still driven
/// correctly — the copy out is unconditional, so the host never asks the AU to
/// alias anything. Skipping that copy for units that do support it would be a
/// future optimisation (one memcpy saved) not attempted here, since getting it
/// wrong means an AU reading its own output as input.
///
/// ## Measured coverage
///
/// On macOS 15.6, 6 of the 9 corpus effects implement this selector: AUDelay,
/// AUDynamicsProcessor, AUDistortion, AUNBandEQ, AULowpass, AUSampleDelay and
/// AUMultibandCompressor answer `noErr` and produce audio. AUMatrixReverb,
/// AUReverb2, both instruments and every mixer answer `unimpErr` (`-4`) — the
/// component manager's "selector not implemented" — as do both third-party units
/// installed. Where it does work it is **exact**: AUDelay's push output matched
/// its `AudioUnitRender` output to the last bit across 8 blocks of a 440 Hz sine
/// (peak `0.35350975`, `0.35353398`, … identical in both paths).
///
/// # Errors
/// * [`AuError::InvalidBuffer`] if `frames` exceeds the scratch's block size, or
///   if the scratch has no input bus 0 / no output bus 0. The frame bound is
///   checked here rather than left to the AU because the host owns the buffer:
///   `bind_list` would report `frames * 4` bytes for storage sized to
///   `block_size`, and the AU would write past it. (The AU *also* refuses —
///   measured `-10874`, `kAudioUnitErr_TooManyFramesToProcess`, for a 256-frame
///   render on a 64-frame AUDelay — but only after being handed a buffer list
///   that lies about its size.)
/// * [`AuError::RenderFailed`] with the AU's `LastRenderError` attached when
///   `AudioUnitProcess` itself fails, which for most units means `unimpErr`.
///
/// # Safety
/// `unit` must be a live, **initialized** `AudioUnit` whose configured maximum
/// block size is at least `frames`.
pub unsafe fn process_push(
    unit: AudioUnit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    if frames > scratch.block_size {
        return Err(AuError::InvalidBuffer(format!(
            "frames ({frames}) > block_size ({})",
            scratch.block_size
        )));
    }
    if scratch.input_audio.is_empty() || scratch.output_audio.is_empty() {
        return Err(AuError::InvalidBuffer(
            "AudioUnitProcess needs one input bus and one output bus".to_string(),
        ));
    }

    scratch.bind_inputs(frames);
    let list = scratch.input_slabs[0].as_mut_ptr() as *mut AudioBufferList;
    let timestamp = AudioTimeStamp::with_sample_time(scratch.advance(frames));
    let mut flags: AudioUnitRenderActionFlags = 0;

    let status = AudioUnitProcess(unit, &mut flags, &timestamp, frames, list);
    if status != NO_ERR {
        return Err(AuError::render_failed(
            "AudioUnitProcess",
            status,
            last_render_error(unit),
        ));
    }

    // `OutputIsSilence` is honoured for the reason `AuReady::process` documents:
    // when the AU sets it, the buffer contents are explicitly *not* guaranteed
    // zeroed — the flag is how an AU says "I produced nothing, don't trust what
    // is in there" — so trusting them emits whatever the last block left.
    let silent = flags & K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE != 0;
    copy_in_bus0_to_out_bus0(scratch, silent, frames);
    Ok(flags)
}

/// Copy input bus 0 into output bus 0 (or zero it), without allocating.
///
/// Split out of [`process_push`] only so the two borrows do not overlap: source
/// and destination live in different fields of the same struct, which index
/// expressions inline cannot express to the borrow checker.
fn copy_in_bus0_to_out_bus0(scratch: &mut PushScratch, silent: bool, frames: u32) {
    let n = (frames as usize).min(scratch.block_size as usize);
    let Some(src) = scratch.input_audio.first() else {
        return;
    };
    let Some(dst) = scratch.output_audio.first_mut() else {
        return;
    };
    for (ch, out) in dst.iter_mut().enumerate() {
        let limit = n.min(out.len());
        match src.get(ch) {
            Some(s) if !silent => {
                let len = limit.min(s.len());
                out[..len].copy_from_slice(&s[..len]);
                out[len..limit].fill(0.0);
            }
            _ => out[..limit].fill(0.0),
        }
    }
}

/// Push `frames` through the AU with `AudioUnitProcessMultiple` — the sidechain
/// call.
///
/// Every input bus staged into `scratch` is handed to the AU at once and every
/// output bus written separately, so input and output do **not** alias. This is
/// the only AUv2 call that can deliver a second input bus in the same render —
/// what a sidechain is: bus 0 the signal, bus 1 the key the compressor listens
/// to.
///
/// ## It does not work on anything installed, and here are the numbers
///
/// Measured on macOS 15.6 against every Apple effect, instrument and mixer in
/// the corpus plus both third-party units:
///
/// * **`unimpErr` (`-4`) from all but one.** AUDelay, AUDynamicsProcessor,
///   AUDistortion, AUMatrixReverb, AUNBandEQ, AULowpass, AUSampleDelay,
///   AUMultibandCompressor, AUSampler, DLSMusicDevice, AUMultiChannelMixer, TDR
///   Nova and TAL-Reverb-4 all answer `-4` — the component manager's reply when
///   an AU does not implement `kAudioUnitProcessMultipleSelect`.
/// * **AUReverb2 implements it, for exactly one input list.** One list renders
///   correctly and stably: 8 blocks of a 440 Hz sine at 0.5 in came out at peak
///   `0.4974…`, finite throughout, with `mDataByteSize` reported as the full 256
///   bytes each block. Two input lists are refused with `-10877`
///   (`kAudioUnitErr_InvalidElement`), and AUReverb2 has one input element, so
///   that refusal is correct rather than a bug.
/// * **A unit with 8 real input elements still refuses.** AUMultiChannelMixer
///   reports 8 input buses via `kAudioUnitProperty_ElementCount`, and answers
///   `-4` to 1, 2 and 8 input lists alike. So the absence is not about element
///   counts — the selector simply is not implemented.
///
/// **There is therefore no working AU sidechain on this machine.** This wrapper
/// is kept because it is the only way to reach the contract at all, it is
/// exercised by `tests/au_offline.rs` against the one unit that answers, and the
/// day a plugin implements the selector the host needs no new code. Nothing in
/// tutti should route a sidechain through it expecting audio to come back.
///
/// # Errors
/// * [`AuError::InvalidBuffer`] if `frames` exceeds the scratch's block size, or
///   if the scratch has no output bus at all — an AU must write somewhere, and a
///   zero-length output array is not a render.
/// * [`AuError::RenderFailed`] carrying the AU's `LastRenderError`. An AU handed
///   more input lists than it has input elements reports
///   `kAudioUnitErr_InvalidElement` here, and one that does not implement the
///   selector reports `unimpErr`. Both surface rather than being absorbed: a
///   silently-dropped sidechain is a compressor that simply never ducks, which
///   is indistinguishable from one whose threshold is set too high.
///
/// # Safety
/// `unit` must be a live, **initialized** `AudioUnit` whose configured maximum
/// block size is at least `frames`, and whose input/output element counts are
/// at least the bus counts `scratch` was built with.
pub unsafe fn process_push_multiple(
    unit: AudioUnit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    if frames > scratch.block_size {
        return Err(AuError::InvalidBuffer(format!(
            "frames ({frames}) > block_size ({})",
            scratch.block_size
        )));
    }
    if scratch.output_audio.is_empty() {
        return Err(AuError::InvalidBuffer(
            "AudioUnitProcessMultiple needs at least one output bus".to_string(),
        ));
    }

    scratch.bind_inputs(frames);
    scratch.bind_outputs(frames);
    let timestamp = AudioTimeStamp::with_sample_time(scratch.advance(frames));
    let mut flags: AudioUnitRenderActionFlags = 0;

    // The pointer arrays are fields, pre-filled in `new`, so `as_mut_ptr` here
    // is a field read and nothing on this path allocates.
    let n_in = scratch.input_ptrs.len() as u32;
    let n_out = scratch.output_ptrs.len() as u32;
    let status = AudioUnitProcessMultiple(
        unit,
        &mut flags,
        &timestamp,
        frames,
        n_in,
        scratch.input_ptrs.as_mut_ptr(),
        n_out,
        scratch.output_ptrs.as_mut_ptr(),
    );
    if status != NO_ERR {
        return Err(AuError::render_failed(
            "AudioUnitProcessMultiple",
            status,
            last_render_error(unit),
        ));
    }

    // As in `process_push`: an AU that flags the block silent has not promised
    // its output buffers are zeroed, so zero them rather than emit stale audio.
    if flags & K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE != 0 {
        let n = (frames as usize).min(scratch.block_size as usize);
        for bus in scratch.output_audio.iter_mut() {
            for ch in bus.iter_mut() {
                let len = n.min(ch.len());
                ch[..len].fill(0.0);
            }
        }
    }
    Ok(flags)
}

/// The AU's `kAudioUnitProperty_LastRenderError`, when it is readable and
/// non-`noErr`.
///
/// Only called on a *failed* render, exactly as the equivalent read in
/// `AuReady::process` is, so the property read it performs cannot affect the
/// no-alloc guarantee on the steady-state path.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
unsafe fn last_render_error(unit: AudioUnit) -> Option<OSStatus> {
    get_property::<OSStatus>(
        unit,
        K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )
    .ok()
    .filter(|&e| e != NO_ERR)
}

/// Every `mDataByteSize` the AU left on the output buses of the last push
/// render.
///
/// Exposed for the test suite rather than for hosts: how `tests/au_offline.rs`
/// observes that the AU wrote for the frame count it was asked for, without
/// reimplementing the `AudioBufferList` walk. An AU rendering fewer frames than
/// requested reports the shortfall here and nowhere else — sample values alone
/// can't distinguish "wrote 32 frames" from "wrote 64, half near zero".
///
/// # Safety
/// Reads the slabs `scratch` owns, which were sized in `new` and whose
/// `mNumberBuffers` is set by every `bind_outputs`. Sound for any `scratch`
/// whose `bind_outputs` has run at least once; on a fresh scratch the slabs are
/// zeroed, so the walk reports zero buffers rather than reading garbage.
pub unsafe fn output_byte_sizes(scratch: &PushScratch) -> Vec<u32> {
    let mut sizes = Vec::new();
    for slab in scratch.output_slabs.iter() {
        let ptr = slab.as_ptr() as *mut AudioBufferList;
        sizes.extend(iter_buffers_mut(ptr).map(|b| b.mDataByteSize));
    }
    sizes
}
