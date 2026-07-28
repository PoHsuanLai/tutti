//! High-level AU instance lifecycle: load → initialize → render.
//!
//! [`AuInstance`] is the main entry point for hosting an Audio Unit. It
//! internally tracks whether the AU has been `AudioUnitInitialize`d and only
//! permits `process()` calls in the ready state.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use crate::buffer::{iter_buffers_mut, RenderScratch};
use crate::cf::CfPlist;
use crate::component::AuType;
use crate::error::{AuError, Result};
use crate::ffi::{check, get_property, set_property};
use crate::handle::AuHandle;
use crate::parameters::{self, AuParameter, ParamView};
use crate::stream::{AuBusLayout, StreamConfig};
use crate::types::*;
use tutti_midi_types::MidiEvent;
use tutti_plugin_types::ChannelLayout;

/// An AU that has been instantiated but not yet initialized.
///
/// In this state parameters and state can be queried/set, the editor can
/// be opened, but audio rendering is not yet possible.
pub struct AuLoaded {
    handle: AuHandle,
    config: StreamConfig,
}

/// An AU that has completed `AudioUnitInitialize` and has render buffers
/// allocated. This is the only state in which [`AuInstance::process`] will
/// succeed.
pub struct AuReady {
    loaded: AuLoaded,
    /// Heap-pinned so its address is stable across the `State`/`mem::replace`
    /// moves in [`AuInstance::initialize`]/[`AuInstance::uninitialize`]. The
    /// AU's input render callback holds a `ref_con` pointing at `*scratch`;
    /// moving the `Box` moves only its 8-byte pointer, not the body, so that
    /// `ref_con` stays valid across state transitions (FIX 1 / FIX 2). Anything
    /// that frees this box MUST have already run `AudioUnitUninitialize` so the
    /// AU can no longer call back into freed memory.
    scratch: Box<RenderScratch>,
    /// Test-only: counts input-render-callback installs on THIS instance, to
    /// prove the FIX-1 invariant (installed once per initialize, never per
    /// render block) without a process-global that races parallel tests.
    #[cfg(test)]
    callback_installs: std::sync::atomic::AtomicU32,
}

/// Public façade wrapping either an [`AuLoaded`] or [`AuReady`] state.
///
/// Most host operations (parameters, state save/load, editor) work regardless
/// of initialization status. [`AuInstance::process`] requires the Ready state
/// and will return `Uninitialized` otherwise.
pub struct AuInstance {
    state: State,
}

enum State {
    Loaded(AuLoaded),
    Ready(AuReady),
    /// Transient marker only seen while a `mem::replace` is mid-transition.
    Empty,
}

impl AuInstance {
    /// Instantiate an AU in the `Loaded` (pre-init) state.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent` handle obtained
    /// from `AudioComponentFindNext` or [`crate::component`].
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if instantiation or initial stream-format
    /// configuration fails.
    pub unsafe fn new(
        component: AudioComponent,
        sample_rate: f64,
        block_size: u32,
    ) -> Result<Self> {
        Ok(AuInstance {
            state: State::Loaded(AuLoaded::new(component, sample_rate, block_size)?),
        })
    }

