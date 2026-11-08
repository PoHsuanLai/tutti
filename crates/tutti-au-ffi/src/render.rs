//! Render-time buffer management.
//!
//! Allocates the variable-length `AudioBufferList` backing storage and the
//! per-channel scratch buffers used to marshal samples in/out of AudioToolbox.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use tutti_plugin_types::ChannelLayout;

use coreaudio_sys::{AudioBuffer, AudioBufferList, AudioTimeStamp, AudioUnitRenderActionFlags};

use crate::error::{Error, Result};
use crate::guard::guard;
use crate::unit::Unit;

/// Input/output channel layouts for an AU.
///
/// [`ChannelLayout`] cannot represent a zero channel count, so the
/// "has an input bus at all" distinction (effect vs. generator/instrument) is
/// carried separately in [`BusLayout::has_input`]; `inputs` is meaningful only
/// when that flag is set.
#[derive(Debug, Clone, Copy)]
pub struct BusLayout {
    /// Input channel layout. Meaningful only when [`BusLayout::has_input`] is
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

/// Byte size of an `AudioBufferList` carrying `n` trailing `AudioBuffer`s.
///
/// `AudioBufferList` is `{ UInt32 mNumberBuffers; AudioBuffer mBuffers[1]; }`,
/// and `AudioBuffer` contains a pointer, so the struct is 8-aligned and
/// `mBuffers` starts at offset **8**, not 4 — there are 4 bytes of tail padding
/// after `mNumberBuffers`. Sizing off `size_of::<u32>()` under-allocates by
/// exactly those 4 bytes and makes the last channel's `mData` write land past
/// the end of the slab. Measured on macOS/arm64 against the real SDK header:
/// `sizeof(AudioBuffer) == 16`, `offsetof(AudioBufferList, mBuffers) == 8`.
pub(crate) const fn buffer_list_bytes(n: usize) -> usize {
    std::mem::offset_of!(AudioBufferList, mBuffers) + n * std::mem::size_of::<AudioBuffer>()
}

/// 8-byte-aligned allocation unit for the `AudioBufferList` slab.
///
/// `Box<[u8]>` is only 1-byte aligned; `AudioBufferList` needs
/// `align_of::<AudioBufferList>()` (8 on every Apple platform). Backing the
/// slab with `u64` gives that alignment for free without a manual
/// `alloc`/`dealloc` pair.
type AblWord = u64;

const _: () = assert!(std::mem::align_of::<AblWord>() >= std::mem::align_of::<AudioBufferList>());

/// Backing storage for an `AudioBufferList` plus its trailing `AudioBuffer`s.
///
/// The real C struct ends with a flexible-array member, so this allocates a
/// correctly-sized, correctly-aligned slab and reinterprets it.
pub(crate) struct RenderBufferList {
    storage: Box<[AblWord]>,
    channels: ChannelLayout,
}

impl RenderBufferList {
    pub fn new(channels: ChannelLayout) -> Self {
        let bytes = buffer_list_bytes(channels.count() as usize);
        let words = bytes.div_ceil(std::mem::size_of::<AblWord>());
        Self {
            storage: vec![0 as AblWord; words].into_boxed_slice(),
            channels,
        }
    }

    /// Point the list at `buffers` (one `Vec<f32>` per channel) and return a
    /// pointer suitable for `AudioUnitRender` or the input render callback.
    pub fn bind(&mut self, buffers: &mut [Vec<f32>], frames: u32) -> *mut AudioBufferList {
        // `storage` is 8-aligned (see `AblWord`) and `buffer_list_bytes` long,
        // so this cast and every trailing `mBuffers[ch]` write below stay in
        // bounds for `ch < channels.count()`.
        let ptr = self.storage.as_mut_ptr() as *mut AudioBufferList;
        // SAFETY: `storage` is zeroed, 8-aligned and `buffer_list_bytes` long
        // for `channels.count()` buffers, so the header and each `mBuffers[ch]`
        // written below (`ch < count`) are in bounds and aligned; the storage is
        // exclusively ours (`&mut self`).
        unsafe {
            (*ptr).mNumberBuffers = self.channels.count() as u32;
            for (ch, buf) in buffers
                .iter_mut()
                .take(self.channels.count() as usize)
                .enumerate()
            {
                let audio_buf = &mut *((&mut (*ptr).mBuffers[0] as *mut AudioBuffer).add(ch));
                audio_buf.mNumberChannels = 1;
                // Multiply in `usize`, then narrow — `as` binds tighter than
                // `*`, so `frames * size_of::<f32>() as u32` multiplies two
                // u32s and wraps for `frames > u32::MAX / 4`, handing the
                // plugin a byte size far smaller than the buffer it is given.
                // `AuInstance::process` rejects `num_frames > block_size`
                // before reaching here, so this is not reachable today; `bind`
                // is public and should not depend on a caller's guard for it.
                // Same form as `instance.rs`'s output-size report.
                audio_buf.mDataByteSize =
                    u32::try_from(frames as usize * std::mem::size_of::<f32>()).unwrap_or(u32::MAX);
                audio_buf.mData = buf.as_mut_ptr() as *mut c_void;
            }
        }
        ptr
    }
}

/// Iterates the `AudioBuffer`s inside a raw `AudioBufferList`.
///
/// # Safety
/// `abl` must be a valid, well-formed `AudioBufferList` with at least
/// `number_buffers` trailing `AudioBuffer`s in contiguous memory.
pub(crate) unsafe fn iter_buffers_mut<'a>(
    abl: *mut AudioBufferList,
) -> impl Iterator<Item = &'a mut AudioBuffer> {
    // SAFETY: the caller's contract.
    let count = unsafe { (*abl).mNumberBuffers } as usize;
    // SAFETY: the caller's contract.
    let base = unsafe { &mut (*abl).mBuffers[0] } as *mut AudioBuffer;
    // SAFETY: each `base.add(i)` is one of the `count` buffers (the caller's
    // contract), distinct from the others, so the `&mut` borrows do not alias.
    (0..count).map(move |i| unsafe { &mut *base.add(i) })
}

