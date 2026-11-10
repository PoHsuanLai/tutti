//! VST3 plugin loader: [`Vst3Instance`] drives a `Lifecycle<Vst3Loaded>` and
//! presents it through the capability traits the server still dispatches on.
//!
//! The host crate is the interpreter of the plugin's lifecycle (activation,
//! restarts, requests); this file is a probe and the translation between the
//! core's notices and the server's [`AsyncEvent`]s.

use std::path::Path;
use tutti_plugin_types::MainThread;

use tutti_plugin::server::{
    AudioBufferMut, AutomationMode, ContentScale, EditorSize, LoadedPlugin, Normalized, ParamId,
    ParameterInfo, PluginAudio, PluginDescriptor, PluginEditorHost, PluginError, PluginMeta,
    PluginParams, PluginPresets, PluginResult, PluginState, Preset, PresetId, ProcessContext,
    ProcessOutput, RenderMode, WindowHandle,
};
use tutti_plugin::{BridgeError, LoadStage, Result};
use tutti_plugin_types::{
    Control, Event, Hand, HostOp, Lifecycle, MaxBlock, Notice, Prepare, Request, SampleFormat,
    SampleRate, Samples, Stage, State,
};
use tutti_vst3_host::{LoadOptions, ProcessorAny, Vst3Loaded};

use crate::loaders::common::{Meta, standing};
use crate::plugin::AsyncEvent;

pub use tutti_vst3_host;

/// A VST3 plugin as the server sees it.
pub struct Vst3Instance {
    lifecycle: Lifecycle<Vst3Loaded>,
    state: State,
    hand: Hand<ProcessorAny>,
    meta: Meta,
    /// What the plugin asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// What the host has not yet been told.
    events: Vec<AsyncEvent>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
}

/// Map a `tutti_vst3_host::Vst3Error` to the server's `BridgeError`.
fn map_vst3_error(e: tutti_vst3_host::Vst3Error, path: &Path) -> BridgeError {
    match e {
        tutti_vst3_host::Vst3Error::LoadFailed { path, stage, cause } => BridgeError::LoadFailed {
            path,
            stage,
            reason: cause.to_string(),
        },
        tutti_vst3_host::Vst3Error::PluginError { stage, code } => {
            BridgeError::FormatCode { stage, code }
        }
        _ => BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: e.to_string(),
        },
    }
}

impl Vst3Instance {
    /// Load and activate a VST3 plugin.
    ///
    /// If `prefer_f64` is `true` and the plugin advertises 64-bit support, the
    /// instance is activated at 64-bit; otherwise 32-bit. The chosen format is
    /// what `loaded().features` reports as [`Features::F64_AUDIO`], so the
    /// session's negotiated format and the activation agree: the session sends
    /// the buffer width it negotiated, and an instance activated in the other
    /// width would refuse every block.
    ///
    /// [`Features::F64_AUDIO`]: tutti_plugin::server::Features::F64_AUDIO
    #[cfg(test)]
    pub fn load(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
    ) -> Result<Self> {
        Self::load_with_state(
            main,
            path,
            sample_rate,
            block_size,
            prefer_f64,
            None,
            &mut || {},
        )
        .map(|(instance, _)| instance)
    }

    /// `load`, restoring `state` **before the plugin is
    /// activated**: the format allows a state on a plugin that is open and not
    /// yet active, and a plugin may take its mode from the state when it is
    /// activated. The second value is the plugin's refusal of the state, if it
    /// refused: the plugin is then activated at its defaults.
    pub fn load_with_state(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> Result<(Self, Option<String>)> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let loaded = Vst3Loaded::load(main, &resolved, LoadOptions::new())
            .map_err(|e| map_vst3_error(e, path))?;
        let format = if prefer_f64 && loaded.plugin_info().supports_f64 {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        };
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Activation,
                reason: format!(
                    "activate failed: the plugin cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                ),
            })?
            .with_format(format);