    /// Transition Loaded → Ready. No-op if already Ready.
    pub fn initialize(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Empty) {
            State::Loaded(l) => match l.initialize() {
                Ok(r) => {
                    self.state = State::Ready(r);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            other @ State::Ready(_) => {
                self.state = other;
                Ok(())
            }
            State::Empty => unreachable!("AuInstance left empty"),
        }
    }

    /// Transition Ready → Loaded. No-op if already Loaded.
    pub fn uninitialize(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Empty) {
            State::Ready(r) => match r.uninitialize() {
                Ok(l) => {
                    self.state = State::Loaded(l);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            other @ State::Loaded(_) => {
                self.state = other;
                Ok(())
            }
            State::Empty => unreachable!("AuInstance left empty"),
        }
    }

    fn handle(&self) -> &AuHandle {
        match &self.state {
            State::Loaded(l) => &l.handle,
            State::Ready(r) => &r.loaded.handle,
            State::Empty => unreachable!("AuInstance accessed while empty"),
        }
    }

    fn config(&self) -> &StreamConfig {
        match &self.state {
            State::Loaded(l) => &l.config,
            State::Ready(r) => &r.loaded.config,
            State::Empty => unreachable!(),
        }
    }

    /// Raw `AudioUnit` pointer. Useful for interop with AudioToolbox calls
    /// not yet wrapped by this crate.
    pub fn raw_unit(&self) -> AudioUnit {
        self.handle().raw_unit()
    }

    /// High-level [`AuType`] this component was classified as.
    pub fn au_type(&self) -> AuType {
        self.handle().au_type()
    }

    /// Configured input channel count (`0` for generators / instruments, which
    /// have no input bus).
    pub fn num_inputs(&self) -> u32 {
        let channels = self.config().channels;
        if channels.has_input {
            channels.inputs.count() as u32
        } else {
            0
        }
    }

    /// Configured output channel count.
    pub fn num_outputs(&self) -> u32 {
        self.config().channels.outputs.count() as u32
    }

    /// Configured sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.config().sample_rate
    }

    /// Whether the AU is currently in the `Ready` state.
    pub fn is_initialized(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Test-only: how many times the input render callback has been installed
    /// on the current `Ready` instance (0 if not ready).
    #[cfg(test)]
    fn callback_install_count(&self) -> u32 {
        match &self.state {
            State::Ready(r) => r
                .callback_installs
                .load(std::sync::atomic::Ordering::SeqCst),
            _ => 0,
        }
    }

    /// Copy the AU's display name.
    pub fn get_name(&self) -> Result<String> {
        Ok(self.handle().get_name())
    }

    /// Write a parameter value.
    pub fn set_parameter(&mut self, id: u32, value: f32) -> Result<()> {
        parameters::set(self.raw_unit(), id, value)
    }

    /// Read a parameter value.
    pub fn get_parameter(&self, id: u32) -> Result<f32> {
        parameters::get(self.raw_unit(), id)
    }

    /// Deliver a block of UMP MIDI events to an instrument / music-effect AU as
    /// legacy `MusicDeviceMIDIEvent` calls.
    ///
    /// Each event is decoded to a 3-byte MIDI 1.0 channel-voice message (status
    /// byte + up to two data bytes, MIDI 2.0 resolutions scaled down per spec)
    /// and delivered at its `frame_offset`. Message families with no legacy
    /// 3-byte form (SysEx, per-note MIDI 2.0 messages, system real-time) are
    /// skipped — AUv2's `MusicDeviceMIDIEvent` only speaks legacy channel voice.
    ///
    /// Only meaningful for AUs whose type [`AuType::receives_midi`] is true;
    /// the caller gates on that. Errors from individual events are ignored so a
    /// single rejected message can't abort the whole block.
    pub fn send_midi(&self, events: &[MidiEvent]) {
        use tutti_midi_types::convert::{midi2_cc_to_midi1, midi2_pitch_bend_to_midi1};
        use tutti_midi_types::MidiMessage;

        let unit = self.raw_unit();
        for ev in events {
            // (status, data1, data2) for the legacy 3-byte message, or None if
            // this message has no legacy channel-voice representation.
            let (status, d1, d2) = match ev.message() {
                MidiMessage::NoteOn {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    let vel = tutti_midi_types::convert::midi2_velocity_to_midi1(velocity);
                    // A zero-velocity note-on is a note-off; keep it as note-on
                    // 0x90 with velocity 0 (a legal legacy note-off encoding).
                    (0x90 | (channel & 0x0F), note & 0x7F, vel & 0x7F)
                }
                MidiMessage::NoteOff {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    let vel = tutti_midi_types::convert::midi2_velocity_to_midi1(velocity);
                    (0x80 | (channel & 0x0F), note & 0x7F, vel & 0x7F)
                }
                MidiMessage::ControlChange {
                    channel,
                    index,
                    value,
                    ..
                } => (
                    0xB0 | (channel & 0x0F),
                    index & 0x7F,
                    midi2_cc_to_midi1(value) & 0x7F,
                ),
                MidiMessage::ProgramChange {
                    channel, program, ..
                } => (0xC0 | (channel & 0x0F), program & 0x7F, 0),
                MidiMessage::ChannelPressure {
                    channel, pressure, ..
                } => (
                    0xD0 | (channel & 0x0F),
                    midi2_cc_to_midi1(pressure) & 0x7F,
                    0,
                ),
                MidiMessage::PitchBend { channel, value, .. } => {
                    let bend14 = midi2_pitch_bend_to_midi1(value);
                    (
                        0xE0 | (channel & 0x0F),
                        (bend14 & 0x7F) as u8,
                        (bend14 >> 7) as u8 & 0x7F,
                    )
                }
                _ => continue,
            };
            unsafe {
                MusicDeviceMIDIEvent(unit, status as u32, d1 as u32, d2 as u32, ev.frame_offset);
            }
        }
    }

    /// Enumerate all parameters exposed by the AU.
    pub fn get_parameter_list(&self) -> Vec<AuParameter> {
        parameters::list(self.raw_unit())
    }

    /// Borrow a [`ParamView`] for scoped parameter access.
    pub fn parameters(&self) -> ParamView<'_> {
        unsafe { ParamView::new(self.raw_unit()) }
    }

    /// Plugin-reported processing latency in samples at the current sample rate.
    ///
    /// Returns 0 if the AU does not advertise `kAudioUnitProperty_Latency`.
    pub fn get_latency(&self) -> Result<u32> {
        let latency = unsafe {
            get_property::<f64>(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_LATENCY,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )
        }
        .unwrap_or(0.0);
        Ok((latency * self.sample_rate()) as u32)
    }

    /// Serialize the AU's current state (all parameters + internal state) to
    /// a binary plist blob suitable for persistence.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        tutti_plugin_types::assert_main_thread();
        let raw: core_foundation_sys::propertylist::CFPropertyListRef = unsafe {
            get_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )?
        };
        match unsafe { CfPlist::from_copied(raw) } {
            Some(plist) => plist.to_binary(),
            None => Ok(Vec::new()),
        }
    }

    /// Restore state previously produced by [`Self::save_state`]. Empty input is a no-op.
    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        let plist = CfPlist::from_binary(data)?;
        let raw = plist.as_raw();
        unsafe {
            set_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &raw,
            )
        }
    }

    /// Render `num_frames` of audio through the AU.
    ///
    /// `input` and `output` are per-channel planar slices. `num_frames` must
    /// not exceed the `block_size` passed to [`AuInstance::new`].
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] with `Uninitialized` if the AU has not
    /// been initialized, [`AuError::InvalidBuffer`] if `num_frames` exceeds
    /// the configured block size, or an `OsStatus` error from `AudioUnitRender`.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> Result<()> {
        match &mut self.state {
            State::Ready(r) => r.process(input, output, num_frames),
            State::Loaded(_) => Err(AuError::OsStatus {
                function: "AuInstance::process",
                code: K_AUDIO_UNIT_ERR_UNINITIALIZED,
            }),
            State::Empty => unreachable!(),
        }
    }

    /// Change the sample rate. If the AU was initialized, it is uninitialized
    /// for reconfiguration and then re-initialized to preserve the state.
    ///
    /// This returns `Ok` only when the AU is *verified* to be running at
    /// `rate`. `StreamConfig::apply` reads the accepted `mSampleRate` back and
    /// errors on a mismatch; on that error the previous rate is restored into
    /// the config, so [`sample_rate`](Self::sample_rate) and
    /// [`get_latency`](Self::get_latency) never report a rate the AU is not
    /// actually at. (Previously the set was best-effort and unverified, yet this
    /// returned `Ok(())` and recorded the *requested* rate — a phantom that both
    /// of those accessors then trusted.)
    pub fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        let was_ready = self.is_initialized();
        if was_ready {
            self.uninitialize()?;
        }
        if let State::Loaded(l) = &mut self.state {
            let previous = l.config.sample_rate;
            l.config.sample_rate = rate;
            // Re-apply and capture the effective layout the AU accepts at the
            // new rate, so the rebuilt scratch is sized correctly (FIX 3).
            match l.config.apply(&l.handle) {
                Ok(channels) => l.config.channels = channels,
                Err(e) => {
                    // Roll the config back to the rate the AU is still on, so a
                    // caller that ignores this error doesn't inherit a lie.
                    l.config.sample_rate = previous;
                    // Best-effort restore of the AU's own format too; if this
                    // also fails there is nothing further to do but surface the
                    // original rejection.
                    let _ = l.config.apply(&l.handle);
                    if was_ready {
                        self.initialize()?;
                    }
                    return Err(e);
                }
            }
        }
        if was_ready {
            self.initialize()?;
        }
        Ok(())
    }
}

