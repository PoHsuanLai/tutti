//! Bounce-time and buffer-strategy properties, plus the push-model render path.
//!
//! Four AUv2 facilities that a live-only host never needs and an exporting host
//! cannot do without. They are grouped here rather than added to
//! [`crate::stream`] because they share one duty: they describe how the host
//! intends to *drive* the render, as opposed to what the stream looks like
//! ([`crate::stream`]) or what the topology is ([`crate::bus`]).
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
//! `AudioUnitProcessMultiple` is the call that takes several input buffer lists,
//! and it is the only AUv2 way a sidechain reaches a compressor. **It is not
//! usable against anything installed on this machine**, and the numbers are in
//! [`process_push_multiple`]'s docs: every unit measured answers `unimpErr`
//! (`-4`, the component manager's "this selector is not implemented") except
//! AUReverb2, which implements it but accepts exactly **one** input list and
//! rejects two with `kAudioUnitErr_InvalidElement`. Both third-party units on
//! the system (TDR Nova, TAL-Reverb-4) answer `-4` for `AudioUnitProcess` *and*
//! `ProcessMultiple`.
//!
//! So [`process_push_multiple`] exists as a thin, tested wrapper — it is the
//! only way to reach the sidechain contract at all, and the day an AU implements
//! it the host needs no new code — but nothing in tutti should route a sidechain
//! through it expecting it to work. The single-list [`process_push`] is the
//! feature that is real: it is measured bit-identical to `AudioUnitRender` on
//! AUDelay across 8 blocks (`0.35350975` peak from both paths, every block).
//!
//! ## Why the push path is a separate module and not a `process` variant
//!
//! [`AuReady::process`](crate::instance::AuReady::process) is a *pull* render:
//! it stages input into the heap-pinned `RenderScratch`, then `AudioUnitRender`
//! calls back into that scratch to fetch it. The push path never installs a
//! callback and never wants one — with `AudioUnitProcess` the `ioData` buffer
//! list carries the input **in** and the output back **out** in the same call.
//! Those are two different contracts on the same unit, and the scratch each
//! needs is shaped differently (the push path needs one list per bus, the pull
//! path exactly one). Keeping them apart is what stops the pull path's callback
//! from firing during a push render and overwriting the input the host just
//! handed in.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use tutti_plugin_types::ChannelLayout;

use crate::buffer::{buffer_list_bytes, iter_buffers_mut};
use crate::error::{AuError, Result};
use crate::ffi::{get_property, set_property};
use crate::types::*;

/// `kAudioUnitProperty_OfflineRender` (37).
///
/// Aliased here rather than in [`crate::types`] with its siblings because
/// nothing on the base branch referenced it; keeping the id beside its only two
/// call sites means the property number and the code that interprets the AU's
/// answer cannot drift apart.
pub(crate) const K_AUDIO_UNIT_PROPERTY_OFFLINE_RENDER: u32 =
    coreaudio_sys::kAudioUnitProperty_OfflineRender;

/// The inclusive maximum this host will write to
/// `kAudioUnitProperty_RenderQuality`.
///
/// 127 — the range Apple's `AudioUnitProperties.h` documents for the property
/// (`kRenderQuality_Max`). Enforced host-side, and the measurement is why:
/// of the four Apple units on macOS 15.6 that implement the property, **only
/// AUDistortion actually rejects an out-of-range value** (`-50`, `paramErr`, for
/// anything above 127). AUMatrixReverb, DLSMusicDevice and AUMultiChannelMixer
/// all accept a write of `999` with `noErr` **and read `999` back**; the mixer
/// accepts and returns `u32::MAX`. So the AU neither rejects nor normalizes the
/// nonsense — it stores it — and a host that trusted the `noErr` would go on
/// displaying "quality: 999" out of a 0–127 control forever, with no way to
/// discover the number is meaningless. See [`set_render_quality`].
pub const RENDER_QUALITY_MAX: u32 = 127;