        let mut instance = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<Vst3Loaded>::initial_state(),
            hand: Hand::local(),
            meta: Meta::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        let state_error = state.and_then(|data| {
            before_restore();
            match instance.lifecycle.control() {
                Some(control) => control.load_state(main, data).err().map(|e| e.to_string()),
                None => Some("the plugin is lost".to_string()),
            }
        });
        let refused = instance.drive(main, Event::Op(HostOp::Activate(prepare)));
        if instance.state.stage() != Stage::Active {
            let reason = refused.map_or_else(|| "refused".to_string(), |r| r.to_string());
            return Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Activation,
                reason: format!("activate failed: {reason}"),
            });
        }
        instance.refresh_meta(main);
        // Whatever the activation told the host is what the load reply carries.
        instance.events.clear();
        Ok((instance, state_error))
    }

    /// Steps one event through the core, applying its effects. Returns the
    /// reason of a refusal it told, if any.
    fn drive(&mut self, main: &MainThread, event: Event) -> Option<PluginError> {
        let mut told = Vec::new();
        // Taken, not cloned: a clone copies the layout's `Vec`s. The state put
        // in its place is built without allocating and is replaced below.
        let state = std::mem::replace(&mut self.state, Lifecycle::<Vst3Loaded>::initial_state());
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, main, |n| told.push(n));
        let mut refusal = None;
        for notice in told {
            if let Some(reason) = self.absorb(main, notice) {
                refusal = Some(reason);
            }
        }
        refusal
    }

    /// Turns a notice into what the host hears, keeping the cached metadata in
    /// step. Returns the reason when the notice is a refusal.
    fn absorb(&mut self, main: &MainThread, notice: Notice) -> Option<PluginError> {
        match notice {
            Notice::LatencyChanged(samples) => {
                self.meta.loaded.latency_samples = samples;
                self.events.push(AsyncEvent::LatencyChanged { samples });
            }
            Notice::TailChanged(tail) => {
                self.meta.loaded.tail = tail;
                self.events.push(AsyncEvent::TailChanged { tail });
            }
            Notice::IoChanged => {
                self.refresh_meta(main);
                self.events.push(AsyncEvent::IoChanged);
            }
            Notice::ParamValuesChanged => self.events.push(AsyncEvent::ParamValuesChanged),
            Notice::ParamTitlesChanged => self.events.push(AsyncEvent::ParamTitlesChanged),
            Notice::Reloaded => self.events.push(AsyncEvent::Reloaded),
            // The wire carries no note-expression change, and nothing here acts
            // on one: the host reads the list when it opens a session.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("VST3 plugin refused {} Hz: {reason}", rate.0);
                // Whether the host must re-read the instance depends on whether
                // a rollback revives it: `finish_refusals` decides.
                self.refresh_meta(main);
                self.refused = true;
                return Some(reason);
            }
            Notice::Stuck { reason } => {
                tracing::warn!("VST3 plugin would not stop: {reason}");
            }
            Notice::Lost { reason } => {
                tracing::error!("VST3 plugin lost: {reason}");
                // The wire has no "lost" message, and a new one would be a
                // protocol change. `IoChanged` is the closest signal: it makes
                // the host re-read an instance that now renders silence.
                self.events.push(AsyncEvent::IoChanged);
                return Some(reason);
            }
        }
        None
    }

    /// Tells the host a plugin that stayed inactive after a refusal now renders
    /// silence, so it re-reads it. A refusal a rollback undid tells nothing.
    fn finish_refusals(&mut self) {
        if std::mem::take(&mut self.refused) && !self.is_active() {
            self.events.push(AsyncEvent::IoChanged);
        }
    }

    /// Delivers what a state save or a fork must see: the writes a full edit
    /// ring refused are retried now rather than at the next tick.
    ///
    /// A write to the plugin is already applied to its edit controller when it
    /// is made (`setParamNormalized`), which is what the plugin's state reads
    /// for the common plugin whose controller is its component. VST3 has no
    /// out-of-band flush into the processor (CLAP's `params.flush`), so a
    /// plugin with a separate component hears of the write in the next block's
    /// `inputParameterChanges`; and no write needs `process` to be heard, so
    /// no parameter is held back from the controller.
    fn settle_edits(&mut self, main: &MainThread) {
        if let Some(control) = self.lifecycle.control() {
            control.flush_edits(main);
        }
    }

    /// Reads the plugin's wiring again into the metadata the host is given.
    fn refresh_meta(&mut self, main: &MainThread) {
        if let Some(control) = self.lifecycle.control() {
            self.meta.descriptor = control.info().clone();
            self.meta.loaded = control.wiring(main);
        }
    }

    /// Acts on what the plugin asked of the host since the last call, and
    /// returns what the host should be told.
    ///
    /// The core decides what the requests need (a restart for `kIoChanged`,
    /// `kLatencyChanged`, `kPrefetchableSupportChanged` or `kReloadComponent`, a
    /// re-read for the rest) and the lifecycle performs it. The plugin's edits,
    /// progress and unit news are discarded unread (the server has never
    /// forwarded them), so the host's bounded queue holds no backlog.
    ///
    /// Call it between blocks, on the thread that loaded the plugin. A pass in
    /// which the plugin asked for nothing and the core has nothing pending (the
    /// idle pass every dispatch takes) does not enter the lifecycle and
    /// allocates nothing. A pass that has work allocates, as a rate change does:
    /// the core queues its effects.
    pub fn poll(&mut self, main: &MainThread) -> Vec<AsyncEvent> {
        self.serve(main);
        std::mem::take(&mut self.events)
    }

    /// [`poll`](Self::poll)'s pass, leaving what the host should be told in
    /// `events`.
    fn serve(&mut self, main: &MainThread) {
        // A restart the pass performs may itself make the plugin ask for more
        // (an activation that raises `kIoChanged`), so the word is taken again
        // until a pass finds it empty. Bounded, so a plugin that asks from every
        // activation cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for _ in 0..MAX_PASSES {
            requests.clear();
            if let Some(loaded) = self.loaded_ref() {
                loaded.discard_notifications();
                loaded.take_requests(main, &mut requests);
            }
            let asked = !requests.is_empty();
            if !asked && self.state.is_quiet() {
                // All a tick would do: retry what a full edit ring refused.
                if let Some(control) = self.lifecycle.control() {
                    control.flush_edits(main);
                }
                break;
            }
            for request in requests.drain(..) {
                self.drive(main, Event::Request(request));
            }
            self.drive(main, Event::Tick);
            if !asked {
                break;
            }
        }
        self.requests = requests;
        self.finish_refusals();
    }

    /// The plugin, whichever half of an activation holds it.
    fn loaded_ref(&self) -> Option<&Vst3Loaded> {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller.loaded()),
            Lifecycle::Poisoned => None,
        }
    }

    /// [`loaded_ref`](Self::loaded_ref), exclusively.
    fn loaded_mut(&mut self) -> Option<&mut Vst3Loaded> {
        match &mut self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller.loaded_mut()),
            Lifecycle::Poisoned => None,
        }
    }

    /// Whether the plugin is active: a refused rate leaves it loaded and
    /// silent.
    fn is_active(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Active { .. })
    }

    /// Deactivates and activates at `wanted`, as a rate or mode change does.
    /// A refusal is rolled back: the plugin is activated again at the
    /// configuration it already accepted. Returns the refusal and whether the
    /// rollback took, or `Ok` when `wanted` did.
    fn reconfigure(
        &mut self,
        main: &MainThread,
        previous: Prepare,
        wanted: Prepare,
    ) -> std::result::Result<(), (PluginError, bool)> {
        let result = match self.drive(main, Event::Op(HostOp::Reconfigure(wanted))) {
            None => Ok(()),
            Some(reason) => {
                // Put back what the plugin accepted once.
                let rolled_back = self
                    .drive(main, Event::Op(HostOp::Activate(previous)))
                    .is_none()
                    && self.is_active();
                if rolled_back {
                    self.refresh_meta(main);
                }
                Err((reason, rolled_back))
            }
        };
        self.finish_refusals();
        result
    }
}