impl AuLoaded {
    /// Instantiate and apply the initial stream configuration.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent`.
    pub unsafe fn new(
        component: AudioComponent,
        sample_rate: f64,
        block_size: u32,
    ) -> Result<Self> {
        let handle = AuHandle::new(component)?;

        let probed = StreamConfig::probe(&handle);
        let channels = AuBusLayout {
            inputs: probed.inputs,
            outputs: ChannelLayout::from(probed.outputs.count().max(2)),
            has_input: probed.has_input,
        };
        let mut config = StreamConfig::new(sample_rate, block_size, channels);
        // `apply` returns the layout the AU actually accepted, which may differ
        // from what we requested. Store the effective layout so the render
        // scratch is later sized to the real topology (FIX 3).
        config.channels = config.apply(&handle)?;

        Ok(Self { handle, config })
    }

    /// Consume self and return an [`AuReady`] after a successful
    /// `AudioUnitInitialize`.
    ///
    /// The input render callback is installed exactly ONCE here, off the
    /// heap-pinned scratch's stable address, rather than every render block on
    /// the RT thread (FIX 1). The `ref_con` is `&*scratch`; because `scratch`
    /// lives behind a `Box`, its body never moves even as the enclosing
    /// [`AuReady`]/`State` is `mem::replace`d, so the pointer the AU retains
    /// stays valid (FIX 2).
    pub fn initialize(self) -> Result<AuReady> {
        check("AudioUnitInitialize", unsafe {
            AudioUnitInitialize(self.handle.raw_unit())
        })?;

        // Allocate the heap-pinned scratch, then move it into `AuReady`. The
        // Box body does not move on that transfer (only the 8-byte pointer
        // does), so the ref_con derived from `&*ready.scratch` below is stable.
        let scratch = Box::new(RenderScratch::new(
            self.config.channels,
            self.config.block_size,
        ));
        let ready = AuReady {
            loaded: self,
            scratch,
            #[cfg(test)]
            callback_installs: std::sync::atomic::AtomicU32::new(0),
        };

        // Install the render callback ONCE, immediately after init, from the
        // scratch's stable heap address. AU accepts a render-callback set on the
        // input scope post-`AudioUnitInitialize`. There is deliberately no
        // per-block install in `process` (that was the RT-thread bug, FIX 1).
        let scratch_ptr: *mut RenderScratch = &*ready.scratch as *const RenderScratch as *mut _;
        if !ready.scratch.inputs.is_empty() {
            unsafe { ready.install_input_callback(scratch_ptr)? };
        }
        Ok(ready)
    }