/// Whether the AU has been told it is rendering offline.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_OfflineRender`, which on macOS 15.6 is **every Apple
/// effect and mixer** — only the instruments (AUSampler, DLSMusicDevice)
/// implement it at all. Propagated rather than flattened to `false`, because the
/// two are different facts a bouncing host must distinguish: "this AU is in
/// real-time mode" is something the host can change, while "this AU has no
/// offline mode" means the exported file may differ from what was auditioned and
/// there is nothing the host can do about it. Absorbing the refusal into `false`
/// makes the second case unreportable — and given how few units implement the
/// property, that second case is the common one.
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
/// The property does not change *what* the host does — the render calls are
/// identical either way. It changes what the AU is permitted to do, and Apple's
/// header names the case outright: an AU "that normally operates within a
/// general real-time calling model" may behave differently once it knows the
/// result is going to a file rather than to a speaker. Three concrete
/// divergences a host that never sets it will ship:
///
/// * **Dropout protection stays armed.** A real-time AU that detects it is
///   running out of time may drop to a cheaper algorithm, or emit the previous
///   block again, rather than miss its deadline. During a bounce there is no
///   deadline — the host renders as fast as the CPU allows and a "late" block is
///   not late — so that protection is pure quality loss, and it fires precisely
///   on the heaviest parts of the mix.
/// * **Cheap resampling.** An AU that resamples internally (pitch shift,
///   time-stretch, any oversampled saturator) will choose a short interpolation
///   kernel in real time and a long one offline. The bounce then carries more
///   aliasing than the monitor path did.
/// * **Non-deterministic dither.** An AU dithering its output may seed from the
///   clock in real time and from a fixed seed offline. Without the flag, two
///   bounces of the same project are not bit-identical, which breaks every
///   downstream check a mastering workflow makes — and makes the host's own
///   render tests unable to compare two files at all.
///
/// The symptom users report is "the export doesn't sound like the mix": quieter
/// transients, or brighter, or simply different every time, with nothing in the
/// project changed.
///
/// ## What was measured, so the expectation is calibrated
///
/// On macOS 15.6 the only units that implement the property are the two
/// instruments, and neither audibly changes: AUSampler and DLSMusicDevice render
/// the same peak (`0.246602` and `0.095300` respectively, over 16 blocks of a
/// held middle C) whether the flag is set or clear. So setting it buys nothing
/// *on this corpus* — the units that would use it are the third-party
/// resampling and dithering plugins the property was designed for, and TDR Nova
/// and TAL-Reverb-4 both expose it (both read `0` by default) without this host
/// having a way to prove they honour it. The value of the API is that a host
/// which never sets the flag cannot benefit even from a plugin that does.
///
/// ## Set it before `initialize`
///
/// Legal in either state — every unit that implements it accepts the write
/// initialized or not, measured — but an AU that sizes an internal oversampling
/// buffer from the flag can only do so at `AudioUnitInitialize`. Setting it
/// afterwards is therefore accepted and may still not take effect, which is a
/// silent partial success. A bouncing host should set the flag, then
/// `initialize`, the same way it orders `MaximumFramesPerSlice`.
///
/// ## The width is 4 bytes, and unlike bypass a narrower write also works
///
/// `AudioUnitGetPropertyInfo` reports size 4 and the value is written as a
/// `UInt32`, matching the header's declared type. Worth stating explicitly
/// because [`AuInstance::set_bypass`](crate::instance::AuInstance::set_bypass)
/// documents the opposite finding for `BypassEffect` — a 1-byte write there is
/// refused with `-10851`. Measured here: a 1-byte write to `OfflineRender` is
/// accepted with `noErr` by both instruments. The `u32` is used anyway, because
/// it is what the header declares and the leniency is one unit's implementation
/// detail rather than a contract.
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
/// let the AU overwrite it, saving a copy per block per plugin. On a 64-plugin
/// session at 48 kHz / 64-frame blocks that is ~48 000 stereo copies a second
/// the host does not make.
///
/// Measured on macOS 15.6: six Apple effects advertise it and all six report
/// **1** (capable) — AUDelay, AUDynamicsProcessor, AUDistortion, AULowpass,
/// AUSampleDelay, AUMultibandCompressor. AUMatrixReverb, AUReverb2 and AUNBandEQ
/// do not implement the property, nor does any instrument or mixer, nor either
/// third-party unit. No unit on the system reports `0`.
///
/// ## Read-only, deliberately, even though the property is writable
///
/// Apple declares this Read/**Write** and `GetPropertyInfo` confirms the
/// writable bit; the write direction has a real meaning, namely that a host
/// whose buffer management would be *defeated* by in-place operation can set `0`
/// to forbid it. This crate exposes only the read, and the reason is that tutti
/// has no such strategy to defend. Writing `0` can only make the AU do more work
/// — it is a request to be *less* efficient — and it is correct only for a host
/// that has already committed to holding the pre-effect signal (a dry/wet mix
/// computed outside the plugin, a look-ahead peek at the unprocessed block).
/// Nothing here does. A setter would offer callers a knob whose only available
/// setting is the pessimal one, and whose correct setting depends on a host
/// invariant this crate cannot see.
///
/// The read, by contrast, is load-bearing: it is the difference between
/// [`process_push`] being allowed to alias its buffers and having to copy.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_InPlaceProcessing`. Not flattened to `false`, and the
/// distinction here is sharper than a diagnostic: `false` means "the AU says no,
/// do not alias", a refusal means "the AU did not say". Both lead the host to
/// copy, but only the first is a fact about the AU, and a host that recorded the
/// refusal as `false` would go on reporting that AUs *forbid* in-place operation
/// when they merely never mentioned it. Since no unit measured actually reports
/// `0`, flattening would make the host's in-place census read "9 units forbid
/// it" when the truth is "6 permit it and 6 never said".
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
/// Distinct from [`set_offline_render`], and the two are not substitutes. The
/// offline flag says *why* the host is rendering and leaves the AU to choose;
/// this says what the host wants regardless of context. A bounce sets both —
/// offline so deterministic paths engage, and quality at maximum so an AU that
/// exposes the knob rather than inferring from the flag also cooperates.
///
/// ## Why the range is enforced here rather than left to the AU
///
/// Because three of the four units that implement the property do not enforce it
/// and do not clamp. Measured on macOS 15.6:
///
/// | unit | write 128 | write 999 | write `u32::MAX` |
/// |---|---|---|---|
/// | AUDistortion | `-50`, reads 127 | `-50`, reads 127 | `-50`, reads 127 |
/// | AUMatrixReverb | `noErr`, reads **128** | `noErr`, reads **999** | `noErr`, reads 127 |
/// | DLSMusicDevice | `noErr`, reads **128** | `noErr`, reads **999** | `noErr`, reads 127 |
/// | AUMultiChannelMixer | `noErr`, reads 128 | `noErr`, reads 999 | `noErr`, reads **`u32::MAX`** |
///
/// Only AUDistortion behaves the way a host would hope, refusing with `paramErr`
/// and keeping its previous value. The other three *store the nonsense and hand
/// it back*, so a host that trusted `noErr` would display "quality: 999" out of a
/// 0–127 control indefinitely, and could not tell that apart from a legitimate
/// setting by reading the property. Nothing about the AU's later behaviour
/// reveals it either.
///
/// Rejecting host-side turns that silent acceptance into a visible
/// [`AuError::InvalidBuffer`]: the caller is told its number was out of range
/// instead of quietly keeping it. The `u32::MAX` row is what rules out the
/// alternative of "write it and read it back to check" — AUMultiChannelMixer
/// round-trips that value faithfully, so a read-back verifier would accept it.
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
/// [`process_push_multiple`] can bind and render without touching the allocator.
/// That is the whole reason this type exists rather than the buffer lists being
/// built per call: the push calls run on the audio thread, and a `Vec` grown per
/// block is an allocator acquisition every 1.3 ms at 48 kHz / 64 frames.
///
/// ## Why the fields are `Vec` and the render path still does not allocate
///
/// Every `Vec` here is grown exactly once, in [`PushScratch::new`]. The render
/// methods only write through existing storage: `bind_*` rewrites the `mData` /
/// `mDataByteSize` fields of `AudioBuffer`s that already exist, and the pointer
/// arrays are `Vec`s whose length never changes, so `as_mut_ptr` on them is a
/// field read. `tests/au_offline.rs` is what proves it — the property is not
/// something the types can express.
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
    /// Monotonic sample cursor for the render timestamp, mirroring the pull
    /// path's. The push path keeps its **own** rather than sharing: they are
    /// separate render sessions, and one shared cursor would make the timestamps
    /// of whichever path was not driving jump forward, which an AU whose internal
    /// LFO is phased off `mSampleTime` hears as a discontinuity.
    sample_position: f64,
}

