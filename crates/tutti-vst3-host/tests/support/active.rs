//! An activation's two halves held together: [`Active`] is
//! `tutti_fixtures::Active` over VST3's typed activation, and this file is what
//! VST3 adds to it: loading and activating in one call, a block through the
//! Processor at the width `T`, its counters, and the suites' bare-`f64`
//! parameter and state calls.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err, dead_code)]

use std::marker::PhantomData;
use std::path::Path;
use tutti_fixtures::{Activation, main_thread};

use tutti_plugin_types::{
    Control, MainThread, MaxBlock, Normalized, ParamId, PluginError, Prepare, RenderMode,
    SampleFormat, SampleRate, Samples,
};
use tutti_vst3_host::{
    AudioBuffer, InputEvents, Library, LoadOptions, ParameterChanges, ProcessFailed, ProcessMode,
    ProcessOutputRef, TransportInfo, Vst3Controller, Vst3Error, Vst3Loaded, Vst3Processor,
    Vst3Sample, class_id,
};

/// VST3's activation at the width `T`: `activate_typed`, whose Processor is
/// typed by it.
#[derive(Debug)]
pub struct Typed<T>(PhantomData<fn() -> T>);

impl<T: Vst3Sample> Activation for Typed<T> {
    type Loaded = Vst3Loaded;
    type Processor = Vst3Processor<T>;
    type Controller = Vst3Controller;

    fn activate(
        main: &MainThread,
        loaded: Vst3Loaded,
        prepare: Prepare,
    ) -> Result<(Vst3Processor<T>, Vst3Controller), (Option<Vst3Loaded>, PluginError)> {
        loaded
            .activate_typed::<T>(main, prepare)
            .map_err(|refused| (refused.plugin, refused.reason))
    }

    fn deactivate(
        main: &MainThread,
        processor: Vst3Processor<T>,
        controller: Vst3Controller,
    ) -> Result<Vst3Loaded, PluginError> {
        Vst3Loaded::deactivate_typed(main, processor, controller).map_err(|(_, _, reason)| reason)
    }
}

/// The two halves of one activation.
pub type Active<T = f32> = tutti_fixtures::Active<Typed<T>>;

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
        Active::activate(self, prepare_for::<T>(rate, frames, mode))
            .map_err(|(plugin, reason)| (plugin.expect("VST3 hands the plugin back"), reason))
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

/// What a suite calls on an [`Active`] besides the Controller: loading and
/// activating in one call, a block through the Processor at the width `T`, its
/// counters, and the loaded plugin.
pub trait ActiveExt<T: Vst3Sample>: Sized {
    /// Loads and activates, in realtime mode.
    fn load(path: &Path, rate: f64, frames: usize) -> Result<Self, LoadError> {
        Self::load_with_mode(path, rate, frames, ProcessMode::Realtime)
    }
    /// Loads and activates in `mode`.
    fn load_with_mode(
        path: &Path,
        rate: f64,
        frames: usize,
        mode: ProcessMode,
    ) -> Result<Self, LoadError>;
    /// Loads the class a bundle exports under `name` and activates it.
    fn load_class(path: &Path, name: &str, rate: f64, frames: usize) -> Result<Self, LoadError>;
    /// One block through the Processor.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> ProcessOutputRef<'_>;
    /// One block, with the failure the plugin reported.
    fn try_process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> Result<ProcessOutputRef<'_>, ProcessFailed>;
    /// [`Vst3Processor::set_prefetch`].
    fn set_prefetch(&mut self, prefetch: bool) -> bool;
    /// [`Vst3Processor::process_mode`].
    fn process_mode(&self) -> ProcessMode;
    /// [`Vst3Processor::event_text_dropped`].
    fn event_text_dropped(&self) -> usize;
    /// [`Vst3Processor::events_dropped`].
    fn events_dropped(&self) -> u64;
    /// [`Vst3Processor::sysex_dropped`].
    fn sysex_dropped(&self) -> u64;
    /// [`Vst3Processor::output_events_dropped`].
    fn output_events_dropped(&self) -> u64;
    /// [`Vst3Processor::param_points_dropped`].
    fn param_points_dropped(&self) -> u64;
    /// [`Vst3Processor::output_param_points_dropped`].
    fn output_param_points_dropped(&self) -> u64;
    /// [`Vst3Processor::stop`].
    fn stop(&mut self);
    /// The rate this activation runs at.
    fn sample_rate(&self) -> f64;
    /// [`reconfigure`](tutti_fixtures::Active::reconfigure) at a new rate.
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), PluginError>;
    /// Deactivates and activates at the configuration it has: what the core
    /// plans for a `kIoChanged`.
    fn restart_bus_configuration(&mut self) -> Result<(), PluginError>;
    /// The loaded plugin under the Controller. A suite names it on purpose:
    /// a call through it skips what the Controller tells the Processor.
    fn loaded(&self) -> &Vst3Loaded;
    /// [`loaded`](Self::loaded), mutably.
    fn loaded_mut(&mut self) -> &mut Vst3Loaded;
}