    /// Borrow the underlying [`AuHandle`].
    pub fn handle(&self) -> &AuHandle {
        &self.handle
    }

    /// Borrow the configured [`StreamConfig`].
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }
}

impl AuReady {
    /// Tear down the render session and return to the [`AuLoaded`] state.
    pub fn uninitialize(self) -> Result<AuLoaded> {
        // Disable the Drop path (which would also uninitialize) to avoid a
        // double `AudioUnitUninitialize`.
        let mut me = std::mem::ManuallyDrop::new(self);
        // ORDERING INVARIANT (FIX 2): `AudioUnitUninitialize` MUST run before the
        // boxed scratch is freed below. After uninitialize the AU can no longer
        // fire the input render callback, so the ref_con pointing at `*scratch`
        // is guaranteed dead before we drop the Box. Reordering these two would
        // let the AU call back into freed memory.
        let status = unsafe { AudioUnitUninitialize(me.loaded.handle.raw_unit()) };
        check("AudioUnitUninitialize", status)?;
        // Move `loaded` out by reading through the ManuallyDrop. Safe because
        // nothing else touches `me` afterwards.
        let loaded = unsafe { std::ptr::read(&me.loaded) };
        // `scratch` is a `Box<RenderScratch>` owning heap storage; drop it
        // explicitly (only now that the AU is uninitialized).
        unsafe { std::ptr::drop_in_place(&mut me.scratch) };
        Ok(loaded)
    }

