//! An activation's two halves held together, for the suites that drive one
//! plugin from a single thread.
//!
//! The Processor and the Controller are separate values on purpose; most of
//! these suites want "the active plugin", so [`Active`] holds the pair, derefs
//! to the Controller, and forwards the Processor's calls. It does not deref to
//! `Vst3Loaded`: a call meant for the loaded plugin names [`Active::loaded`], so
//! a name both have (`Control::set_parameter`) cannot quietly skip what the Controller tells
//! the Processor. [`Active::reconfigure`] is the deactivate/activate pair a
//! rate or block-size change is.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err, dead_code)]

use std::ops::{Deref, DerefMut};
use std::path::Path;

use tutti_plugin_types::{
    Control, MaxBlock, Normalized, ParamId, PluginError, Prepare, RenderMode, SampleFormat,
    SampleRate, Samples,
};
use tutti_vst3_host::{
    AudioBuffer, InputEvents, LoadOptions, ParameterChanges, ProcessFailed, ProcessMode,
    ProcessOutputRef, TransportInfo, Vst3Controller, Vst3Error, Vst3Loaded, Vst3Processor,
    Vst3Sample,
};

/// The two halves of one activation.
#[derive(Debug)]
pub struct Active<T: Vst3Sample = f32> {
    pair: Option<(Vst3Processor<T>, Vst3Controller)>,
    prepare: Prepare,
}

fn format_of<T: Vst3Sample>() -> SampleFormat {
    if std::mem::size_of::<T>() == 8 {
        SampleFormat::Float64
    } else {
        SampleFormat::Float32
    }
}

fn render_mode(mode: ProcessMode) -> RenderMode {
    match mode {
        ProcessMode::Offline => RenderMode::Offline,
        _ => RenderMode::Realtime,
    }
}

fn prepare_for<T: Vst3Sample>(rate: f64, frames: usize, mode: ProcessMode) -> Prepare {
    let block = MaxBlock::new(Samples(frames)).expect("a block of at least one frame");
    Prepare::new(SampleRate(rate), block)
        .expect("a positive finite rate")
        .with_format(format_of::<T>())
        .with_mode(render_mode(mode))
}

/// Activation at a rate and block size, on a loaded plugin.
pub trait Activate {
    /// Activates as `T`; a refusal hands the plugin back with the reason.
    fn activate_at<T: Vst3Sample>(
        self,
        rate: f64,
        frames: usize,
    ) -> Result<Active<T>, (Vst3Loaded, PluginError)>;

    /// [`activate_at`](Self::activate_at) in a given mode.
    fn activate_in<T: Vst3Sample>(
        self,
        rate: f64,
        frames: usize,
        mode: ProcessMode,
    ) -> Result<Active<T>, (Vst3Loaded, PluginError)>;
}

impl Activate for Vst3Loaded {
    fn activate_at<T: Vst3Sample>(
        self,
        rate: f64,
        frames: usize,
    ) -> Result<Active<T>, (Vst3Loaded, PluginError)> {
        self.activate_in(rate, frames, ProcessMode::Realtime)
    }

    fn activate_in<T: Vst3Sample>(
        self,
        rate: f64,
        frames: usize,
        mode: ProcessMode,
    ) -> Result<Active<T>, (Vst3Loaded, PluginError)> {
        let prepare = prepare_for::<T>(rate, frames, mode);
        match self.activate_typed::<T>(&main_thread(), prepare) {
            Ok(pair) => Ok(Active {
                pair: Some(pair),
                prepare,
            }),
            Err(refused) => Err((
                refused.plugin.expect("VST3 hands the plugin back"),
                refused.reason,
            )),
        }
    }
}

/// A load or an activation that did not take.
#[derive(Debug)]
pub enum LoadError {
    /// The bundle did not load.
    Load(Vst3Error),
    /// The plugin loaded and refused to activate.
    Activate(PluginError),
}

impl<T: Vst3Sample> Active<T> {
    /// Loads and activates, in realtime mode.
    pub fn load(path: &Path, rate: f64, frames: usize) -> Result<Self, LoadError> {
        Self::load_with_mode(path, rate, frames, ProcessMode::Realtime)
    }

    /// Loads and activates in `mode`.
    pub fn load_with_mode(
        path: &Path,
        rate: f64,
        frames: usize,
        mode: ProcessMode,
    ) -> Result<Self, LoadError> {
        let loaded =
            Vst3Loaded::load(&main_thread(), path, LoadOptions::new()).map_err(LoadError::Load)?;
        loaded
            .activate_in::<T>(rate, frames, mode)
            .map_err(|(_, reason)| LoadError::Activate(reason))
    }

