//! The main-thread half of an activation, and the transitions that split a
//! [`Vst2Loaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use vst::plugin::Plugin as _;

use tutti_plugin_types::{
    AutomationMode, Control, Controller, Loaded, LoadedPlugin, Normalized, ParamId, ParameterInfo,
    PluginDescriptor, PluginError, PluginResult, PluginTail, Prepare, Preset, PresetId, Processor,
    Refused, RescanScope, RestartRules, SampleFormat, Samples, Stuck,
};

use crate::loaded::Vst2Loaded;
use crate::parameters::SendParams;
use crate::processor::{ProcessorParts, Vst2Processor};
use crate::scratch::RenderScratch;
use crate::shell::Shell;

/// The main-thread half of an activation: the whole control surface of a
/// [`Vst2Loaded`], reached through `Deref`, beside a Processor that renders.
///
/// Every call runs on the plugin's dispatcher while the Processor may be inside
/// `process`, which is what hosts do with a VST2 plugin and what the plugin is
/// written against; the one invariant on `Shell` says so plainly. A parameter
/// write is a direct `setParameter`, which the SDK allows from either thread, so
/// no write is queued and a rejoin has none to carry.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it releases its
/// share of the activation; the plugin is destroyed when the Processor's goes
/// too (see the invariant on the shell).
///
/// The doc test below is the guard (mutation, run: an `unsafe impl Sync` for the
/// type makes it compile, and the test fails):
///
/// ```compile_fail
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_host::Vst2Controller>();
/// ```
pub struct Vst2Controller {
    /// `None` only once `deactivate` has taken the shell.
    shell: Option<Arc<Shell>>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Vst2Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst2Controller").finish_non_exhaustive()
    }
}

// SAFETY: the Controller runs on one thread at a time (every method takes
// `&mut self` or a shared reference of a type that is not `Sync`), and clause 3
// of the invariant on `Shell` keeps its calls to the set hosts run beside
// `process`.
unsafe impl Send for Vst2Controller {}

impl Vst2Controller {
    fn new(shell: Arc<Shell>) -> Self {
        Self {
            shell: Some(shell),
            _not_sync: PhantomData,
        }
    }

    fn shell(&self) -> &Arc<Shell> {
        self.shell
            .as_ref()
            .expect("a Controller holds its shell until it is consumed")
    }

    fn loaded(&self) -> &Vst2Loaded {
        // SAFETY: main thread, shared; see `Shell::loaded`.
        unsafe { self.shell().loaded() }
            .as_ref()
            .expect("a live Controller's shell holds its plugin")
    }
}

impl Deref for Vst2Controller {
    type Target = Vst2Loaded;

    fn deref(&self) -> &Vst2Loaded {
        self.loaded()
    }
}

impl DerefMut for Vst2Controller {
    fn deref_mut(&mut self) -> &mut Vst2Loaded {
        let shell = self.shell();
        // SAFETY: main thread, exclusive; see `Shell::loaded_mut`.
        unsafe { shell.loaded_mut() }
            .as_mut()
            .expect("a live Controller's shell holds its plugin")
    }
}

impl Control for Vst2Controller {
    fn info(&self) -> &PluginDescriptor {
        Control::info(&**self)
    }

    fn wiring(&self) -> LoadedPlugin {
        Control::wiring(&**self)
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        Control::parameters(&**self)
    }

    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        Control::parameter(&**self, id)
    }

    /// A direct `setParameter` on the plugin: the SDK allows it from either
    /// thread, and the Processor's own automation writes go the same way.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        Control::set_parameter(&mut **self, id, value);
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(&**self, id, value)
    }

    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        Control::parameter_value_from_text(&**self, id, text)
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut **self)
    }

    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        Control::load_state(&mut **self, bytes)
    }

    fn presets(&mut self) -> Vec<Preset> {
        Control::presets(&mut **self)
    }

    fn load_preset(&mut self, id: &PresetId) -> bool {
        Control::load_preset(&mut **self, id)
    }

    fn current_preset(&mut self) -> Option<PresetId> {
        Control::current_preset(&mut **self)
    }

    fn set_automation_state(&mut self, mode: AutomationMode) {
        Control::set_automation_state(&mut **self, mode);
    }

    fn rescan_parameters(&mut self, scope: RescanScope) {
        Control::rescan_parameters(&mut **self, scope);
    }

    /// A tick is a control-side moment: plugins whose last holder was a
    /// Processor are destroyed here, not on its thread.
    fn flush_edits(&mut self) {
        crate::shell::reap();
    }
}