    /// Render `num_frames` through the AU. See [`AuInstance::process`] for
    /// arg semantics and error conditions.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> Result<()> {
        if num_frames > self.loaded.config.block_size {
            return Err(AuError::InvalidBuffer(format!(
                "num_frames ({num_frames}) > block_size ({})",
                self.loaded.config.block_size
            )));
        }

        self.scratch.stage_input(input, num_frames);

        // NOTE: the render callback is installed ONCE at initialize time off the
        // scratch's stable heap address — deliberately NOT here. Re-installing it
        // per block issued an `AudioUnitSetProperty` on the RT thread every call
        // (FIX 1) and, with the old inline scratch, handed the AU a ref_con that
        // dangled once the state machine moved (FIX 2).
        let abl = self.scratch.bind_output(num_frames);
        let timestamp = AudioTimeStamp::with_sample_time(self.scratch.advance(num_frames));
        let mut flags: AudioUnitRenderActionFlags = 0;

        let status = unsafe {
            AudioUnitRender(
                self.loaded.handle.raw_unit(),
                &mut flags,
                &timestamp,
                0,
                num_frames,
                abl,
            )
        };
        if status != NO_ERR {
            // A-2: enrich the diagnostic with the AU's last render error. This
            // is a global-scope/element-0 read the AU updates on each render;
            // it's advisory only, so a failed query silently degrades to the
            // plain `AudioUnitRender` OSStatus. Only reached when render itself
            // failed — never on a successful steady-state block — so the extra
            // property read does not affect the no-alloc guarantee.
            let last = unsafe {
                get_property::<OSStatus>(
                    self.loaded.handle.raw_unit(),
                    K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR,
                    K_AUDIO_UNIT_SCOPE_GLOBAL,
                    0,
                )
            }
            .ok()
            .filter(|&e| e != NO_ERR);
            return Err(AuError::render_failed("AudioUnitRender", status, last));
        }

        // A-1: honor the AU's OutputIsSilence signal. When the AU declares the
        // block silent, its output buffers are not guaranteed to be zeroed
        // (the flag is precisely how an AU says "I produced nothing, don't
        // trust the buffer contents"). Force the destination to silence rather
        // than emitting stale scratch. `fill` writes in place — no allocation,
        // preserving the RT no-alloc guarantee.
        if flags & K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE != 0 {
            let n = num_frames as usize;
            for dst in output.iter_mut() {
                let len = n.min(dst.len());
                dst[..len].fill(0.0);
            }
        } else {
            self.scratch.emit_output(output, num_frames);
        }
        Ok(())
    }

    /// Install the input render callback exactly once, wiring `ref_con` to the
    /// heap-pinned scratch's stable address.
    ///
    /// # Safety
    /// `scratch_ptr` must point at this `AuReady`'s boxed `RenderScratch` and
    /// must outlive every `AudioUnitRender` call and remain valid until
    /// `AudioUnitUninitialize` runs. The `Box` indirection guarantees the
    /// address is stable across `State`/`mem::replace` moves.
    unsafe fn install_input_callback(&self, scratch_ptr: *mut RenderScratch) -> Result<()> {
        #[cfg(test)]
        self.callback_installs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let callback = AURenderCallbackStruct {
            inputProc: Some(au_input_render_callback),
            inputProcRefCon: scratch_ptr as *mut c_void,
        };
        set_property(
            self.loaded.handle.raw_unit(),
            K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
            K_AUDIO_UNIT_SCOPE_INPUT,
            0,
            &callback,
        )
    }

    /// Borrow the underlying [`AuHandle`].
    pub fn handle(&self) -> &AuHandle {
        &self.loaded.handle
    }

    /// Borrow the configured [`StreamConfig`].
    pub fn config(&self) -> &StreamConfig {
        &self.loaded.config
    }
}