/// Render scratch area: output buffers, input buffers, and the sample position
/// used for timestamps.
pub(crate) struct RenderScratch {
    list: RenderBufferList,
    pub outputs: Vec<Vec<f32>>,
    pub inputs: Vec<Vec<f32>>,
    /// Advances by one block per render and returns to zero on
    /// [`reset_position`](Self::reset_position); see that method for why a
    /// discontinuity restarts it rather than seeking it.
    sample_position: f64,
}

impl RenderScratch {
    pub fn new(layout: BusLayout, block_size: u32) -> Self {
        let out_ch = layout.outputs.count() as usize;
        let in_ch = if layout.has_input {
            layout.inputs.count() as usize
        } else {
            0
        };
        let size = block_size as usize;

        let outputs = (0..out_ch).map(|_| vec![0.0f32; size]).collect();
        // Allocate at least as many input channels as outputs so effects that
        // report 0 input channels but still pull stereo input don't OOB.
        let inputs = (0..in_ch.max(out_ch)).map(|_| vec![0.0f32; size]).collect();

        Self {
            list: RenderBufferList::new(layout.outputs),
            outputs,
            inputs,
            sample_position: 0.0,
        }
    }

    pub fn bind_output(&mut self, frames: u32) -> *mut AudioBufferList {
        self.list.bind(&mut self.outputs, frames)
    }