    /// Loads the class a bundle exports under `name` and activates it.
    pub fn load_class(
        path: &Path,
        name: &str,
        rate: f64,
        frames: usize,
    ) -> Result<Self, LoadError> {
        let loaded = Vst3Loaded::load(&main_thread(), path, LoadOptions::new().class(name))
            .map_err(LoadError::Load)?;
        loaded
            .activate_at::<T>(rate, frames)
            .map_err(|(_, reason)| LoadError::Activate(reason))
    }

    fn pair(&self) -> &(Vst3Processor<T>, Vst3Controller) {
        self.pair.as_ref().expect("an Active holds its pair")
    }

    fn pair_mut(&mut self) -> &mut (Vst3Processor<T>, Vst3Controller) {
        self.pair.as_mut().expect("an Active holds its pair")
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut Vst3Processor<T> {
        &mut self.pair_mut().0
    }

    /// One block through the Processor.
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> ProcessOutputRef<'_> {
        self.try_process(buffer, events, param_changes, transport)
            .expect("the block failed; a test of failure drives `try_process`")
    }

    /// One block, with the failure the plugin reported.
    pub fn try_process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> Result<ProcessOutputRef<'_>, ProcessFailed> {
        self.pair_mut()
            .0
            .process(buffer, events, param_changes, transport)
    }

    /// [`Vst3Processor::set_prefetch`].
    pub fn set_prefetch(&mut self, prefetch: bool) -> bool {
        self.pair_mut().0.set_prefetch(prefetch)
    }

    /// [`Vst3Processor::process_mode`].
    pub fn process_mode(&self) -> ProcessMode {
        self.pair().0.process_mode()
    }

    /// [`Vst3Processor::event_text_dropped`].
    pub fn event_text_dropped(&self) -> usize {
        self.pair().0.event_text_dropped()
    }

    /// [`Vst3Processor::events_dropped`].
    pub fn events_dropped(&self) -> u64 {
        self.pair().0.events_dropped()
    }

    /// [`Vst3Processor::sysex_dropped`].
    pub fn sysex_dropped(&self) -> u64 {
        self.pair().0.sysex_dropped()
    }

    /// [`Vst3Processor::output_events_dropped`].
    pub fn output_events_dropped(&self) -> u64 {
        self.pair().0.output_events_dropped()
    }

    /// [`Vst3Processor::param_points_dropped`].
    pub fn param_points_dropped(&self) -> u64 {
        self.pair().0.param_points_dropped()
    }

    /// [`Vst3Processor::output_param_points_dropped`].
    pub fn output_param_points_dropped(&self) -> u64 {
        self.pair().0.output_param_points_dropped()
    }

    /// [`Vst3Processor::stop`].
    pub fn stop(&mut self) {
        self.pair_mut().0.stop();
    }

    /// The rate this activation runs at.
    pub fn sample_rate(&self) -> f64 {
        self.prepare.rate().0
    }

    /// Ends the activation: the plugin comes back deactivated.
    pub fn deactivate(mut self) -> Vst3Loaded {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        Vst3Loaded::deactivate_typed(&main_thread(), processor, controller)
            .map_err(|(_, _, reason)| reason)
            .expect("the halves are a pair")
    }

    /// Deactivates and activates in `prepare`, as a rate or block change does.
    /// A refusal activates again at the configuration it had and reports the
    /// refusal.
    pub fn reconfigure(&mut self, wanted: Prepare) -> Result<(), PluginError> {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        let loaded = Vst3Loaded::deactivate_typed(&main_thread(), processor, controller)
            .map_err(|(_, _, reason)| reason)
            .expect("the halves are a pair");
        match loaded.activate_typed::<T>(&main_thread(), wanted) {
            Ok(pair) => {
                self.pair = Some(pair);
                self.prepare = wanted;
                Ok(())
            }
            Err(refused) => {
                let reason = refused.reason;
                let plugin = refused.plugin.expect("VST3 hands the plugin back");
                let previous = self.prepare;
                match plugin.activate_typed::<T>(&main_thread(), previous) {
                    Ok(pair) => self.pair = Some(pair),
                    Err(_) => panic!("the plugin refused the configuration it had"),
                }
                Err(reason)
            }
        }
    }

    /// [`reconfigure`](Self::reconfigure) at a new rate.
    pub fn set_sample_rate(&mut self, rate: f64) -> Result<(), PluginError> {
        if rate == self.prepare.rate().0 {
            return Ok(());
        }
        let wanted = self
            .prepare
            .with_rate(SampleRate(rate))
            .expect("a positive finite rate");
        self.reconfigure(wanted)
    }

    /// Deactivates and activates at the configuration it has: what the core
    /// plans for a `kIoChanged`.
    pub fn restart_bus_configuration(&mut self) -> Result<(), PluginError> {
        self.reconfigure(self.prepare)
    }
}

