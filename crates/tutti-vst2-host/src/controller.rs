//! The main-thread half of an activation, and the transitions that split a
//! [`Vst2Loaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use tutti_plugin_types::MainThread;

use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, Controller, EditorSize, Loaded, LoadedPlugin,
    Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginResult, PluginTail,
    Prepare, Preset, PresetId, Processor, Refused, RenderMode, Request, RescanScope, RestartRules,
    SampleFormat, Samples, Stuck, WindowHandle,
};

use crate::loaded::Vst2Loaded;
use crate::processor::{ProcessorParts, Vst2Processor};
use crate::scratch::RenderScratch;

/// The main-thread half of an activation: the whole control surface of a
/// [`Vst2Loaded`] (the [`Control`] trait; [`loaded`](Self::loaded) and
/// [`loaded_mut`](Self::loaded_mut) for the format's own surface), beside a Processor that renders.
///
/// It **owns** the [`Vst2Loaded`], and so the plugin: the Processor holds only the audio
/// handle ([`RenderHandle`](vst::host::RenderHandle)), which shares the plugin's core and
/// does not keep it open. Dropping the Controller (on the thread the plugin was loaded on)
/// closes the plugin; a Processor that outlives it renders silence, and a Processor dropped
/// last does nothing. The main-thread entries take a [`MainThread`] (see
/// [`PluginInstance`](tutti_vst2_ffi::host::PluginInstance)); a parameter write is a direct
/// `setParameter`, which the SDK allows from either thread, so no write is queued and a
/// rejoin has none to carry.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once.
///
/// The doc test below is the guard (mutation, run: an `unsafe impl Sync` for the
/// type makes it compile, and the test fails):
///
/// ```compile_fail
/// // No `Deref`: a `&Vst2Controller` is not a `&Vst2Loaded`.
/// fn loaded(c: &tutti_vst2_host::Vst2Controller) -> &tutti_vst2_host::Vst2Loaded {
///     c
/// }
/// ```
///
/// ```compile_fail
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_host::Vst2Controller>();
/// ```
pub struct Vst2Controller {
    loaded: Vst2Loaded,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Vst2Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst2Controller").finish_non_exhaustive()
    }
}

impl Vst2Controller {
    fn new(loaded: Vst2Loaded) -> Self {
        Self {
            loaded,
            _not_sync: PhantomData,
        }
    }

    /// The loaded plugin, for the surface the [`Control`] impl does not cover.
    /// The Controller does not `Deref` to it (doc 016 section 2.2).
    #[must_use]
    pub fn loaded(&self) -> &Vst2Loaded {
        &self.loaded
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut Vst2Loaded {
        &mut self.loaded
    }
}

impl Control for Vst2Controller {
    fn info(&self) -> &PluginDescriptor {
        Control::info(self.loaded())
    }

    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        Control::wiring(self.loaded(), main)
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        Control::parameters(self.loaded(), main)
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        Control::parameter(self.loaded(), main, id)
    }

    /// A direct `setParameter` on the plugin: the SDK allows it from either
    /// thread, and the Processor's own automation writes go the same way.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        Control::set_parameter(self.loaded_mut(), main, id, value);
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded(), main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded(), main, id, text)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(self.loaded_mut(), main)
    }

    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        Control::load_state(self.loaded_mut(), main, bytes)
    }

    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        Control::presets(self.loaded_mut(), main)
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        Control::load_preset(self.loaded_mut(), main, id)
    }

    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        Control::current_preset(self.loaded_mut(), main)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        Control::set_automation_state(self.loaded_mut(), main, mode);
    }

    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        Control::rescan_parameters(self.loaded_mut(), main, scope);
    }

    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        Control::take_requests(self.loaded_mut(), main, out);
    }

    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        Control::set_render_mode(self.loaded_mut(), main, mode)
    }

    fn has_editor(&self, main: &MainThread) -> bool {
        Control::has_editor(self.loaded(), main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        Control::open_editor(self.loaded_mut(), main, parent, scale)
    }

    fn close_editor(&mut self, main: &MainThread) {
        Control::close_editor(self.loaded_mut(), main);
    }

    fn editor_idle(&mut self, main: &MainThread) {
        Control::editor_idle(self.loaded_mut(), main);
    }

    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<(ParamId, Normalized)>) {
        Control::take_reported_edits(self.loaded_mut(), main, out);
    }
}

impl Controller for Vst2Controller {
    type Loaded = Vst2Loaded;
    type Processor = Vst2Processor;

    fn latency(&self, _main: &MainThread) -> Samples {
        self.loaded.latency_samples()
    }

    fn tail(&self, main: &MainThread) -> PluginTail {
        Vst2Loaded::tail(&self.loaded, main)
    }
}

impl Loaded for Vst2Loaded {
    type Processor = Vst2Processor;
    type Controller = Vst2Controller;

    /// VST2 has no restart: a change of pins, latency or tail is read again.
    const RESTARTS: RestartRules = RestartRules::NONE;