impl Controller for Vst2Controller {
    type Loaded = Vst2Loaded;
    type Processor = Vst2Processor;

    fn latency(&self) -> Samples {
        self.latency_samples()
    }

    fn tail(&self) -> PluginTail {
        Vst2Loaded::tail(self)
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
        prepare: Prepare,
    ) -> Result<(Vst2Processor, Vst2Controller), Refused<Self>> {
        tutti_plugin_types::assert_main_thread();
        crate::shell::warm_retirement();
        crate::shell::reap();
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
        self.handle.instance.set_sample_rate(rate as f32);
        self.host.set_block_size(block);
        self.handle.instance.set_block_size(block as i64);
        self.host.set_offline(prepare.mode().is_offline());
        // `effSetProcessPrecision` is a suspended-state opcode, and a plugin that
        // switches its internal precision on it reallocates the same buffers
        // `effMainsChanged` does.
        self.handle.instance.set_precision(double_precision);

        self.handle.instance.resume();
        self.handle.resumed = true;
        // The bracket every rejoin repeats: `effStartProcess` after
        // `effMainsChanged(1)`. A plugin that arms its DSP on the start edge
        // would otherwise render dry until the first block.
        self.handle.instance.start_process();

        // The pins as the plugin has them now: it may have settled them in the
        // calls above.
        let (inputs, outputs) = self.pins();
        // SAFETY: the handle goes to the Processor, which holds an `Arc` of the
        // shell that owns this instance, so the instance outlives the handle
        // (shell clauses 1 and 4).
        let render = unsafe { self.handle.instance.render_handle() };
        let scratch = RenderScratch::new(inputs, outputs, block);
        let params = SendParams(Arc::clone(&self.params.0));
        let param_count = self.initial_values.len();
        let host = Arc::clone(&self.host);

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

        let shell = Arc::new(Shell::new(self));
        let processor = Vst2Processor::new(ProcessorParts {
            shell: Arc::clone(&shell),
            render,
            params,
            param_count,
            scratch,
            audio,
            block_size: block,
            double_precision,
            host,
        });
        Ok((processor, Vst2Controller::new(shell)))
    }

    /// Rejoins the halves of one activation: checks they are a pair, stops the
    /// Processor (`effStopProcess`, once), drops it, and suspends the plugin
    /// (`effMainsChanged(0)`, once).
    ///
    /// # Errors
    ///
    /// The halves come back, with the reason, when they are of two activations
    /// or a third handle exists.
    fn deactivate(
        mut processor: Vst2Processor,
        mut controller: Vst2Controller,
    ) -> Result<Self, Stuck<Vst2Processor, Vst2Controller>> {
        tutti_plugin_types::assert_main_thread();
        if !Arc::ptr_eq(processor.shell(), controller.shell()) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the halves are of two activations".into()),
            });
        }
        // The pair and nothing else: one share each.
        if Arc::strong_count(controller.shell()) != 2 {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the activation has a third handle".into()),
            });
        }
        Processor::stop(&mut processor);
        let audio = processor.into_audio();
        let shell = controller
            .shell
            .take()
            .expect("a Controller holds its shell until it is consumed");
        let shell = Arc::try_unwrap(shell)
            .unwrap_or_else(|_| unreachable!("the pair was the only two handles"));
        let mut loaded = shell
            .into_loaded()
            .expect("the plugin stays in the shell until the last half drops");
        loaded.audio = Some(audio);
        loaded.suspend();
        Ok(loaded)
    }
}

impl Vst2Loaded {
    /// `effMainsChanged(0)`, if the plugin is resumed. The Processor has stopped
    /// processing by now (the rejoin stops it, and sends `effStopProcess` only
    /// if it was on), so the SDK's teardown order holds and nothing is sent
    /// twice.
    fn suspend(&mut self) {
        if self.handle.resumed {
            self.handle.instance.suspend();
            self.handle.resumed = false;
        }
    }
}