impl PluginMeta for Vst3Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for Vst3Instance {
    fn process(
        &mut self,
        mut buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        // A rate change the plugin refused left it deactivated (the host raised
        // `IoChanged` when it happened). VST3 allows `process` only while
        // active, so the block is silence and the plugin is not touched.
        match self.lifecycle.processor() {
            Some(processor) => processor.render(&mut buffer, ctx, out),
            None => {
                out.clear();
                match buffer {
                    AudioBufferMut::F32(mut buf) => buf.clear_outputs(),
                    AudioBufferMut::F64(mut buf) => buf.clear_outputs(),
                }
                Ok(())
            }
        }
    }

    /// The rate the plugin already runs at is a no-op, so a per-frame caller
    /// does not tear it down sixty times a second.
    ///
    /// A refusal is rolled back: the plugin is activated again at the
    /// configuration it already accepted, and the error says it is running there.
    /// (The session still renders nothing after an error, because its clock
    /// follows the rate the host asked for.) When the rollback is refused too the
    /// plugin is left loaded and silent, and the host is told through
    /// [`AsyncEvent::IoChanged`]; the next rate revives it.
    ///
    /// The plugin may also change its bus layout and its latency across the
    /// re-activation; the metadata follows them and the host is told.
    fn set_sample_rate(&mut self, main: &MainThread, rate: f64) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "VST3 plugin has no configuration to change".to_string(),
            ));
        };
        if self.is_active() && (previous.rate().0 - rate).abs() < f64::EPSILON {
            return Ok(());
        }
        let Some(wanted) = previous.with_rate(SampleRate(rate)) else {
            return Err(PluginError::Other(format!(
                "plugin refused sample rate {rate} ({}): not a positive finite rate",
                standing(self.is_active(), previous.rate())
            )));
        };
        self.reconfigure(main, previous, wanted)
            .map_err(|(reason, rolled_back)| {
                PluginError::Other(format!(
                    "VST3 plugin refused sample rate {rate} ({}): {reason}",
                    if rolled_back {
                        format!("running at {}", previous.rate().0)
                    } else {
                        "deactivated".to_string()
                    }
                ))
            })
    }

    /// Activates the plugin again in the requested mode, keeping its state.
    ///
    /// VST3 delivers `processMode` through `setupProcessing`, which runs once
    /// per activation and may only run while deactivated
    /// (`ivstaudioprocessor.h:328`). Reaching `kOffline` from `kRealtime`
    /// therefore *requires* a fresh setup, the spec says so explicitly at
    /// `:142-143`, so unlike a format that applies the mode live this is a
    /// restart: the core plans a deactivate and an activate at the new
    /// [`Prepare`], and the instance, its parameters and its state stay.
    ///
    /// A refusal is rolled back to the mode the plugin was running in, and
    /// answers `false`.
    ///
    /// The realtime and prefetch pair is the one exception the spec carves out
    /// and is `Vst3Processor::set_prefetch`, not this; prefetch is not reachable
    /// through [`RenderMode`] by design (see its docs).
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        let Some(previous) = self.state.prepare() else {
            return false;
        };
        if previous.mode() == mode && self.is_active() {
            return true;
        }
        self.reconfigure(main, previous, previous.with_mode(mode))
            .is_ok()
    }
}

impl PluginParams for Vst3Instance {
    /// Normalized `0..=1`, which is VST3's own domain.
    fn get_parameter(&self, main: &MainThread, id: ParamId) -> f64 {
        self.loaded_ref()
            .and_then(|loaded| Control::parameter(loaded, main, id))
            .map_or(0.0, Normalized::get)
    }

    /// Normalized `0..=1` in. On an active plugin the write reaches the edit
    /// controller now and is queued to the Processor, which hands it to the
    /// plugin at the head of the next block.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        if let Some(control) = self.lifecycle.control() {
            control.set_parameter(main, id, value);
        }
    }

    /// VST3's `getParamStringByValue` takes the value already normalized, so
    /// this is the one format where no conversion stands between the seam's
    /// domain and the ABI's.
    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded_ref()?, main, id, value)
    }

    /// The inverse. `getParamValueByString` answers normalized too, so the
    /// result passes through `Normalized::clamped` only.
    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded_ref()?, main, id, text)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        // Runs on the server's main thread (same as set_parameter), satisfying
        // the host's main-thread assertion.
        if let Some(control) = self.lifecycle.control() {
            control.set_automation_state(main, mode);
        }
    }

    fn get_parameter_list(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.loaded_ref()
            .map(|loaded| Control::parameters(loaded, main))
            .unwrap_or_default()
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        self.loaded_ref()
            .map_or(0, |loaded| loaded.parameter_count(main) as usize)
    }
}

impl PluginEditorHost for Vst3Instance {
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        // `parent` was validated at the IPC boundary in server.rs.
        let loaded = self
            .loaded_mut()
            .ok_or_else(|| PluginError::Other("the plugin is lost".to_string()))?;
        loaded
            .open_editor(main, parent, scale)
            .map(|size| EditorSize {
                width: size.width,
                height: size.height,
            })
            .map_err(|e| PluginError::Editor(tutti_plugin::EditorError::PluginError(e.to_string())))
    }

    fn close_editor(&mut self, main: &MainThread) {
        if let Some(loaded) = self.loaded_mut() {
            loaded.close_editor(main);
        }
    }
}