impl PushScratch {
    /// Allocate for the given per-bus input and output widths.
    ///
    /// `inputs` and `outputs` are one entry per bus, in bus order — so a
    /// hypothetical sidechain compressor is `&[Stereo, Stereo]` in, `&[Stereo]`
    /// out. Size these from [`AuInstance::bus_count`](crate::instance::AuInstance::bus_count),
    /// never from what the host wishes were there: an AU handed more input lists
    /// than it has input elements rejects the render outright (measured:
    /// AUReverb2 answers `kAudioUnitErr_InvalidElement` for a second list), it
    /// does not ignore the extra.
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
        // are stable for the lifetime of `self` even as `PushScratch` itself is
        // moved — the same property `AuReady::scratch` relies on. Recomputing
        // them per block would be correct but pointless work on the RT path.
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
/// bus and the widths differ per bus. The field math is identical and is the part
/// that matters — see `crate::buffer::buffer_list_bytes` for why the slab size is
/// driven by `offset_of!(AudioBufferList, mBuffers)` and not `size_of::<u32>()`,
/// a 4-byte under-allocation that put the last channel's `mData` store past the
/// end.
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
/// output back over the *same* list. That in-place contract is
/// `AudioUnitProcess`'s own, not a choice made here — the API has one `ioData`
/// parameter and Apple's header names it "io". So
/// [`PushScratch::stage_input`] must have been called on bus 0 first, and this
/// function copies the AU's result across into output bus 0 itself, so that
/// [`PushScratch::emit_output`]`(0, ..)` means the same thing whichever push
/// function produced the audio. A caller should not have to know that one form
/// aliases and the other does not.
///
/// A unit that reports [`supports_in_place`]` == false` is still driven
/// correctly, because the host never asks the AU to alias anything: the copy out
/// is unconditional. The in-place *read* is what a future optimisation would use
/// to skip that copy, and it is deliberately not skipped here — the saving is one
/// memcpy and the cost of getting it wrong is an AU reading its own output as
/// input.
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
/// the only AUv2 call that can deliver a second input bus in the same render,
/// which is what a sidechain is: bus 0 is the signal, bus 1 the key the
/// compressor listens to.
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
/// Exposed for the test suite rather than for hosts: it is how
/// `tests/au_offline.rs` observes that the AU wrote for the frame count it was
/// asked for, without the test reimplementing the `AudioBufferList` walk. An AU
/// that renders fewer frames than requested reports the shortfall here and
/// nowhere else — the sample values alone cannot distinguish "wrote 32 frames of
/// audio" from "wrote 64 frames, 32 of which happened to be near zero".
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