impl<T: Vst3Sample> Active<T> {
    /// The Controller, named: a parameter write or a state load that is also
    /// queued to the Processor. `Deref` reaches it too.
    pub fn controller_mut(&mut self) -> &mut Vst3Controller {
        &mut self.pair_mut().1
    }

    /// The Controller, shared.
    pub fn controller(&self) -> &Vst3Controller {
        &self.pair().1
    }
}

impl<T: Vst3Sample> Active<T> {
    /// The loaded plugin under the Controller. A suite names it on purpose:
    /// a call through it skips what the Controller tells the Processor.
    pub fn loaded(&self) -> &Vst3Loaded {
        self.pair().1.loaded()
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut Vst3Loaded {
        self.pair_mut().1.loaded_mut()
    }
}

impl<T: Vst3Sample> Deref for Active<T> {
    type Target = Vst3Controller;

    fn deref(&self) -> &Vst3Controller {
        &self.pair().1
    }
}

impl<T: Vst3Sample> DerefMut for Active<T> {
    fn deref_mut(&mut self) -> &mut Vst3Controller {
        &mut self.pair_mut().1
    }
}

/// The normalized read and write the suites were written against, as bare
/// `f64`s: `Control::parameter` and `Control::set_parameter` without the
/// newtypes. On an [`Active`] they go through the Controller, so a write is
/// queued to the Processor too.
pub trait ParamExt {
    /// The parameter's normalized value; `0.0` when the plugin has none.
    fn get_parameter(&self, id: u32) -> f64;
    /// Writes a normalized value.
    fn set_parameter(&mut self, id: u32, value: f64);
}

impl ParamExt for Vst3Loaded {
    fn get_parameter(&self, id: u32) -> f64 {
        Control::parameter(self, &main_thread(), ParamId::new(id)).map_or(0.0, Normalized::get)
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        Control::set_parameter(
            self,
            &main_thread(),
            ParamId::new(id),
            Normalized::clamped(value),
        );
    }
}

impl ParamExt for Vst3Controller {
    fn get_parameter(&self, id: u32) -> f64 {
        Control::parameter(self, &main_thread(), ParamId::new(id)).map_or(0.0, Normalized::get)
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        Control::set_parameter(
            self,
            &main_thread(),
            ParamId::new(id),
            Normalized::clamped(value),
        );
    }
}

impl<T: Vst3Sample> ParamExt for Active<T> {
    fn get_parameter(&self, id: u32) -> f64 {
        ParamExt::get_parameter(self.controller(), id)
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        ParamExt::set_parameter(self.controller_mut(), id, value);
    }
}

/// State in and out as the suites were written against it, with the shared
/// vocabulary's error: `Control::save_state` and `Control::load_state`.
pub trait StateExt {
    /// The plugin's state as bytes.
    fn get_state(&mut self) -> Result<Vec<u8>, PluginError>;
    /// Restores state saved by [`get_state`](Self::get_state).
    fn set_state(&mut self, bytes: &[u8]) -> Result<(), PluginError>;
}

impl StateExt for Vst3Loaded {
    fn get_state(&mut self) -> Result<Vec<u8>, PluginError> {
        Control::save_state(self, &main_thread())
    }

    fn set_state(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        Control::load_state(self, &main_thread(), bytes)
    }
}

impl StateExt for Vst3Controller {
    fn get_state(&mut self) -> Result<Vec<u8>, PluginError> {
        Control::save_state(self, &main_thread())
    }

    fn set_state(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        Control::load_state(self, &main_thread(), bytes)
    }
}

impl<T: Vst3Sample> StateExt for Active<T> {
    fn get_state(&mut self) -> Result<Vec<u8>, PluginError> {
        StateExt::get_state(self.controller_mut())
    }

    fn set_state(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        StateExt::set_state(self.controller_mut(), bytes)
    }
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}