impl PluginPresets for Vst3Instance {
    /// The plugin's programs, flattened out of its named program lists; see
    /// [`Control::presets`].
    fn get_presets(&mut self, main: &MainThread) -> Vec<Preset> {
        self.lifecycle
            .control()
            .map(|loaded| Control::presets(loaded, main))
            .unwrap_or_default()
    }

    /// Select a program by writing the parameter that owns its list; see
    /// [`Control::load_preset`].
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        self.lifecycle
            .control()
            .is_some_and(|control| control.load_preset(main, id))
    }

    /// The program the plugin currently has selected, read back off the same
    /// parameter [`load_preset`](Self::load_preset) writes.
    fn get_current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset(main)
    }
}

impl PluginState for Vst3Instance {
    fn get_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        self.settle_edits(main);
        self.lifecycle
            .control()
            .ok_or_else(|| PluginError::State("the plugin is lost".to_string()))?
            .save_state(main)
    }

    fn set_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()> {
        self.lifecycle
            .control()
            .ok_or_else(|| PluginError::State("the plugin is lost".to_string()))?
            .load_state(main, data)
    }
}

#[cfg(test)]
impl Vst3Instance {
    /// The plugin, whichever half holds it.
    pub(crate) fn vst3_loaded(&self) -> &Vst3Loaded {
        self.loaded_ref().expect("the plugin is not lost")
    }

    /// Calls the host's `restartComponent(flags)` as the plugin would, so a
    /// session test can raise a restart the probe never raises on its own.
    pub(crate) fn plugin_restart_component(&self, flags: i32) {
        use tutti_vst3_host::host::conformance::plugin_restart_component;
        plugin_restart_component(self.vst3_loaded(), flags);
    }
}

#[cfg(test)]
#[cfg(feature = "vst3")]
mod tests {
    use super::*;

    /// This process's main thread. Each test is its own process under nextest, so
    /// the claim cannot be refused.
    fn main_thread() -> tutti_plugin_types::MainThread {
        tutti_plugin_types::MainThread::claim().expect("one test, one process")
    }

    use std::path::Path;
    use tutti_plugin::server::{AudioBuffer, AudioBuffer64, AudioBufferMut, Features, MidiEvent};
    use tutti_ump::{MidiChannel, MidiGroup};

    /// Sets `TUTTI_PROBE_MISBEHAVIOUR` for the life of the guard and restores
    /// the previous value on drop, a panic included. Take it while holding
    /// `plugin_load_lock()`, and drop it before the lock (declare it after).
    struct ProbeMisbehaviour {
        previous: Option<std::ffi::OsString>,
    }

    impl ProbeMisbehaviour {
        fn set(code: &str) -> Self {
            let previous = std::env::var_os("TUTTI_PROBE_MISBEHAVIOUR");
            // SAFETY: the caller holds `plugin_load_lock()`, which every test here that loads
            // the probe (the only reader of this variable) also takes, so none runs meanwhile.
            // Process-wide `std::env` access from code that does not take the lock is not
            // excluded, only absent from this crate's tests.
            unsafe { std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", code) };
            Self { previous }
        }
    }

    impl Drop for ProbeMisbehaviour {
        fn drop(&mut self) {
            // SAFETY: as in `set`; the lock outlives this guard.
            match self.previous.take() {
                Some(v) => unsafe { std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", v) },
                None => unsafe { std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR") },
            }
        }
    }

    /// The reference VST3 plugin, built by `tutti-vst3-host`'s build script
    /// from the in-repo SDK submodules during this same `cargo test`.
    ///
    /// The SDK is a submodule, so these tests run on any machine, like their
    /// CLAP siblings.
    fn vst3_plugin() -> &'static str {
        crate::test_utils::vst3_probe_path()
    }

    /// The SDK's `multiple_programchanges` sample, built from the in-repo SDK
    /// submodule by `tutti-vst3-host`'s build script during this same
    /// `cargo test`.
    ///
    /// It is a build artifact, so absence is a build failure and the tests
    /// assert rather than skip.
    fn multi_program_sample() -> &'static str {
        crate::test_utils::vst3_program_sample_path()
    }

    /// The probe's `kParamMode` (`probeids.h`), and its `kModeActivationCount`
    /// as a normalized mode: `index / kModeStepCount`.
    const PARAM_MODE: u32 = 100;
    const MODE_ACTIVATION_COUNT: f64 = 8.0 / 10.0;
    /// `kActivationCountBase`: mode 8 writes this plus the number of
    /// `setActive(true)` calls.
    const ACTIVATION_COUNT_BASE: f32 = 9000.0;