impl Drop for AuReady {
    fn drop(&mut self) {
        unsafe {
            let _ = AudioUnitUninitialize(self.loaded.handle.raw_unit());
        }
    }
}

/// The AU calls this on its render thread to pull input. It is `extern "C"`, so
/// A panic must never escape it: unwinding across the FFI boundary into
/// AudioToolbox is undefined behaviour. The whole body runs inside
/// [`catch_unwind`](std::panic::catch_unwind) and a caught panic is reported as
/// an error status, not swallowed.
unsafe extern "C" fn au_input_render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    // `AssertUnwindSafe`: the only state reachable here is `&RenderScratch`
    // (shared, read-only) and the AU's own buffers. A panic mid-copy can leave
    // a partially-written output buffer, which is a glitched block — not a
    // broken invariant — so there is nothing for unwind safety to protect.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_input(in_ref_con, in_number_frames, io_data)
    }));
    match result {
        Ok(status) => status,
        Err(_) => {
            // Do NOT swallow this. `eprintln!` rather than a logging facade
            // because this crate has no logger dependency, and a write to
            // stderr is the one diagnostic guaranteed to survive a process
            // whose audio thread just panicked.
            eprintln!(
                "tutti-au-host: PANIC in au_input_render_callback, \
                 contained to avoid unwinding into AudioToolbox"
            );
            K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT
        }
    }
}

/// The actual input-pull body. Split out so [`au_input_render_callback`] is
/// nothing but the `catch_unwind` guard around it.
///
/// # Safety
/// `in_ref_con` must be null or point at a live `RenderScratch`; `io_data` must
/// be null or a well-formed `AudioBufferList`.
unsafe fn render_input(
    in_ref_con: *mut c_void,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_data.is_null() {
        return -1;
    }

    let scratch = &*(in_ref_con as *const RenderScratch);
    let requested = in_number_frames as usize;

    for (ch, buf) in iter_buffers_mut(io_data).enumerate() {
        // Never trust the buffer the AU handed us. `mData` may be null
        // (the AU asking us to supply our own pointer) and `mDataByteSize` may
        // describe FEWER frames than `in_number_frames`. Writing
        // `in_number_frames` blind is a null deref in the first case and an
        // out-of-bounds store in the second.
        if buf.mData.is_null() {
            // Nothing to write into; report the size honestly as zero rather
            // than claiming we filled a buffer that does not exist.
            buf.mDataByteSize = 0;
            continue;
        }
        // The buffer's own declared capacity, in f32 frames. This channel is
        // non-interleaved (mNumberChannels == 1 in our ASBD), but honour a
        // wider mNumberChannels defensively by dividing it out.
        let per_channel = (buf.mNumberChannels as usize).max(1);
        let capacity_frames =
            buf.mDataByteSize as usize / (std::mem::size_of::<f32>() * per_channel);
        let frames = requested.min(capacity_frames);
        if frames == 0 {
            buf.mDataByteSize = 0;
            continue;
        }

        let dst = std::slice::from_raw_parts_mut(buf.mData as *mut f32, frames);
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
        // Report what was actually written, which is bounded by the buffer's
        // own capacity — not the host's requested frame count.
        buf.mDataByteSize = (frames * std::mem::size_of::<f32>() * per_channel) as u32;
    }

    NO_ERR
}