    /// Tells the plugin its rate, block size and precision while it is
    /// suspended, resumes it, starts processing, and splits it.
    ///
    /// The precision is 64-bit only when the activation asks for
    /// [`SampleFormat::Float64`] and the plugin declared
    /// `effFlagsCanDoubleReplacing`: the opcode states the width the host WILL
    /// call the plugin at, and a plugin told 64 that is then called at 32 may
    /// route `processReplacing` to a stub. The render mode needs no call: it
    /// answers `audioMasterGetCurrentProcessLevel`.
    ///
    /// Whatever the plugin asked of the host from inside the activation about
    /// its pins, latency or tail is answered by the reads after it, and is
    /// dropped here.
    ///
    /// # Errors
    ///
    /// [`Refused`] hands the plugin back, suspended and untouched, if it is
    /// already active. The protocol has no way for a plugin to decline
    /// `effMainsChanged`: one that stays suspended and renders silence (a
    /// failed licence claim, say) is not an error the host can see.
    fn activate(
        mut self,
        main: &MainThread,
        prepare: Prepare,
    ) -> Result<(Vst2Processor, Vst2Controller), Refused<Self>> {
        let rate = prepare.rate().0;
        let block = prepare.max_block().get();
        let Some(audio) = self.audio.take() else {
            return Err(Refused {
                plugin: Some(self),
                reason: PluginError::Other("the plugin is already active".into()),
            });
        };

        let double_precision =
            prepare.format() == SampleFormat::Float64 && self.metadata().supports_f64;
        // Before each dispatch, so a plugin asking `audioMasterGetSampleRate` or
        // `GetBlockSize` from inside it reads the figure being set.
        self.host.set_sample_rate(rate);
        self.instance.set_sample_rate(main, rate as f32);
        self.host.set_block_size(block);
        self.instance.set_block_size(main, block as i64);
        self.host.set_offline(prepare.mode().is_offline());
        // `effSetProcessPrecision` is a suspended-state opcode, and a plugin that
        // switches its internal precision on it reallocates the same buffers
        // `effMainsChanged` does.
        self.instance.set_precision(main, double_precision);

        self.instance.resume(main);
        // The bracket every rejoin repeats: `effStartProcess` after
        // `effMainsChanged(1)`. A plugin that arms its DSP on the start edge
        // would otherwise render dry until the first block. `effStartProcess` is an audio
        // handle's entry, so it goes through one (a first handle, dropped at once: the
        // core remembers that processing is on), and the handle the Processor keeps is
        // made after it, with the pins as the plugin has them now.
        self.instance.render_handle(main).start_process();

        // The pins as the plugin has them now: it may have settled them in the
        // calls above.
        let (inputs, outputs) = self.pins();
        let render = self.instance.render_handle(main);
        let scratch = RenderScratch::new(inputs, outputs, block);
        let param_count = self.initial_values.len();
        let host = std::sync::Arc::clone(&self.host);

        // What the plugin asked of the host while it was activating about the
        // pins, latency and tail is answered by the reads that follow; the rest
        // waits for the next pass.
        let kept: Vec<_> = tutti_plugin_types::taken(self.host.requests())
            .filter(|request| {
                !matches!(
                    request,
                    tutti_plugin_types::Request::IoChanged
                        | tutti_plugin_types::Request::LatencyChanged
                        | tutti_plugin_types::Request::TailChanged
                )
            })
            .collect();
        for request in kept {
            let _ = request.raise(self.host.requests());
        }

        let processor = Vst2Processor::new(ProcessorParts {
            render,
            param_count,
            scratch,
            audio,
            block_size: block,
            double_precision,
            host,
        });
        Ok((processor, Vst2Controller::new(self)))
    }

    /// Rejoins the halves of one activation: checks they are a pair, stops the
    /// Processor (`effStopProcess`, once), drops it, and suspends the plugin
    /// (`effMainsChanged(0)`, once).
    ///
    /// # Errors
    ///
    /// The halves come back, with the reason, when they are of two activations.
    fn deactivate(
        main: &MainThread,
        mut processor: Vst2Processor,
        controller: Vst2Controller,
    ) -> Result<Self, Stuck<Vst2Processor, Vst2Controller>> {
        if !processor.audio_handle().is_of(&controller.loaded.instance) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the halves are of two activations".into()),
            });
        }
        Processor::stop(&mut processor);
        let audio = processor.into_audio();
        let mut loaded = controller.loaded;
        loaded.audio = Some(audio);
        loaded.suspend(main);
        Ok(loaded)
    }
}

impl Vst2Loaded {
    /// `effMainsChanged(0)`, if the plugin is resumed. The Processor has stopped
    /// processing by now (the rejoin stops it, and sends `effStopProcess` only
    /// if it was on), so the SDK's teardown order holds and nothing is sent
    /// twice.
    fn suspend(&mut self, main: &MainThread) {
        self.instance.suspend(main);
    }
}