    /// How many times the probe has been activated, as the probe reports it:
    /// the mode is selected through the host's own write path (queued to the
    /// Processor, which puts it in the block's parameter changes) and one block
    /// is rendered, so the answer is what the plugin saw, not what the host
    /// believes.
    fn activations_seen_by_the_plugin(instance: &mut Vst3Instance) -> u32 {
        instance.set_parameter(
            &main_thread(),
            tutti_plugin_types::ParamId::new(PARAM_MODE),
            Normalized::clamped(MODE_ACTIVATION_COUNT),
        );
        let frames = 128;
        let input = vec![vec![0.0f32; frames]; 3];
        let mut output = vec![vec![0.0f32; frames]; 3];
        let input_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples: frames,
            sample_rate: 48_000.0,
        };
        let ctx = ProcessContext::new();
        let mut out = ProcessOutput::default();
        // The first block carries the write into the plugin; the mode applies
        // from the next.
        instance
            .process(AudioBufferMut::F32(buffer), &ctx, &mut out)
            .expect("renders");
        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples: frames,
            sample_rate: 48_000.0,
        };
        instance
            .process(AudioBufferMut::F32(buffer), &ctx, &mut out)
            .expect("renders");
        (output[0][0] - ACTIVATION_COUNT_BASE) as u32
    }

    /// The restart poll with a plugin's edits waiting in the host's queue,
    /// which is the common case while a user drags a knob in the server-side
    /// editor: the host crate's two reads of the plugin's callbacks do not
    /// allocate, the poll discards the edits (the server never forwards them),
    /// so no backlog builds up, and the coalesced restart reaches the host once.
    ///
    /// The bridge's own pass allocates when it has something to do (the core
    /// copies its state and queues the effects), as a CLAP pass does; what is
    /// gated is the part that runs when the plugin asked for nothing.
    ///
    /// Mutations (run): `take_requests` collecting the requests into a fresh
    /// `Vec` aborts inside the first gate; dropping the `discard_notifications`
    /// call from the gated closure fails the "queue is empty" assertion; making
    /// `RestartFlags::raise_into` raise nothing for `kParamValuesChanged` fails
    /// the `RescanParams(Values)` assertion.
    #[test]
    fn the_request_reads_do_not_allocate_with_edits_pending() {
        use tutti_vst3_host::host::conformance::{plugin_perform_edit, plugin_restart_component};
        // `RestartFlags_::kParamValuesChanged` (`ivsteditcontroller.h`). The
        // assertion on the request below pins that this is the bit.
        const K_PARAM_VALUES_CHANGED: i32 = 1 << 2;

        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        let mut requests: Vec<Request> = Vec::with_capacity(16);

        // Nothing pending: the idle read every pass takes.
        assert_no_alloc::assert_no_alloc(|| {
            let loaded = instance.vst3_loaded();
            loaded.discard_notifications();
            loaded.take_requests(&main_thread(), &mut requests);
        });
        assert!(requests.is_empty());

        let loaded = instance.vst3_loaded();
        for i in 0..64 {
            plugin_perform_edit(loaded, 100, f64::from(i) / 64.0);
        }
        plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
        plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
        // The gate below is only meaningful if the edits are really there.
        assert_eq!(loaded.dropped_notifications(), 0);
        assert_eq!(loaded.pending_notifications(), 64, "the edits are queued");

        assert_no_alloc::assert_no_alloc(|| {
            loaded.discard_notifications();
            loaded.take_requests(&main_thread(), &mut requests);
        });
        assert_eq!(
            loaded.pending_notifications(),
            0,
            "the poll discards what the server never forwards"
        );
        assert_eq!(
            requests,
            [Request::RescanParams(
                tutti_plugin_types::RescanScope::Values
            )],
            "two flags are one request"
        );
        requests.clear();
        loaded.take_requests(&main_thread(), &mut requests);
        assert!(requests.is_empty(), "a take consumes the requests");

        // And the bridge turns one into the host's event, once.
        plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
        assert!(matches!(
            instance.poll(&main_thread()).as_slice(),
            [AsyncEvent::ParamValuesChanged]
        ));
        assert!(
            instance.poll(&main_thread()).is_empty(),
            "a poll consumes the restart"
        );
    }

    /// **A latency that moves with the rate is read again after a rate
    /// change** and reaches the next poll, with no `restartComponent` from the
    /// plugin. The figure was read inside the host crate and dropped, so PDC
    /// kept the old rate's.
    ///
    /// Mutation (run): delete the `self.events.push(AsyncEvent::LatencyChanged
    /// { samples })` from `absorb` → the poll carries no latency → fails.
    #[test]
    fn a_latency_that_follows_the_rate_is_reread_after_a_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        // `kMisbehaveLatencyFollowsRate`: the latency is the rate in kHz.
        let _misbehaviour = ProbeMisbehaviour::set("12");
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            44_100.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        assert_eq!(instance.meta.loaded.latency_samples, Samples(44));
        assert!(instance.poll(&main_thread()).is_empty());

        instance
            .set_sample_rate(&main_thread(), 96_000.0)
            .expect("rate change");

        assert_eq!(instance.meta.loaded.latency_samples, Samples(96));
        assert!(
            matches!(
                instance.poll(&main_thread()).as_slice(),
                [AsyncEvent::LatencyChanged { samples }] if *samples == Samples(96)
            ),
            "the new figure reaches the host"
        );
        assert!(instance.poll(&main_thread()).is_empty(), "once");
    }

    /// **`kPrefetchableSupportChanged` runs the deactivate and activate the
    /// header asks for.** The flag was decoded and dropped. The probe counts its
    /// own activations, which is what the host cannot see.
    ///
    /// Mutation (run): `RestartFlags::raise_into` raising nothing for
    /// `prefetchable_support_changed` → the plugin is not activated again →
    /// fails.
    #[test]
    fn a_prefetchable_support_change_restarts_the_plugin() {
        // `RestartFlags_::kPrefetchableSupportChanged`.
        const K_PREFETCHABLE_SUPPORT_CHANGED: i32 = 1 << 8;
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        assert_eq!(
            activations_seen_by_the_plugin(&mut instance),
            1,
            "a fresh load activates once"
        );
        assert!(instance.poll(&main_thread()).is_empty());

        instance.plugin_restart_component(K_PREFETCHABLE_SUPPORT_CHANGED);

        let events = instance.poll(&main_thread());
        assert!(
            !events.iter().any(|e| matches!(e, AsyncEvent::IoChanged)),
            "the layout was not asked about: {events:?}"
        );
        assert_eq!(
            activations_seen_by_the_plugin(&mut instance),
            2,
            "the plugin was deactivated and activated again"
        );
    }

    /// **`kParamIDMappingChanged` reaches the client as a parameter-list
    /// change**, so it re-pulls the ids; it was decoded and dropped, and every
    /// lane that named a remapped id detached.
    ///
    /// Mutation (run): `RestartFlags::raise_into` not raising the rescan for
    /// `param_id_mapping_changed` → fails.
    #[test]
    fn a_param_id_remap_is_reported_as_a_changed_list() {
        // `RestartFlags_::kParamIDMappingChanged`.
        const K_PARAM_ID_MAPPING_CHANGED: i32 = 1 << 11;
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");

        instance.plugin_restart_component(K_PARAM_ID_MAPPING_CHANGED);

        assert!(
            instance
                .poll(&main_thread())
                .iter()
                .any(|e| matches!(e, AsyncEvent::ParamTitlesChanged))
        );
    }

    /// **A rate the plugin refuses is rolled back to the one it already
    /// accepted**, and the session keeps running: the error says it is running
    /// there, the plugin is active and has the layout it had, and the next rate
    /// it takes is taken.
    ///
    /// Mutation (run): `reconfigure` not activating again at the previous
    /// `Prepare` (the `Activate(previous)` drive removed) → the plugin is left
    /// loaded and silent → the `is_active` assertion fails.
    #[test]
    fn a_refused_rate_is_rolled_back_to_the_previous_one() {
        let _lock = crate::test_utils::plugin_load_lock();
        // `kMisbehaveRefusesHighRates`: `setActive(true)` fails above 50 kHz.
        let _misbehaviour = ProbeMisbehaviour::set("14");
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        assert!(instance.poll(&main_thread()).is_empty());
        let layout = instance.meta.loaded.clone();

        let error = instance
            .set_sample_rate(&main_thread(), 96_000.0)
            .expect_err("the probe refuses 96 kHz");
        assert!(
            error.to_string().contains("running at 48000"),
            "the error says where it runs: {error}"
        );
        assert!(instance.is_active(), "the rollback revived the plugin");
        // The core tells the host to re-read a plugin it revived (`IoChanged`);
        // what the host reads is the layout the plugin had.
        let told = instance.poll(&main_thread());
        assert!(
            told.iter().all(|e| matches!(e, AsyncEvent::IoChanged)),
            "{told:?}"
        );
        assert_eq!(instance.meta.loaded.inputs, layout.inputs);
        assert_eq!(instance.meta.loaded.outputs, layout.outputs);
        assert_eq!(activations_seen_by_the_plugin(&mut instance), 2);

        instance
            .set_sample_rate(&main_thread(), 44_100.0)
            .expect("a rate it takes is taken");
        assert!(instance.is_active());
    }

    /// **A rate that is no rate is refused before the plugin is touched**: the
    /// instance runs on at the configuration it has, the plugin sees no second
    /// activation, the host is told nothing, and the error says where the plugin
    /// runs.
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every rate
    /// -> the zero rate reaches the reconfigure -> the `expect_err` fails (or the
    /// activation count moves).
    #[test]
    fn a_rate_that_is_no_rate_is_refused_before_the_plugin_is_touched() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        assert!(instance.poll(&main_thread()).is_empty());
        let layout = instance.meta.loaded.clone();
        let prepare = instance.state.prepare();
        for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = instance
                .set_sample_rate(&main_thread(), rate)
                .expect_err("a rate that is no rate is refused");
            assert!(error.to_string().contains("running at 48000"), "{error}");
            assert!(instance.is_active(), "{rate}");
        }
        assert_eq!(instance.state.prepare(), prepare);
        assert!(
            instance.poll(&main_thread()).is_empty(),
            "the host is told nothing"
        );
        assert_eq!(instance.meta.loaded.inputs, layout.inputs);
        assert_eq!(instance.meta.loaded.outputs, layout.outputs);
        assert_eq!(
            activations_seen_by_the_plugin(&mut instance),
            1,
            "the plugin was never activated again"
        );
        instance
            .set_sample_rate(&main_thread(), 44_100.0)
            .expect("a real rate");
    }

    /// **A load at no rate or no block is refused** at the activation stage:
    /// zero, negative and non-finite rates, and a block of no frames. A real load
    /// afterwards activates once.
    ///
    /// Mutation (run): `valid_rate` accepting every rate, and `MaxBlock::new`
    /// accepting zero, each -> the matching case loads -> fails.
    #[test]
    fn a_load_at_no_rate_or_no_block_is_refused() {
        let _lock = crate::test_utils::plugin_load_lock();
        for (rate, block) in [
            (0.0, 128),
            (-48_000.0, 128),
            (f64::NAN, 128),
            (f64::INFINITY, 128),
            (f64::NEG_INFINITY, 128),
            (48_000.0, 0),
        ] {
            match Vst3Instance::load(&main_thread(), Path::new(vst3_plugin()), rate, block, false) {
                Err(BridgeError::LoadFailed {
                    stage: LoadStage::Activation,
                    reason,
                    ..
                }) => assert!(reason.contains("cannot run at"), "{reason}"),
                Err(other) => panic!("{rate} Hz in {block} frames: {other:?}"),
                Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
            }
        }
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("a real load");
        assert_eq!(activations_seen_by_the_plugin(&mut instance), 1);
    }

    /// **A render mode is the activation's: changing it activates the plugin
    /// again in the new mode**, keeping the instance, and a mode the plugin
    /// refuses is rolled back and answered `false`. VST3 delivers the mode in
    /// `setupProcessing` and nowhere else, so (unlike a format that applies it
    /// live) the change is a restart, and the core's `Prepare` carries the mode
    /// the restart uses.
    ///
    /// Mutation (run): `set_render_mode` recording the mode without reconfiguring
    /// (answering `true` at once) → the plugin is never activated again → the
    /// activation count stays 1 → fails; not rolling a refusal back → the
    /// `is_active` assertion fails.
    #[test]
    fn a_render_mode_change_activates_the_plugin_again_in_the_new_mode() {
        let _lock = crate::test_utils::plugin_load_lock();
        // `kMisbehaveRefusesOffline`: `setActive(true)` fails under `kOffline`.
        let _misbehaviour = ProbeMisbehaviour::set("15");
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(vst3_plugin()),
            48_000.0,
            128,
            false,
        )
        .expect("the vendored VST3 probe loads");
        assert_eq!(activations_seen_by_the_plugin(&mut instance), 1);

        assert!(
            !instance.set_render_mode(&main_thread(), RenderMode::Offline),
            "the probe refuses offline"
        );
        assert!(instance.is_active(), "the refusal was rolled back");
        assert_eq!(
            instance.state.prepare().map(|p| p.mode()),
            Some(RenderMode::Realtime),
            "and the mode the core holds is the one that runs"
        );
        assert_eq!(
            activations_seen_by_the_plugin(&mut instance),
            2,
            "the rollback activated the plugin once"
        );

        assert!(
            instance.set_render_mode(&main_thread(), RenderMode::Realtime),
            "the mode it already has is taken without a restart"
        );
        assert_eq!(activations_seen_by_the_plugin(&mut instance), 2);
    }

    /// A VST3 program keeps both coordinates, and its list's name.
    ///
    /// The SDK's `multiple_programchanges` sample is the fixture that makes
    /// this witnessable: it builds 16 program lists whose ids are
    /// `kProgramStartId + i`, so a **list id is provably not a position** in
    /// `program_lists()`. A mapping that dropped `list_id` and kept `index`
    /// would collapse all 16 lists onto one another — 2048 presets becoming
    /// 128 — which is what the count assertion catches.
    ///
    /// `bank` carries the list name because VST3 is the one format whose
    /// presets are grouped; the other three expose a single flat set and
    /// report `None`.
    #[test]
    fn a_vst3_program_keeps_its_list_id_and_bank() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(multi_program_sample()),
            44_100.0,
            512,
            false,
        )
        .expect("the multiple-program-changes sample loads");

        let presets = instance.get_presets(&main_thread());
        assert!(
            presets.len() > 128,
            "16 lists of 128 programs must not collapse onto one another; got {}",
            presets.len()
        );

        // Every id keeps both coordinates, and more than one distinct list id
        // appears — the property a flattened mapping would destroy.
        let lists: std::collections::BTreeSet<i32> = presets
            .iter()
            .filter_map(|p| match &p.id {
                PresetId::Program { list_id, .. } => Some(*list_id),
                _ => None,
            })
            .collect();
        assert!(
            lists.len() > 1,
            "the sample publishes several program lists; saw {lists:?}"
        );
        // The load-bearing half, and the one a distinctness check misses: the
        // sample's ids are `kProgramStartId + i` = 1..=16, so a host that
        // substituted the *position* would report 0..=15 — still 16 distinct
        // values, still one bank each, still one index-0 per list. Every other
        // assertion here passes under that bug; only this one sees it.
        assert!(
            !lists.contains(&0),
            "list ids are `kProgramStartId + i` (1..=16), so a 0 means the host \
             reported a position where the plugin gave an id: {lists:?}"
        );
        assert_eq!(
            lists.len(),
            presets
                .iter()
                .filter_map(|p| p.bank.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "each list must contribute its own bank name"
        );

        // The same index appears in every list, so an id that dropped its
        // list would be ambiguous.
        let index_zero: Vec<&Preset> = presets
            .iter()
            .filter(|p| matches!(p.id, PresetId::Program { index: 0, .. }))
            .collect();
        assert_eq!(
            index_zero.len(),
            lists.len(),
            "index 0 exists once per list; an id without list_id could not tell them apart"
        );
    }

    /// A program listed through the loader loads, and reads back.
    ///
    /// VST3 has no load-preset call — this *is* the load, routed through the
    /// `kIsProgramChange` parameter that owns the list, so a caller does not
    /// have to find and drive that parameter itself.
    ///
    /// Round-tripped rather than merely accepted: `load_preset` returning
    /// `true` says the parameter was written, and only reading it back through
    /// `get_current_preset` shows the write landed on the program asked for.
    /// That is what catches the normalization off-by-one — `i / (N - 1)`, not
    /// `i / N` — which lands on a neighbour rather than failing.
    #[test]
    fn a_vst3_program_loads_and_reads_back() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(multi_program_sample()),
            44_100.0,
            512,
            false,
        )
        .expect("the multiple-program-changes sample loads");

        let presets = instance.get_presets(&main_thread());
        assert!(presets.len() > 2, "the sample publishes many programs");

        // Taken from the listing, never constructed.
        let wanted = presets[2].id.clone();
        assert!(
            instance.load_preset(&main_thread(), &wanted),
            "a program the plugin listed must load"
        );
        assert_eq!(
            instance.get_current_preset(&main_thread()),
            Some(wanted.clone()),
            "the plugin must report the program just written"
        );

        // The round trip alone cannot catch the off-by-one: `get_current_preset`
        // inverts the *same* formula, so a wrong divisor agrees with itself.
        // Only the absolute value the plugin now holds distinguishes them.
        // Measured on the SDK sample: 128 programs -> `step_count == 127`, so
        // index 2 is 2/127 = 0.015748…, where `i / N` would give 2/128 =
        // 0.015625 and select a neighbouring program on a longer list.
        let PresetId::Program { list_id, index } = &wanted else {
            unreachable!("VST3 ids are programs")
        };
        let (param_id, step_count) = instance
            .vst3_loaded()
            .program_change_parameter(&main_thread(), *list_id)
            .expect("the list has a program-change parameter");
        assert_eq!(step_count, 127, "128 programs report 127 steps, not 128");
        let raw = Control::parameter(instance.vst3_loaded(), &main_thread(), param_id)
            .expect("the plugin has a controller")
            .get();
        let expected = f64::from(*index) / f64::from(step_count);
        assert!(
            (raw - expected).abs() < 1e-9,
            "wrote {raw}, expected {expected} — the divisor is `step_count`, \
             not the program count"
        );
    }

    /// An id from another format is refused rather than coerced.
    ///
    /// `Number` and `Location` name nothing in VST3's `(list, index)` space.
    /// A `Number` is the dangerous one: it *looks* like a program index, and
    /// treating it as one would write a real program in whichever list
    /// happened to be found first.
    #[test]
    fn a_vst3_load_refuses_an_id_from_another_format() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = Vst3Instance::load(
            &main_thread(),
            Path::new(multi_program_sample()),
            44_100.0,
            512,
            false,
        )
        .expect("the multiple-program-changes sample loads");

        let before = instance.get_current_preset(&main_thread());
        assert!(
            !instance.load_preset(&main_thread(), &PresetId::Number(1)),
            "an AU selector / VST2 index addresses no VST3 program"
        );
        assert!(
            !instance.load_preset(&main_thread(), &PresetId::Location("/x.clap-preset".into())),
            "a CLAP path addresses no VST3 program"
        );
        assert_eq!(
            instance.get_current_preset(&main_thread()),
            before,
            "a refused load must not move the program"
        );
    }

    #[test]
    fn test_vst3_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load VST3 plugin: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");
    }

    #[test]
    fn test_vst3_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");
        let outputs = instance.loaded().total_outputs();

        assert!(outputs > 0, "Expected audio outputs > 0, got {outputs}");
    }

    #[test]
    fn test_vst3_parameter_count() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");

        let count = instance.get_parameter_list(&main_thread()).len();
        assert!(
            count > 0,
            "TAL-NoiseMaker should have parameters, got {}",
            count
        );
    }

    #[test]
    fn test_vst3_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");

        let params = instance.get_parameter_list(&main_thread());
        assert!(
            !params.is_empty(),
            "Parameter list should not be empty for TAL-NoiseMaker"
        );

        for param in &params {
            assert!(
                !param.name.is_empty(),
                "Parameter id {} has empty name",
                param.id
            );
        }
    }

    #[test]
    fn test_vst3_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = PluginParams::get_parameter(&instance, &main_thread(), first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    #[test]
    fn test_vst3_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let mut instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        // Should not panic
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(AudioBufferMut::F32(buffer), &ctx, &mut out);
    }

    #[test]
    fn test_vst3_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(vst3_plugin());
        let mut instance = Vst3Instance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let note_on = [MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut has_nonzero = false;

        // Process block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = tutti_plugin::server::ProcessContext::new().midi(&note_on);
            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(AudioBufferMut::F32(buffer), &ctx, &mut out);

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks to give the synth time to produce sound
        let empty_ctx = tutti_plugin::server::ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let mut out = tutti_plugin::server::ProcessOutput::default();

            let _output = instance.process(AudioBufferMut::F32(buffer), &empty_ctx, &mut out);

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        assert!(
            has_nonzero,
            "Expected at least one non-zero output sample after NoteOn"
        );
    }

    // =========================================================================
    // Voxengo SPAN / Boogex — f64 support tests (local fixture plugins)
    // =========================================================================

    const SPAN_VST3: &str = "tests/fixtures/plugins/SPAN.vst3";
    const BOOGEX_VST3: &str = "tests/fixtures/plugins/Boogex.vst3";

    /// Resolve fixture path relative to workspace root.
    fn fixture_path(relative: &str) -> std::path::PathBuf {
        tutti_fixtures::repo_root().join(relative)
    }

    #[test]
    fn test_voxengo_span_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&main_thread(), &path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load SPAN: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        eprintln!(
            "SPAN: name={}, supports_f64={}",
            instance.descriptor().name,
            instance.loaded().features.contains(Features::F64_AUDIO)
        );
        assert!(!instance.descriptor().name.is_empty());
    }

    #[test]
    fn test_voxengo_boogex_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&main_thread(), &path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load Boogex: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        eprintln!(
            "Boogex: name={}, supports_f64={}",
            instance.descriptor().name,
            instance.loaded().features.contains(Features::F64_AUDIO)
        );
        assert!(!instance.descriptor().name.is_empty());
    }

    #[test]
    fn test_voxengo_span_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance = Vst3Instance::load(&main_thread(), &path, 44100.0, 512, true)
            .expect("Failed to load SPAN");

        if !instance.loaded().features.contains(Features::F64_AUDIO) {
            eprintln!("SPAN does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a 440Hz sine wave to test pass-through
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx, &mut out);

        // SPAN is an analyzer — it should pass audio through unchanged
        let mut all_zero = true;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    all_zero = false;
                    break;
                }
            }
        }
        eprintln!(
            "SPAN f64 output: first sample = {}, all_zero = {}",
            output_data[0][0], all_zero
        );
    }

    #[test]
    fn test_voxengo_boogex_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance = Vst3Instance::load(&main_thread(), &path, 44100.0, 512, true)
            .expect("Failed to load Boogex");

        if !instance.loaded().features.contains(Features::F64_AUDIO) {
            eprintln!("Boogex does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a sine wave — Boogex is an amp sim so it should transform the audio
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx, &mut out);

        let mut has_nonzero = false;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    has_nonzero = true;
                    break;
                }
            }
        }
        eprintln!(
            "Boogex f64 output: first sample = {}, has_nonzero = {}",
            output_data[0][0], has_nonzero
        );
    }
}