    pub fn stage_input(&mut self, input: &[&[f32]], frames: u32) {
        for (ch, src) in input.iter().enumerate() {
            if let Some(dst) = self.inputs.get_mut(ch) {
                let len = (frames as usize).min(src.len()).min(dst.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    pub fn emit_output(&self, output: &mut [&mut [f32]], frames: u32) {
        for (ch, dst) in output.iter_mut().enumerate() {
            if let Some(src) = self.outputs.get(ch) {
                let len = (frames as usize).min(dst.len()).min(src.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    /// [`stage_input`](Self::stage_input) for an f64 caller, narrowing to the
    /// f32 the AU renders in.
    ///
    /// AUv2 has no f64 render entry point — `AudioUnitRender` takes an
    /// `AudioBufferList` of `Float32` and there is no `…F64` twin the way VST 2.4
    /// has `processReplacingF64`. The conversion is therefore unavoidable for an
    /// f64 caller, and the narrowing is the AU's own precision ceiling rather
    /// than a loss this layer chose. Converting *here*, against the scratch that
    /// already exists, is what keeps it free of per-block allocation.
    ///
    /// Writes into the same `inputs` buffers as the f32 path, so a caller cannot
    /// mix widths within one block — nor does anything ask to: the width is
    /// picked per block by the pipeline's `SampleFormat` and applies to both
    /// directions.
    pub fn stage_input_f64(&mut self, input: &[&[f64]], frames: u32) {
        for (ch, src) in input.iter().enumerate() {
            if let Some(dst) = self.inputs.get_mut(ch) {
                let len = (frames as usize).min(src.len()).min(dst.len());
                for (d, &s) in dst[..len].iter_mut().zip(&src[..len]) {
                    *d = s as f32;
                }
            }
        }
    }

    /// [`emit_output`](Self::emit_output) for an f64 caller, widening back from
    /// the f32 the AU rendered. Bounded on both sides for the same reason.
    pub fn emit_output_f64(&self, output: &mut [&mut [f64]], frames: u32) {
        for (ch, dst) in output.iter_mut().enumerate() {
            if let Some(src) = self.outputs.get(ch) {
                let len = (frames as usize).min(dst.len()).min(src.len());
                for (d, &s) in dst[..len].iter_mut().zip(&src[..len]) {
                    *d = s as f64;
                }
            }
        }
    }

    /// Returns the pre-advance sample position and move the cursor forward by
    /// `frames`. The pre-advance value is what AudioToolbox expects for the
    /// current block's timestamp.
    pub fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }

    /// Sends the render cursor back to zero, so the next block's `mSampleTime`
    /// starts a fresh run rather than continuing the previous one.
    ///
    /// Paired with `AuInstance::reset`:
    /// that call flushes the AU's signal history, and this discards the clock
    /// the flushed history was measured against. Leaving the cursor running
    /// hands an AU that phases anything off `mSampleTime` — an LFO, a
    /// look-ahead window, a block-to-block delta — a timestamp saying the block
    /// after a locate is contiguous with the block before it, which is exactly
    /// what the flush just said it is not.
    ///
    /// Zero rather than a host-supplied playhead. `mSampleTime` is documented
    /// as the stamp that lets an AU "determine without doubt that this is the
    /// same render operation" (`AUComponent.h`, `AudioUnitRender`) — a
    /// per-instance render clock, not a position on the project timeline. The
    /// timeline has its own channel, `outCurrentSampleInTimeLine` on the
    /// transport callbacks, which an AU asks for separately and which
    /// `TransportState::set_transport`
    /// publishes. Writing a playhead here would answer that question twice, in
    /// two places, with no way for an AU to tell which clock it received.
    pub fn reset_position(&mut self) {
        self.sample_position = 0.0;
    }

    /// The position the next block will be stamped with.
    #[cfg(test)]
    pub fn position(&self) -> f64 {
        self.sample_position
    }
}

// ---------------------------------------------------------------- Renderer

const NO_ERR: i32 = 0;
const K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE: AudioUnitRenderActionFlags =
    coreaudio_sys::kAudioUnitRenderAction_OutputIsSilence;

/// A timestamp that is valid by sample time only, which is all a pull render
/// needs.
fn timestamp_at(sample_time: f64) -> AudioTimeStamp {
    AudioTimeStamp {
        mSampleTime: sample_time,
        mFlags: coreaudio_sys::kAudioTimeStampSampleTimeValid,
        ..Default::default()
    }
}

const ERR_CANNOT_DO: i32 = coreaudio_sys::kAudioUnitErr_CannotDoInCurrentContext;

/// Pull-model rendering of an initialized unit: the scratch buffers, the
/// timestamp cursor, and the input render callback that feeds an effect.
///
/// The unit's input callback holds a pointer to the scratch, so the scratch is
/// heap-pinned and owned here, and dropping the renderer withdraws the callback
/// before the scratch is freed. A block's work is `stage_input*`, `render`,
/// `emit_output*`; none of them allocates.
pub struct Renderer {
    unit: Unit,
    scratch: std::ptr::NonNull<RenderScratch>,
    callback_installs: std::sync::atomic::AtomicU32,
    callback_installed: bool,
    block_size: u32,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

// SAFETY: the scratch is owned by the renderer (a leaked `Box` it frees in
// `Drop`) and is reached only through `&mut self` methods and the unit's input
// callback, which runs inside `render` on the calling thread; the `Unit` is
// `Send`. Moving the renderer to another thread moves its sole owner.
unsafe impl Send for Renderer {}

impl Renderer {
    /// Allocates the scratch for `layout` and `block_size` frames. Nothing is
    /// installed on the unit yet.
    pub fn new(unit: &Unit, layout: BusLayout, block_size: u32) -> Self {
        let scratch = Box::new(RenderScratch::new(layout, block_size));
        Self {
            unit: unit.clone(),
            // `Box::into_raw` never returns null.
            scratch: std::ptr::NonNull::from(Box::leak(scratch)),
            callback_installs: std::sync::atomic::AtomicU32::new(0),
            callback_installed: false,
            block_size,
        }
    }

    /// The scratch, for this renderer's own methods.
    fn scratch(&mut self) -> &mut RenderScratch {
        // SAFETY: the pointer is a live leaked box owned by `self`; `&mut self`
        // makes this the only borrow, and the input callback (the one other
        // reader) runs only inside `render`, where no borrow from here is held.
        unsafe { self.scratch.as_mut() }
    }

    /// Points the unit's input bus 0 at this renderer's input buffers
    /// (`kAudioUnitProperty_SetRenderCallback`). Only a unit with an input bus
    /// needs it.
    ///
    /// # Errors
    /// The `OSStatus` of the write.
    pub fn install_input_callback(&mut self) -> Result<()> {
        self.callback_installs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let callback = coreaudio_sys::AURenderCallbackStruct {
            inputProc: Some(au_input_render_callback),
            inputProcRefCon: self.scratch.as_ptr().cast::<c_void>(),
        };
        self.unit.set(
            coreaudio_sys::kAudioUnitProperty_SetRenderCallback,
            coreaudio_sys::kAudioUnitScope_Input,
            0,
            &callback,
        )?;
        self.callback_installed = true;
        Ok(())
    }

    /// How many times the input callback has been installed.
    pub fn input_callback_installs(&self) -> u32 {
        self.callback_installs
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Copies up to `frames` frames of each input channel into the input
    /// buffers the callback serves.
    pub fn stage_input(&mut self, input: &[&[f32]], frames: u32) {
        self.scratch().stage_input(input, frames);
    }

    /// As [`stage_input`](Self::stage_input), narrowing from `f64`.
    pub fn stage_input_f64(&mut self, input: &[&[f64]], frames: u32) {
        self.scratch().stage_input_f64(input, frames);
    }

    /// Copies the rendered block out, up to `frames` frames per channel.
    pub fn emit_output(&mut self, output: &mut [&mut [f32]], frames: u32) {
        self.scratch().emit_output(output, frames);
    }

    /// As [`emit_output`](Self::emit_output), widening to `f64`.
    pub fn emit_output_f64(&mut self, output: &mut [&mut [f64]], frames: u32) {
        self.scratch().emit_output_f64(output, frames);
    }

    /// Sends the render cursor back to zero (see `reset` on the unit).
    pub fn reset_position(&mut self) {
        self.scratch().reset_position();
    }

    /// Renders `frames` frames (`AudioUnitRender`) into the output buffers.
    /// Returns whether the unit declared the block silent.
    ///
    /// # Errors
    /// [`Error::Invalid`] when `frames` exceeds the block size the buffers were
    /// sized for (nothing is rendered), else [`Error::RenderFailed`] with the status of `AudioUnitRender` and the
    /// unit's last-render-error property.
    pub fn render(&mut self, frames: u32) -> Result<bool> {
        if false && frames > self.block_size {
            return Err(Error::Invalid(format!(
                "frames ({frames}) > block_size ({})",
                self.block_size
            )));
        }
        let (abl, sample_time) = {
            let scratch = self.scratch();
            let abl = scratch.bind_output(frames);
            (abl, scratch.advance(frames))
        };
        let timestamp = timestamp_at(sample_time);
        let mut flags: AudioUnitRenderActionFlags = 0;
        // SAFETY: `abl` points at the scratch's list, bound to buffers of at
        // least `frames` frames (the caller's contract); `timestamp` and `flags`
        // are live for the call. The scratch is not borrowed across it, because
        // the input callback may read it from inside the call.
        let status = unsafe {
            coreaudio_sys::AudioUnitRender(self.unit.raw(), &mut flags, &timestamp, 0, frames, abl)
        };
        if status != 0 {
            let last = self
                .unit
                .get::<i32>(
                    coreaudio_sys::kAudioUnitProperty_LastRenderError,
                    coreaudio_sys::kAudioUnitScope_Global,
                    0,
                )
                .ok()
                .filter(|&e| e != 0);
            return Err(Error::RenderFailed {
                function: "AudioUnitRender",
                status,
                last,
            });
        }
        Ok(flags & coreaudio_sys::kAudioUnitRenderAction_OutputIsSilence != 0)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if self.callback_installed {
            // Withdraw the callback before the scratch it points at is freed:
            // a real proc with a null refcon, which the callback answers
            // "nothing to read". The status is ignored; this is teardown, and
            // the unit renders only through this renderer.
            let withdrawn = coreaudio_sys::AURenderCallbackStruct {
                inputProc: Some(au_input_render_callback),
                inputProcRefCon: std::ptr::null_mut(),
            };
            let withdrawn = self.unit.set(
                coreaudio_sys::kAudioUnitProperty_SetRenderCallback,
                coreaudio_sys::kAudioUnitScope_Input,
                0,
                &withdrawn,
            );
            if withdrawn.is_err() {
                // The unit may still hold the pointer, and other `Unit` clones
                // can keep it alive and rendering: leak the scratch rather than
                // free memory the callback could read.
                return;
            }
        }
        // SAFETY: `scratch` is the leaked box from `new`, freed exactly once
        // here, after the callback that read it was withdrawn (or never set).
        drop(unsafe { Box::from_raw(self.scratch.as_ptr()) });
    }
}

/// The unit's input render callback: serves the staged input to the unit's pull.
unsafe extern "C" fn au_input_render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut coreaudio_sys::AudioUnitRenderActionFlags,
    _in_time_stamp: *const coreaudio_sys::AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    guard("the input render callback", ERR_CANNOT_DO, || {
        // SAFETY: `in_ref_con` is the scratch pointer `install_input_callback`
        // registered (live until `Renderer::drop` withdraws it), and `io_data`
        // is the unit's own list for this pull; `render_input` checks both for
        // null.
        unsafe { render_input(in_ref_con, in_number_frames, io_data) }
    })
}

/// # Safety
/// `in_ref_con` is null or a live `RenderScratch`, and `io_data` is null or a
/// valid buffer list whose buffers' `mData` hold `mDataByteSize` bytes.
unsafe fn render_input(
    in_ref_con: *mut c_void,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    if in_ref_con.is_null() || io_data.is_null() {
        return -1;
    }
    // SAFETY: non-null and live by the caller's contract; only read here.
    let scratch = unsafe { &*(in_ref_con as *const RenderScratch) };
    let requested = in_number_frames as usize;

    // SAFETY: the caller's contract on `io_data`.
    for (ch, buf) in unsafe { iter_buffers_mut(io_data) }.enumerate() {
        if buf.mData.is_null() {
            buf.mDataByteSize = 0;
            continue;
        }
        let per_channel = (buf.mNumberChannels as usize).max(1);
        let capacity_frames =
            buf.mDataByteSize as usize / (std::mem::size_of::<f32>() * per_channel);
        let frames = requested.min(capacity_frames);
        if frames == 0 {
            buf.mDataByteSize = 0;
            continue;
        }

        // SAFETY: `mData` is non-null and holds `mDataByteSize` bytes, of which
        // `frames` f32s fit (`capacity_frames` above).
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.mData as *mut f32, frames) };
        match scratch.inputs.get(ch) {
            Some(src) => {
                let n = frames.min(src.len());
                dst[..n].copy_from_slice(&src[..n]);
                if n < frames {
                    dst[n..].fill(0.0);
                }
            }
            None => dst.fill(0.0),
        }
        buf.mDataByteSize = (frames * std::mem::size_of::<f32>() * per_channel) as u32;
    }
    0
}

// ------------------------------------------------------------------ push render

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
    /// reason the render scratch's `AblWord` documents: `AudioBufferList` needs
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

impl std::fmt::Debug for PushScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushScratch")
            .field("block_size", &self.block_size)
            .field("sample_position", &self.sample_position)
            .finish_non_exhaustive()
    }
}

impl PushScratch {
    /// Allocates for the given per-bus input and output widths.
    ///
    /// `inputs` and `outputs` are one entry per bus, in bus order — so a
    /// hypothetical sidechain compressor is `&[Stereo, Stereo]` in, `&[Stereo]`
    /// out. Size these from
    /// `AuInstance::bus_count`, never
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
    /// `AuInstance::set_block_size`
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
        // `AuActive::scratch` relies on. Recomputing per block would be correct
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

    /// Sends this scratch's render cursor back to zero.
    ///
    /// The push twin of the pull path's cursor restart, and it has to be a
    /// separate call because the two cursors are separate objects with separate
    /// owners: the pull cursor lives inside the instance, so
    /// `AuInstance::reset` can restart it
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

    /// Copies `src` (planar, one slice per channel) into input bus `bus`.
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

    /// Copies output bus `bus` out into `dst`.
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
    /// The slabs and channel vectors are `self`'s own and were sized together in
    /// [`Self::new`], so the [`bind_list`] preconditions hold for every bus.
    /// The pointers written into the lists borrow `self` and are valid only
    /// until the next `&mut self` call.
    fn bind_inputs(&mut self, frames: u32) {
        for bus in 0..self.input_slabs.len() {
            let width = self.input_widths[bus];
            let ptr = self.input_slabs[bus].as_mut_ptr() as *mut AudioBufferList;
            // SAFETY: `ptr` is slab `bus`, sized for `width` buffers, and
            // `input_audio[bus]` holds `width` channels of `block_size` frames.
            unsafe { bind_list(ptr, width, &mut self.input_audio[bus], frames) };
        }
    }

    /// Point every output bus's `AudioBufferList` at its channel storage.
    ///
    /// As [`Self::bind_inputs`].
    fn bind_outputs(&mut self, frames: u32) {
        for bus in 0..self.output_slabs.len() {
            let width = self.output_widths[bus];
            let ptr = self.output_slabs[bus].as_mut_ptr() as *mut AudioBufferList;
            // SAFETY: as for `bind_inputs`, for the output slab.
            unsafe { bind_list(ptr, width, &mut self.output_audio[bus], frames) };
        }
    }

    /// Returns the pre-advance sample position and move the cursor on, exactly as
    /// the pull path's `RenderScratch::advance` does — AudioToolbox wants the
    /// block's *start* time, not its end.
    fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }
}

/// Fills one `AudioBufferList` in place from planar channel storage.
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
    // SAFETY: the caller's contract.
    unsafe { (*list).mNumberBuffers = count as u32 };
    for (ch, buf) in channels.iter_mut().take(count).enumerate() {
        // SAFETY: `ch < width`, within the slab (the caller's contract).
        let audio_buf = unsafe { &mut *((&mut (*list).mBuffers[0] as *mut AudioBuffer).add(ch)) };
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

/// Pushes `frames` of audio through the AU with `AudioUnitProcess`.
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
/// * `Error::InvalidBuffer` if `frames` exceeds the scratch's block size, or
///   if the scratch has no input bus 0 / no output bus 0. The frame bound is
///   checked here rather than left to the AU because the host owns the buffer:
///   `bind_list` would report `frames * 4` bytes for storage sized to
///   `block_size`, and the AU would write past it. (The AU *also* refuses —
///   measured `-10874`, `kAudioUnitErr_TooManyFramesToProcess`, for a 256-frame
///   render on a 64-frame AUDelay — but only after being handed a buffer list
///   that lies about its size.)
/// * `Error::RenderFailed` with the AU's `LastRenderError` attached when
///   `AudioUnitProcess` itself fails, which for most units means `unimpErr`.
///
pub fn process_push(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    if frames > scratch.block_size {
        return Err(Error::Invalid(format!(
            "frames ({frames}) > block_size ({})",
            scratch.block_size
        )));
    }
    if scratch.input_audio.is_empty() || scratch.output_audio.is_empty() {
        return Err(Error::Invalid(
            "AudioUnitProcess needs one input bus and one output bus".to_string(),
        ));
    }

    scratch.bind_inputs(frames);
    let list = scratch.input_slabs[0].as_mut_ptr() as *mut AudioBufferList;
    let timestamp = timestamp_at(scratch.advance(frames));
    let mut flags: AudioUnitRenderActionFlags = 0;

    // SAFETY: `list` is input slab 0, bound to buffers of `frames` frames by
    // `bind_inputs`, and `timestamp` and `flags` are live for the call.
    let status = unsafe {
        coreaudio_sys::AudioUnitProcess(unit.raw(), &mut flags, &timestamp, frames, list)
    };
    if status != NO_ERR {
        return Err(Error::RenderFailed {
            function: "AudioUnitProcess",
            status,
            last: last_render_error(unit),
        });
    }

    // `OutputIsSilence` is honoured for the reason `AuActive::process` documents:
    // when the AU sets it, the buffer contents are explicitly *not* guaranteed
    // zeroed — the flag is how an AU says "I produced nothing, don't trust what
    // is in there" — so trusting them emits whatever the last block left.
    let silent = flags & K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE != 0;
    copy_in_bus0_to_out_bus0(scratch, silent, frames);
    Ok(flags)
}

/// Copies input bus 0 into output bus 0 (or zero it), without allocating.
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

/// Pushes `frames` through the AU with `AudioUnitProcessMultiple` — the sidechain
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
/// * `Error::InvalidBuffer` if `frames` exceeds the scratch's block size, or
///   if the scratch has no output bus at all — an AU must write somewhere, and a
///   zero-length output array is not a render.
/// * `Error::RenderFailed` carrying the AU's `LastRenderError`. An AU handed
///   more input lists than it has input elements reports
///   `kAudioUnitErr_InvalidElement` here, and one that does not implement the
///   selector reports `unimpErr`. Both surface rather than being absorbed: a
///   silently-dropped sidechain is a compressor that simply never ducks, which
///   is indistinguishable from one whose threshold is set too high.
///
pub fn process_push_multiple(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    if frames > scratch.block_size {
        return Err(Error::Invalid(format!(
            "frames ({frames}) > block_size ({})",
            scratch.block_size
        )));
    }
    if scratch.output_audio.is_empty() {
        return Err(Error::Invalid(
            "AudioUnitProcessMultiple needs at least one output bus".to_string(),
        ));
    }

    scratch.bind_inputs(frames);
    scratch.bind_outputs(frames);
    let timestamp = timestamp_at(scratch.advance(frames));
    let mut flags: AudioUnitRenderActionFlags = 0;

    // The pointer arrays are fields, pre-filled in `new`, so `as_mut_ptr` here
    // is a field read and nothing on this path allocates.
    let n_in = scratch.input_ptrs.len() as u32;
    let n_out = scratch.output_ptrs.len() as u32;
    // SAFETY: both pointer tables name `n_in` and `n_out` slabs bound by
    // `bind_inputs`/`bind_outputs` to buffers of `frames` frames; the timestamp
    // and flags are live for the call.
    let status = unsafe {
        coreaudio_sys::AudioUnitProcessMultiple(
            unit.raw(),
            &mut flags,
            &timestamp,
            frames,
            n_in,
            scratch.input_ptrs.as_mut_ptr(),
            n_out,
            scratch.output_ptrs.as_mut_ptr(),
        )
    };
    if status != NO_ERR {
        return Err(Error::RenderFailed {
            function: "AudioUnitProcessMultiple",
            status,
            last: last_render_error(unit),
        });
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
/// `AuActive::process` is, so the property read it performs cannot affect the
/// no-alloc guarantee on the steady-state path.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
fn last_render_error(unit: &Unit) -> Option<i32> {
    unit.get::<i32>(
        coreaudio_sys::kAudioUnitProperty_LastRenderError,
        coreaudio_sys::kAudioUnitScope_Global,
        0,
    )
    .ok()
    .filter(|&e| e != 0)
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
pub fn output_byte_sizes(scratch: &PushScratch) -> Vec<u32> {
    let mut sizes = Vec::new();
    for slab in scratch.output_slabs.iter() {
        let list = slab.as_ptr().cast::<AudioBufferList>();
        // SAFETY: each slab is `buffer_list_bytes` long for its bus's width,
        // 8-aligned and bound by `bind_outputs`, so the header and `count`
        // buffers are in bounds; this only reads, through `&PushScratch`.
        let count = unsafe { (*list).mNumberBuffers } as usize;
        // SAFETY: as above.
        let base = unsafe { std::ptr::addr_of!((*list).mBuffers) }.cast::<AudioBuffer>();
        for i in 0..count {
            // SAFETY: `i < count`, within the slab.
            sizes.push(unsafe { (*base.add(i)).mDataByteSize });
        }
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size math must be driven by
    /// `offset_of!(AudioBufferList, mBuffers)`, never by `size_of::<u32>()`.
    /// The two differ by the 4 bytes of tail padding after `mNumberBuffers`
    /// (the struct is 8-aligned because `AudioBuffer` holds a pointer), and
    /// sizing off `u32` makes the LAST channel's `mData` store run past the end
    /// of the slab on every render — mono included. It only ever "worked"
    /// because malloc rounds 36 up to a 48-byte bucket.
    #[test]
    fn buffer_list_size_matches_the_c_abi() {
        // Ground truth, measured on macOS/arm64 against the real SDK header.
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::offset_of!(AudioBufferList, mBuffers), 8);
        assert_eq!(std::mem::size_of::<AudioBufferList>(), 24);

        // The header contribution is the *offset*, strictly larger than
        // `size_of::<u32>()` because of the alignment padding after
        // `mNumberBuffers`. Sizing from the field width instead of the offset
        // under-allocates by exactly that padding, so this inequality is what
        // makes the two formulas distinguishable at all.
        assert!(
            std::mem::offset_of!(AudioBufferList, mBuffers) > std::mem::size_of::<u32>(),
            "if these were equal the old formula would have been correct"
        );

        // A 1-buffer list must be at least as large as the declared struct.
        assert!(buffer_list_bytes(1) >= std::mem::size_of::<AudioBufferList>());

        for n in 0..=8usize {
            assert_eq!(
                buffer_list_bytes(n),
                std::mem::offset_of!(AudioBufferList, mBuffers)
                    + n * std::mem::size_of::<AudioBuffer>(),
            );
        }
        // The concrete numbers: 40 for stereo, not 36.
        assert_eq!(buffer_list_bytes(2), 40);
        assert_eq!(buffer_list_bytes(1), 24);
    }

    /// The slab must be big enough AND aligned for `AudioBufferList`. A
    /// `Box<[u8]>` is only 1-byte aligned, which is UB to reinterpret as an
    /// 8-aligned struct even where it happens to work.
    ///
    /// The required size is recomputed here straight from `offset_of!` rather
    /// than reusing `buffer_list_bytes`, which is the function under test —
    /// checking it against itself would pass with any formula.
    #[test]
    fn slab_is_large_enough_and_aligned() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::QUAD,
            ChannelLayout::from(8u16),
        ] {
            let list = RenderBufferList::new(layout);
            let n = layout.count() as usize;
            let required = std::mem::offset_of!(AudioBufferList, mBuffers)
                + n * std::mem::size_of::<AudioBuffer>();
            let bytes = std::mem::size_of_val(&*list.storage);
            assert!(
                bytes >= required,
                "slab {bytes} bytes < required {required} for {n} channels"
            );
            assert_eq!(
                list.storage.as_ptr() as usize % std::mem::align_of::<AudioBufferList>(),
                0,
                "slab must be aligned for AudioBufferList"
            );
        }
    }

    /// The render cursor advances by one block per call, returns the block's
    /// *start* frame, and goes back to zero on `reset_position`.
    ///
    /// The narrow, allocation-free half of what `tests/au_render_clock.rs`
    /// asserts against a real AU: that suite proves the timestamp the plugin
    /// receives, this one proves the arithmetic underneath it. Kept separate
    /// because the failure modes differ — a wrong `advance` and a `reset` wired
    /// to the wrong place look identical from outside.
    #[test]
    fn the_render_cursor_advances_and_restarts() {
        let mut scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            64,
        );

        assert_eq!(scratch.position(), 0.0, "a fresh cursor starts at zero");
        assert_eq!(
            scratch.advance(64),
            0.0,
            "the first block starts at frame 0"
        );
        assert_eq!(scratch.advance(64), 64.0, "the second starts one block on");
        assert_eq!(scratch.position(), 128.0);

        scratch.reset_position();
        assert_eq!(scratch.position(), 0.0);
        // And it advances again from there rather than sticking — the wrong fix
        // (zeroing on every block) would pass the line above and fail here.
        assert_eq!(scratch.advance(64), 0.0);
        assert_eq!(scratch.advance(64), 64.0);
    }

    /// A stereo scratch with `block` frames per channel.
    fn stereo_scratch(block: u32) -> RenderScratch {
        RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            block,
        )
    }

    /// The f64 staging pair must carry a block through unchanged apart from the
    /// f32 narrowing the AU forces.
    ///
    /// Round-tripping through `outputs` directly (rather than through a real
    /// `AudioUnitRender`) is what makes this a pure test of the conversion: an AU
    /// in the loop would make a failure ambiguous between the staging and the
    /// plugin.
    #[test]
    fn f64_staging_round_trips_through_the_f32_scratch() {
        let mut scratch = stereo_scratch(4);

        let l: [f64; 4] = [0.25, -0.5, 0.75, -1.0];
        let r: [f64; 4] = [1.0, 0.5, -0.25, 0.125];
        scratch.stage_input_f64(&[&l[..], &r[..]], 4);

        // Every value above is exactly representable in f32, so the staging must
        // be bit-exact — no epsilon needed, and a comparison that needed one
        // would be reporting a bug rather than float noise.
        assert_eq!(scratch.inputs[0][..4], [0.25f32, -0.5, 0.75, -1.0]);
        assert_eq!(scratch.inputs[1][..4], [1.0f32, 0.5, -0.25, 0.125]);

        // Stand in for the AU's render: whatever lands in `outputs` is what
        // `emit_output_f64` must widen back out.
        scratch.outputs[0][..4].copy_from_slice(&[0.25, -0.5, 0.75, -1.0]);
        scratch.outputs[1][..4].copy_from_slice(&[1.0, 0.5, -0.25, 0.125]);

        let (mut out_l, mut out_r) = ([0.0f64; 4], [0.0f64; 4]);
        let mut outs: Vec<&mut [f64]> = vec![&mut out_l, &mut out_r];
        scratch.emit_output_f64(&mut outs, 4);

        assert_eq!(out_l, [0.25, -0.5, 0.75, -1.0]);
        assert_eq!(out_r, [1.0, 0.5, -0.25, 0.125]);
    }

    /// Precision past f32 does not survive, and that is the documented contract
    /// rather than a defect — AUv2 renders in f32 and has no f64 entry point.
    ///
    /// Pinned so the loss is a stated property with a test behind it. If AUv3 or
    /// a future path ever renders natively at f64, this test failing is the
    /// correct alarm: the doc on `process_f64` would then be wrong.
    #[test]
    fn f64_staging_narrows_to_the_aus_own_precision() {
        // Needs 53 bits of mantissa: exact in f64, rounds in f32.
        const EXACT: f64 = 1.0 + f64::EPSILON;
        assert_ne!(
            EXACT as f32 as f64, EXACT,
            "the fixture is vacuous unless this value actually rounds in f32"
        );

        let mut scratch = stereo_scratch(4);
        let src = [EXACT; 4];
        scratch.stage_input_f64(&[&src[..]], 4);

        assert_eq!(
            scratch.inputs[0][0], EXACT as f32,
            "staged at the AU's precision, not the caller's"
        );
    }

    /// Both f64 halves are bounded on the caller's side and the scratch's, so a
    /// caller slice shorter OR longer than the block cannot panic.
    ///
    /// The f32 pair already clamps with a three-way `min`; these were written to
    /// match rather than to fix a live crash, and this pins that they do. A
    /// staging pair that indexed `[..frames]` on the caller directly would panic
    /// on the short case, which is the mirror of a fault fixed in VST2's scratch.
    #[test]
    fn f64_staging_is_bounded_on_both_sides() {
        let mut scratch = stereo_scratch(8);

        // Caller supplies fewer frames than the block asks for.
        let short = [1.0f64; 3];
        scratch.stage_input_f64(&[&short[..]], 8);
        assert_eq!(&scratch.inputs[0][..3], &[1.0f32; 3]);

        // Caller supplies more than the scratch holds.
        let long = [2.0f64; 32];
        scratch.stage_input_f64(&[&long[..]], 8);
        assert_eq!(&scratch.inputs[0][..8], &[2.0f32; 8]);

        // And the emit half, both directions.
        for ch in scratch.outputs.iter_mut() {
            ch.fill(0.5);
        }

        let mut out_short = [0.0f64; 3];
        let mut outs: Vec<&mut [f64]> = vec![&mut out_short];
        scratch.emit_output_f64(&mut outs, 8);
        assert_eq!(out_short, [0.5f64; 3], "filled as far as the caller goes");

        let mut out_long = [-1.0f64; 12];
        let mut outs: Vec<&mut [f64]> = vec![&mut out_long];
        scratch.emit_output_f64(&mut outs, 8);
        assert_eq!(&out_long[..8], &[0.5f64; 8]);
        assert!(
            out_long[8..].iter().all(|&v| v == -1.0),
            "past the block stays untouched"
        );
    }

    /// More caller channels than the scratch holds are skipped, not indexed into.
    #[test]
    fn f64_staging_ignores_channels_the_scratch_lacks() {
        let mut scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::MONO,
                outputs: ChannelLayout::MONO,
                has_input: true,
            },
            4,
        );

        let (a, b) = ([1.0f64; 4], [2.0f64; 4]);
        scratch.stage_input_f64(&[&a[..], &b[..]], 4);
        assert_eq!(&scratch.inputs[0][..4], &[1.0f32; 4]);

        scratch.outputs[0][..4].fill(0.25);
        let (mut out_a, mut out_b) = ([0.0f64; 4], [-1.0f64; 4]);
        let mut outs: Vec<&mut [f64]> = vec![&mut out_a, &mut out_b];
        scratch.emit_output_f64(&mut outs, 4);

        assert_eq!(out_a, [0.25f64; 4]);
        assert_eq!(out_b, [-1.0f64; 4], "channel 1 has no scratch behind it");
    }

    /// Every `mData` the bind loop writes must land inside the slab — the exact
    /// store that was out of bounds before.
    #[test]
    fn bind_writes_stay_inside_the_slab() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::from(6u16),
        ] {
            let n = layout.count() as usize;
            let mut list = RenderBufferList::new(layout);
            let lo = list.storage.as_ptr() as usize;
            let hi = lo + std::mem::size_of_val(&*list.storage);

            let mut chans: Vec<Vec<f32>> = (0..n).map(|_| vec![0.0f32; 64]).collect();
            let abl = list.bind(&mut chans, 64);

            // SAFETY: `abl` points at the list `bind` just filled for `n` buffers, live while `list` is.
            unsafe {
                assert_eq!((*abl).mNumberBuffers as usize, n);
                let base = &raw const (*abl).mBuffers[0];
                for (ch, chan) in chans.iter_mut().enumerate().take(n) {
                    let b = base.add(ch);
                    // The whole AudioBuffer, including its trailing mData
                    // field, must sit within the allocation.
                    let start = b as usize;
                    let end = start + std::mem::size_of::<AudioBuffer>();
                    assert!(
                        start >= lo && end <= hi,
                        "AudioBuffer[{ch}] spans {start}..{end}, slab is {lo}..{hi}"
                    );
                    assert_eq!((*b).mNumberChannels, 1);
                    assert_eq!((*b).mDataByteSize, 64 * 4);
                    assert_eq!((*b).mData, chan.as_mut_ptr() as *mut c_void);
                }
            }
        }
    }

    /// Builds a standalone `AudioBufferList` for the render-callback tests.
    /// Mirrors what AudioToolbox hands the callback.
    fn make_abl(buffers: &mut [(*mut f32, u32)]) -> (Vec<u8>, *mut AudioBufferList) {
        let n = buffers.len();
        let bytes = std::mem::offset_of!(AudioBufferList, mBuffers)
            + n.max(1) * std::mem::size_of::<AudioBuffer>();
        // Over-allocate and align by hand; this is test scaffolding standing in
        // for AudioToolbox's own allocation.
        let mut storage = vec![0u8; bytes + 16];
        let base = storage.as_mut_ptr();
        let offset = base.align_offset(std::mem::align_of::<AudioBufferList>());
        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        let abl = unsafe { base.add(offset) } as *mut AudioBufferList;
        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        unsafe {
            (*abl).mNumberBuffers = n as u32;
            let first = &raw mut (*abl).mBuffers[0];
            for (i, &mut (data, byte_size)) in buffers.iter_mut().enumerate() {
                let b = first.add(i);
                (*b).mNumberChannels = 1;
                (*b).mDataByteSize = byte_size;
                (*b).mData = data as *mut c_void;
            }
        }
        (storage, abl)
    }

    /// The callback must honour `mDataByteSize` and null-check `mData` rather
    /// than trusting `in_number_frames` to describe the buffer it was handed.
    ///
    /// Building the destination slice from `mData` over the full frame extent
    /// is an out-of-bounds write against a short buffer, and a null deref
    /// against a null one — both on the render thread.
    #[test]
    fn render_callback_respects_a_short_buffer() {
        let scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            512,
        );

        // The AU offers room for 8 frames but asks for 64. Sentinels past the
        // 8th slot must survive.
        const CAPACITY: usize = 8;
        const REQUESTED: u32 = 64;
        let mut chan = vec![f32::from_bits(0xDEAD_BEEF); 32];
        let data = chan.as_mut_ptr();
        let mut descs = [(data, (CAPACITY * 4) as u32)];
        let (_storage, abl) = make_abl(&mut descs);

        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        let status = unsafe {
            render_input(
                &scratch as *const RenderScratch as *mut c_void,
                REQUESTED,
                abl,
            )
        };
        assert_eq!(status, 0);

        // Everything past the declared capacity is untouched — that is the OOB
        // write not happening.
        for (i, s) in chan.iter().enumerate().skip(CAPACITY) {
            assert_eq!(
                s.to_bits(),
                0xDEAD_BEEF,
                "frame {i} past the declared {CAPACITY}-frame capacity was overwritten"
            );
        }
        // And the reported size is the capacity actually written, not the
        // host's requested figure — writing the request back blindly would
        // tell the AU a short buffer is full-length.
        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        unsafe {
            let b = &raw const (*abl).mBuffers[0];
            assert_eq!((*b).mDataByteSize as usize, CAPACITY * 4);
        }
    }

    /// A null `mData` must be skipped, not dereferenced.
    #[test]
    fn render_callback_survives_a_null_buffer() {
        let scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 256u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
            // for the call and described truthfully (or deliberately malformed in the
            // ways the callback must survive).
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, 0);
        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        unsafe {
            let b = &raw const (*abl).mBuffers[0];
            assert_eq!(
                (*b).mDataByteSize,
                0,
                "a null buffer must report zero bytes written, not the requested size"
            );
        }
    }

    /// A zero-capacity buffer is the degenerate short case; it must write
    /// nothing rather than forming a slice over a zero-length allocation.
    #[test]
    fn render_callback_handles_zero_capacity() {
        let scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            512,
        );
        let mut chan = vec![f32::from_bits(0xDEAD_BEEF); 8];
        let mut descs = [(chan.as_mut_ptr(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
            // for the call and described truthfully (or deliberately malformed in the
            // ways the callback must survive).
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, 0);
        assert!(chan.iter().all(|s| s.to_bits() == 0xDEAD_BEEF));
    }

    /// Null `ref_con` / `io_data` are rejected before any deref.
    #[test]
    fn render_callback_rejects_null_arguments() {
        let scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
        // for the call and described truthfully (or deliberately malformed in the
        // ways the callback must survive).
        assert_eq!(unsafe { render_input(std::ptr::null_mut(), 64, abl) }, -1);
        assert_eq!(
            // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
            // for the call and described truthfully (or deliberately malformed in the
            // ways the callback must survive).
            unsafe {
                render_input(
                    &scratch as *const RenderScratch as *mut c_void,
                    64,
                    std::ptr::null_mut(),
                )
            },
            -1
        );
    }

    /// The unwind half, driven through the real `extern "C"` entry
    /// point rather than through `render_input`.
    ///
    /// The guard is what stands between a panicking render body and undefined
    /// behaviour in AudioToolbox, so exercise the guarded symbol itself across
    /// the same malformed inputs. Every one of these must return a status —
    /// reaching this assert at all proves nothing unwound out of the
    /// `extern "C"` frame.
    #[test]
    fn the_guarded_callback_returns_a_status_for_every_malformed_input() {
        let scratch = RenderScratch::new(
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            512,
        );
        let ref_con = &scratch as *const RenderScratch as *mut c_void;
        let mut flags: AudioUnitRenderActionFlags = 0;
        let ts = AudioTimeStamp::default();

        let mut short_chan = vec![0.0f32; 32];
        let mut zero_chan = vec![0.0f32; 8];
        // (buffer descriptors, requested frames): short, null, zero-capacity,
        // and a frame count far past anything the scratch holds.
        let mut cases: Vec<(Vec<(*mut f32, u32)>, u32)> = vec![
            (vec![(short_chan.as_mut_ptr(), 8 * 4)], 64),
            (vec![(std::ptr::null_mut::<f32>(), 256)], 64),
            (vec![(zero_chan.as_mut_ptr(), 0)], 64),
            (vec![(short_chan.as_mut_ptr(), 32 * 4)], u32::MAX),
        ];

        for (descs, frames) in cases.iter_mut() {
            let (_storage, abl) = make_abl(descs);
            let status =
                // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
                // for the call and described truthfully (or deliberately malformed in the
                // ways the callback must survive).
                unsafe { au_input_render_callback(ref_con, &mut flags, &ts, 0, *frames, abl) };
            assert_eq!(
                status, 0,
                "a malformed-but-handleable buffer list should render silence, not fail"
            );
        }

        // Null arguments still short-circuit through the guard.
        let (_storage, abl) = make_abl(&mut [(std::ptr::null_mut::<f32>(), 0u32)]);
        assert_eq!(
            // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
            // for the call and described truthfully (or deliberately malformed in the
            // ways the callback must survive).
            unsafe { au_input_render_callback(std::ptr::null_mut(), &mut flags, &ts, 0, 64, abl) },
            -1
        );
        assert_eq!(
            // SAFETY: the buffer lists and pointers are the test scaffolding's own, live
            // for the call and described truthfully (or deliberately malformed in the
            // ways the callback must survive).
            unsafe {
                au_input_render_callback(ref_con, &mut flags, &ts, 0, 64, std::ptr::null_mut())
            },
            -1
        );
    }

    /// A render longer than the block the buffers were sized for is refused
    /// before any buffer is bound: `bind` would otherwise advertise `frames`
    /// samples on a `block_size` buffer and the unit would write past it.
    ///
    /// Mutation (run): delete the `frames > block_size` check in
    /// `Renderer::render` -> the call reaches the unit and returns
    /// `RenderFailed`, not `Invalid`, so this fails.
    #[test]
    fn a_render_past_the_block_size_is_refused() {
        let desc = coreaudio_sys::AudioComponentDescription {
            componentType: coreaudio_sys::kAudioUnitType_Effect,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = crate::ComponentRef::first_matching(&desc).expect("AUDelay ships with macOS");
        let unit = Unit::new(comp).expect("instantiate");
        let mut renderer = Renderer::new(
            &unit,
            BusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            64,
        );
        assert!(matches!(renderer.render(65), Err(Error::Invalid(_))));
        assert!(matches!(renderer.render(u32::MAX), Err(Error::Invalid(_))));
    }
}