/// Test-only counter of how many times the input render callback property has
/// been set via `AudioUnitSetProperty(SetRenderCallback)`. Used by
/// `test_render_callback_installed_once` to prove the FIX-1 invariant: the
/// callback is installed once per initialize, never per render block.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::*;

    fn find_apple_delay() -> Option<AudioComponent> {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        find_component(&desc)
    }

    #[test]
    fn test_new() {
        let comp = find_apple_delay().expect("AUDelay should be present");
        let inst = unsafe { AuInstance::new(comp, 44100.0, 512) };
        assert!(inst.is_ok());
    }

    #[test]
    fn test_initialize_uninitialize() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();

        assert!(!inst.is_initialized());
        inst.initialize().unwrap();
        assert!(inst.is_initialized());
        inst.uninitialize().unwrap();
        assert!(!inst.is_initialized());
    }

    #[test]
    fn test_get_name() {
        let comp = find_apple_delay().unwrap();
        let inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        assert!(!inst.get_name().unwrap().is_empty());
    }

    #[test]
    fn test_parameter_list() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let params = inst.get_parameter_list();
        assert!(!params.is_empty());
    }

    #[test]
    fn test_get_set_parameter() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let params = inst.get_parameter_list();
        let p = &params[0];
        let mid = p.range.mid();
        inst.set_parameter(p.id, mid).unwrap();
        let val = inst.get_parameter(p.id).unwrap();
        assert!((val - mid).abs() < 0.01);
    }

    #[test]
    fn test_process_silence() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

        inst.process(&in_slices, &mut out_slices, 512).unwrap();
    }

    #[test]
    fn test_process_audio() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let input: Vec<Vec<f32>> = (0..2)
            .map(|_| {
                (0..512)
                    .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

        inst.process(&in_slices, &mut out_slices, 512).unwrap();
    }

    #[test]
    fn test_latency() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();
        let _ = inst.get_latency().unwrap();
    }

    #[test]
    fn test_save_load_state() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let state = inst.save_state().unwrap();
        assert!(!state.is_empty());
        inst.load_state(&state).unwrap();
    }

    #[test]
    fn test_render_callback_installed_once() {
        // AUDelay is an effect (has input), so the render callback IS installed.
        // Per-instance counter — no process-global, so this is race-free under
        // cargo's parallel test runner.
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        // Exactly one install after initialize — none per render block.
        assert_eq!(
            inst.callback_install_count(),
            1,
            "render callback should be installed exactly once at initialize"
        );

        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();

        for _ in 0..16 {
            let mut out_slices: Vec<&mut [f32]> =
                output.iter_mut().map(|v| v.as_mut_slice()).collect();
            inst.process(&in_slices, &mut out_slices, 512).unwrap();
        }

        // Still exactly one — process() must not re-install per block (FIX 1).
        assert_eq!(
            inst.callback_install_count(),
            1,
            "process() must not re-install the render callback per block"
        );
    }

    #[test]
    fn test_set_sample_rate() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();
        inst.set_sample_rate(48000.0).unwrap();
        assert_eq!(inst.sample_rate(), 48000.0);
        assert!(inst.is_initialized());
    }

    /// `sample_rate()` used to report the *requested* rate whether or
    /// not the AU took it, because the stream-format set was `let _`'d and only
    /// `mChannelsPerFrame` was read back. Now an `Ok` from `set_sample_rate`
    /// means the AU's own ASBD agrees — so assert against the AU, not against
    /// the number we just stored.
    #[test]
    fn set_sample_rate_ok_means_the_au_really_moved() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        for rate in [48000.0f64, 96000.0, 44100.0] {
            if inst.set_sample_rate(rate).is_err() {
                // A refusal is a legitimate outcome; what must never happen is
                // a refusal reported as success. Check the config was rolled
                // back rather than left holding the rejected rate.
                assert_ne!(
                    inst.sample_rate(),
                    rate,
                    "a rejected rate must not be left in the config"
                );
                continue;
            }
            assert_eq!(inst.sample_rate(), rate);
            // The independent check: ask the AU itself.
            let asbd = unsafe {
                get_property::<AudioStreamBasicDescription>(
                    inst.raw_unit(),
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_OUTPUT,
                    0,
                )
            }
            .expect("AUDelay reports its output stream format");
            assert!(
                (asbd.mSampleRate - rate).abs() < 1e-6,
                "set_sample_rate({rate}) returned Ok but the AU is at {}",
                asbd.mSampleRate
            );
        }
    }

    /// The end-to-end invariant: `set_sample_rate` returning `Ok` must
    /// imply the AU's own ASBD agrees.
    ///
    /// CAVEAT on what this can prove locally: Apple's AUDelay accepts *every*
    /// rate offered to it — probed here at 0.5 Hz, 1 Hz, 8 kHz, 48 kHz, 192 kHz
    /// and 1 MHz, all reported back verbatim. So no installed AU on this machine
    /// drives the *rejection* branch, and this test cannot fail by deleting the
    /// `check_sample_rate` call site. The rejection logic itself is pinned by
    /// `stream::tests::a_rate_the_au_did_not_accept_is_an_error`; what this adds
    /// is the standing guarantee that the two agree for whatever the AU does —
    /// including on a machine with a pickier AU installed.
    #[test]
    fn an_unusual_rate_cannot_be_reported_as_accepted_unless_it_was() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        // A rate a stricter AU would refuse.
        let absurd = 1.0f64;
        match inst.set_sample_rate(absurd) {
            Err(_) => {
                // Rejected, as expected. The config must have been rolled back
                // rather than left holding the rate the AU refused.
                assert_ne!(inst.sample_rate(), absurd);
            }
            Ok(()) => {
                // Astonishing, but then the AU must really be at 1 Hz.
                let asbd = unsafe {
                    get_property::<AudioStreamBasicDescription>(
                        inst.raw_unit(),
                        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                        K_AUDIO_UNIT_SCOPE_OUTPUT,
                        0,
                    )
                }
                .expect("AUDelay reports its output stream format");
                assert!(
                    (asbd.mSampleRate - absurd).abs() < 1e-6,
                    "set_sample_rate({absurd}) returned Ok while the AU is at {} — \
                     Ok must mean the rate was actually applied",
                    asbd.mSampleRate
                );
            }
        }
    }

    /// Build a standalone `AudioBufferList` for the render-callback tests.
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
        let abl = unsafe { base.add(offset) } as *mut AudioBufferList;
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

    /// The callback used to build its destination slice straight from
    /// `mData` for the full `in_number_frames` extent, never reading
    /// `mDataByteSize` and never null-checking `mData`. A short buffer was an
    /// out-of-bounds write; a null one was a null deref.
    #[test]
    fn render_callback_respects_a_short_buffer() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
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

        let status = unsafe {
            render_input(
                &scratch as *const RenderScratch as *mut c_void,
                REQUESTED,
                abl,
            )
        };
        assert_eq!(status, NO_ERR);

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
        // host's requested figure (which the old code wrote back blindly).
        unsafe {
            let b = &raw const (*abl).mBuffers[0];
            assert_eq!((*b).mDataByteSize as usize, CAPACITY * 4);
        }
    }

    /// A null `mData` must be skipped, not dereferenced.
    #[test]
    fn render_callback_survives_a_null_buffer() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 256u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, NO_ERR);
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
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut chan = vec![f32::from_bits(0xDEAD_BEEF); 8];
        let mut descs = [(chan.as_mut_ptr(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, NO_ERR);
        assert!(chan.iter().all(|s| s.to_bits() == 0xDEAD_BEEF));
    }

    /// Null `ref_con` / `io_data` are rejected before any deref.
    #[test]
    fn render_callback_rejects_null_arguments() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        assert_eq!(unsafe { render_input(std::ptr::null_mut(), 64, abl) }, -1);
        assert_eq!(
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
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let ref_con = &scratch as *const RenderScratch as *mut c_void;
        let mut flags: AudioUnitRenderActionFlags = 0;
        let ts = AudioTimeStamp::with_sample_time(0.0);

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
                unsafe { au_input_render_callback(ref_con, &mut flags, &ts, 0, *frames, abl) };
            assert_eq!(
                status, NO_ERR,
                "a malformed-but-handleable buffer list should render silence, not fail"
            );
        }

        // Null arguments still short-circuit through the guard.
        let (_storage, abl) = make_abl(&mut [(std::ptr::null_mut::<f32>(), 0u32)]);
        assert_eq!(
            unsafe { au_input_render_callback(std::ptr::null_mut(), &mut flags, &ts, 0, 64, abl) },
            -1
        );
        assert_eq!(
            unsafe {
                au_input_render_callback(ref_con, &mut flags, &ts, 0, 64, std::ptr::null_mut())
            },
            -1
        );
    }
}
