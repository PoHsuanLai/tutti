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
use crate::stream::{ChannelLayout, StreamConfig};
use crate::types::*;

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
    scratch: RenderScratch,
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

    /// Configured input channel count.
    pub fn num_inputs(&self) -> u32 {
        self.config().channels.inputs
    }

    /// Configured output channel count.
    pub fn num_outputs(&self) -> u32 {
        self.config().channels.outputs
    }

    /// Configured sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.config().sample_rate
    }

    /// Whether the AU is currently in the `Ready` state.
    pub fn is_initialized(&self) -> bool {
        matches!(self.state, State::Ready(_))
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
    pub fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        let was_ready = self.is_initialized();
        if was_ready {
            self.uninitialize()?;
        }
        if let State::Loaded(l) = &mut self.state {
            l.config.sample_rate = rate;
            l.config.apply(&l.handle)?;
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
        let channels = ChannelLayout {
            inputs: probed.inputs,
            outputs: probed.outputs.max(2),
        };
        let config = StreamConfig::new(sample_rate, block_size, channels);
        config.apply(&handle)?;

        Ok(Self { handle, config })
    }

    /// Consume self and return an [`AuReady`] after a successful
    /// `AudioUnitInitialize`.
    pub fn initialize(self) -> Result<AuReady> {
        check("AudioUnitInitialize", unsafe {
            AudioUnitInitialize(self.handle.raw_unit())
        })?;

        let scratch = RenderScratch::new(self.config.channels, self.config.block_size);
        Ok(AuReady {
            loaded: self,
            scratch,
        })
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
        let status = unsafe { AudioUnitUninitialize(me.loaded.handle.raw_unit()) };
        check("AudioUnitUninitialize", status)?;
        // Move `loaded` out by reading through the ManuallyDrop. Safe because
        // nothing else touches `me` afterwards.
        let loaded = unsafe { std::ptr::read(&me.loaded) };
        // `scratch` still owns Vecs and must be dropped explicitly.
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

        if !input.is_empty() {
            self.set_input_callback()?;
        }

        let abl = self.scratch.bind_output(num_frames);
        let timestamp = AudioTimeStamp::with_sample_time(self.scratch.advance(num_frames));
        let mut flags: AudioUnitRenderActionFlags = 0;

        check("AudioUnitRender", unsafe {
            AudioUnitRender(
                self.loaded.handle.raw_unit(),
                &mut flags,
                &timestamp,
                0,
                num_frames,
                abl,
            )
        })?;

        self.scratch.emit_output(output, num_frames);
        Ok(())
    }

    fn set_input_callback(&mut self) -> Result<()> {
        let callback = AURenderCallbackStruct {
            input_proc: au_input_render_callback,
            input_proc_ref_con: &mut self.scratch as *mut RenderScratch as *mut c_void,
        };
        unsafe {
            set_property(
                self.loaded.handle.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &callback,
            )
        }
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

unsafe extern "C" fn au_input_render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_data.is_null() {
        return -1;
    }

    let scratch = &*(in_ref_con as *const RenderScratch);
    let frames = in_number_frames as usize;

    for (ch, buf) in iter_buffers_mut(io_data).enumerate() {
        let dst = std::slice::from_raw_parts_mut(buf.data as *mut f32, frames);
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
        buf.data_byte_size = (frames * std::mem::size_of::<f32>()) as u32;
    }

    NO_ERR
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::*;

    fn find_apple_delay() -> Option<AudioComponent> {
        let desc = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_EFFECT,
            component_sub_type: u32::from_be_bytes(*b"dely"),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
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
    fn test_set_sample_rate() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();
        inst.set_sample_rate(48000.0).unwrap();
        assert_eq!(inst.sample_rate(), 48000.0);
        assert!(inst.is_initialized());
    }
}