impl<T: Vst3Sample> ActiveExt<T> for Active<T> {
    fn load_with_mode(
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

    fn load_class(path: &Path, name: &str, rate: f64, frames: usize) -> Result<Self, LoadError> {
        let library = Library::load(path).expect("the bundle opens");
        let id = (0..library.count_classes())
            .filter_map(|i| library.class_info(i).ok())
            .find(|info| info.name == name)
            .map(|info| class_id(&info))
            .expect("the bundle exports a class under that name");
        drop(library);
        let loaded = Vst3Loaded::load(&main_thread(), path, LoadOptions::new().class_id(id))
            .map_err(LoadError::Load)?;
        loaded
            .activate_at::<T>(rate, frames)
            .map_err(|(_, reason)| LoadError::Activate(reason))
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> ProcessOutputRef<'_> {
        self.try_process(buffer, events, param_changes, transport)
            .expect("the block failed; a test of failure drives `try_process`")
    }

    fn try_process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        events: &InputEvents,
        param_changes: Option<&ParameterChanges>,
        transport: &TransportInfo,
    ) -> Result<ProcessOutputRef<'_>, ProcessFailed> {
        self.processor()
            .process(buffer, events, param_changes, transport)
    }

    fn set_prefetch(&mut self, prefetch: bool) -> bool {
        self.processor().set_prefetch(prefetch)
    }

    fn process_mode(&self) -> ProcessMode {
        self.processor_shared().process_mode()
    }

    fn event_text_dropped(&self) -> usize {
        self.processor_shared().event_text_dropped()
    }

    fn events_dropped(&self) -> u64 {
        self.processor_shared().events_dropped()
    }

    fn sysex_dropped(&self) -> u64 {
        self.processor_shared().sysex_dropped()
    }

    fn output_events_dropped(&self) -> u64 {
        self.processor_shared().output_events_dropped()
    }

    fn param_points_dropped(&self) -> u64 {
        self.processor_shared().param_points_dropped()
    }

    fn output_param_points_dropped(&self) -> u64 {
        self.processor_shared().output_param_points_dropped()
    }

    fn stop(&mut self) {
        self.processor().stop();
    }

    fn sample_rate(&self) -> f64 {
        self.prepare().rate().0
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), PluginError> {
        if rate == self.prepare().rate().0 {
            return Ok(());
        }
        let wanted = self
            .prepare()
            .with_rate(SampleRate(rate))
            .expect("a positive finite rate");
        self.reconfigure(wanted)
    }

    fn restart_bus_configuration(&mut self) -> Result<(), PluginError> {
        self.reconfigure(self.prepare())
    }

    fn loaded(&self) -> &Vst3Loaded {
        self.controller().loaded()
    }

    fn loaded_mut(&mut self) -> &mut Vst3Loaded {
        self.controller_mut().loaded_mut()
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
