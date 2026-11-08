//! VST2 plugin loader: [`Vst2Instance`] drives a `Lifecycle<Vst2Loaded>` and
//! presents it through the capability traits the server still dispatches on.
//!
//! The host crate is the interpreter of the plugin's lifecycle (activation,
//! requests); this file is a probe and the translation between the core's
//! notices and the server's [`AsyncEvent`]s.

use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, ContentScale, EditorSize, LoadedPlugin, Normalized, ParamId, ParameterInfo,
    PluginAudio, PluginDescriptor, PluginEditorHost, PluginError, PluginMeta, PluginParams,
    PluginPresets, PluginResult, PluginState, Preset, PresetId, ProcessContext, ProcessOutput,
    RenderMode, WindowHandle,
};
use tutti_plugin::{BridgeError, LoadStage, Result};
use tutti_plugin_types::{
    Control, Event, Hand, HostOp, Lifecycle, MaxBlock, Notice, Prepare, Request, SampleFormat,
    SampleRate, Samples, Stage, State,
};
use tutti_vst2_host::{Vst2Error, Vst2Loaded, Vst2Processor};

use crate::loaders::common::{Meta, standing};
use crate::plugin::AsyncEvent;

/// A VST2 plugin as the server sees it.
pub struct Vst2Instance {
    lifecycle: Lifecycle<Vst2Loaded>,
    state: State,
    hand: Hand<Vst2Processor>,
    meta: Meta,
    /// What the plugin asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// What the host has not yet been told.
    events: Vec<AsyncEvent>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
}

// SAFETY: the host instances are `Send`, and the instance is driven from one
// thread at a time.
unsafe impl Send for Vst2Instance {}

fn translate_error(err: Vst2Error, _path: &Path) -> BridgeError {
    match err {
        Vst2Error::LoadFailed {
            path,
            stage,
            reason,
        } => BridgeError::LoadFailed {
            path,
            // Host and bridge LoadStage are the same shared type
            // (tutti-plugin-types); no conversion needed.
            stage,
            reason,
        },
        Vst2Error::EditorError(s) => BridgeError::EditorError(s),
        Vst2Error::StateError(s) => BridgeError::StateRestoreError(s),
        // Deliberate fallback: `Vst2Error` is `#[non_exhaustive]`.
        other => BridgeError::ProcessError(other.to_string()),
    }
}

impl Vst2Instance {
    /// Lightweight probe: open the plugin and read its identity, without
    /// activating it.
    pub fn probe(path: &Path) -> Result<PluginDescriptor> {
        let loaded = Vst2Loaded::load(path).map_err(|e| translate_error(e, path))?;
        Ok(Control::info(&loaded).clone())
    }

    /// Load and activate a VST2 plugin.
    ///
    /// `prefer_f64` is the caller's request for 64-bit blocks: the plugin is told
    /// `effSetProcessPrecision(64)` only when it also declared
    /// `effFlagsCanDoubleReplacing` (the same condition that negotiates
    /// `SampleFormat::Float64` with the host), so the width announced is the
    /// width `process` is entered at.
    pub fn load(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
    ) -> Result<Self> {
        let loaded = Vst2Loaded::load(path).map_err(|e| translate_error(e, path))?;
        let format = if prefer_f64 && loaded.metadata().supports_f64 {
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
            state: Lifecycle::<Vst2Loaded>::initial_state(),
            hand: Hand::local(),
            meta: Meta::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        let refused = instance.drive(Event::Op(HostOp::Activate(prepare)));
        if instance.state.stage() != Stage::Active {
            let reason = refused.map_or_else(|| "refused".to_string(), |r| r.to_string());
            return Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Activation,
                reason: format!("activate failed: {reason}"),
            });
        }
        instance.refresh_meta();
        // Whatever the activation told the host is what the load reply carries.
        instance.events.clear();
        Ok(instance)
    }

    /// Steps one event through the core, applying its effects. Returns the
    /// reason of a refusal it told, if any.
    fn drive(&mut self, event: Event) -> Option<PluginError> {
        let mut told = Vec::new();
        // Taken, not cloned: a clone copies the layout's `Vec`s. The state put
        // in its place is built without allocating and is replaced below.
        let state = std::mem::replace(&mut self.state, Lifecycle::<Vst2Loaded>::initial_state());
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, |n| told.push(n));
        let mut refusal = None;
        for notice in told {
            if let Some(reason) = self.absorb(notice) {
                refusal = Some(reason);
            }
        }
        refusal
    }

    /// Turns a notice into what the host hears, keeping the cached metadata in
    /// step. Returns the reason when the notice is a refusal.
    fn absorb(&mut self, notice: Notice) -> Option<PluginError> {
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
                self.refresh_meta();
                self.events.push(AsyncEvent::IoChanged);
            }
            Notice::ParamValuesChanged => self.events.push(AsyncEvent::ParamValuesChanged),
            Notice::ParamTitlesChanged => self.events.push(AsyncEvent::ParamTitlesChanged),
            Notice::Reloaded => self.events.push(AsyncEvent::Reloaded),
            // VST2 has no note expression; the core never raises it here.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("VST2 plugin refused {} Hz: {reason}", rate.0);
                // Whether the host must re-read the instance depends on whether
                // a rollback revives it: `finish_refusals` decides.
                self.refresh_meta();
                self.refused = true;
                return Some(reason);
            }
            Notice::Stuck { reason } => {
                tracing::warn!("VST2 plugin would not stop: {reason}");
            }
            Notice::Lost { reason } => {
                tracing::error!("VST2 plugin lost: {reason}");
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

    /// Reads the plugin's wiring again into the metadata the host is given.
    fn refresh_meta(&mut self) {
        if let Some(control) = self.lifecycle.control() {
            self.meta.descriptor = control.info().clone();
            self.meta.loaded = control.wiring();
        }
    }

    /// Acts on what the plugin asked of the host since the last call, and
    /// returns what the host should be told: the events the core derives from
    /// the plugin's requests, and the parameter moves the plugin reported
    /// itself (knobs turned in its editor).
    ///
    /// VST 2.4 has one notice for pins, latency and tail alike
    /// (`audioMasterIOChanged`), and the core reads those figures again rather
    /// than restarting ([`RestartRules::NONE`](tutti_plugin_types::RestartRules::NONE)).
    ///
    /// Call it between blocks, on the thread that loaded the plugin. A pass in
    /// which the plugin asked for nothing and the core has nothing pending (the
    /// idle pass every dispatch takes) does not enter the lifecycle and
    /// allocates nothing. A pass that has work allocates, as a rate change does:
    /// the core queues its effects.
    pub fn poll(&mut self) -> Vec<AsyncEvent> {
        self.serve();
        if let Some(loaded) = self.loaded_ref() {
            // An empty queue is an empty `Vec`, which allocates nothing.
            for (index, value) in loaded.drain_param_changes() {
                // A report with a negative index names no parameter.
                if let Ok(index) = u32::try_from(index) {
                    self.events.push(AsyncEvent::ParameterChanged {
                        index: ParamId::new(index),
                        value: Normalized::clamped(f64::from(value)),
                    });
                }
            }
        }
        std::mem::take(&mut self.events)
    }

    /// [`poll`](Self::poll)'s pass, leaving what the host should be told in
    /// `events`.
    fn serve(&mut self) {
        // A restart the pass performs may itself make the plugin ask for more
        // (an activation whose `effMainsChanged` raises `audioMasterIOChanged`),
        // so the word is taken again until a pass finds it empty. Bounded, so a
        // plugin that asks from every activation cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for _ in 0..MAX_PASSES {
            requests.clear();
            if let Some(loaded) = self.loaded_ref() {
                loaded.take_requests(&mut requests);
            }
            let asked = !requests.is_empty();
            if !asked && self.state.is_quiet() {
                // All a tick would do: let the control side reap what a
                // Processor retired.
                if let Some(control) = self.lifecycle.control() {
                    control.flush_edits();
                }
                break;
            }
            for request in requests.drain(..) {
                self.drive(Event::Request(request));
            }
            self.drive(Event::Tick);
            if !asked {
                break;
            }
        }
        self.requests = requests;
        self.finish_refusals();
    }

    /// The plugin, whichever half of an activation holds it.
    fn loaded_ref(&self) -> Option<&Vst2Loaded> {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller),
            Lifecycle::Poisoned => None,
        }
    }

    /// [`loaded_ref`](Self::loaded_ref), exclusively.
    fn loaded_mut(&mut self) -> Option<&mut Vst2Loaded> {
        match &mut self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(&mut **controller),
            Lifecycle::Poisoned => None,
        }
    }

    /// Whether the plugin is active: a refused rate leaves it loaded and
    /// silent.
    fn is_active(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Active { .. })
    }
}

impl PluginMeta for Vst2Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl Vst2Instance {
    /// Runs the plugin at `rate`: a no-op for the rate it already runs at
    /// unless `restart`.
    fn retarget(&mut self, rate: f64, restart: bool) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "VST2 plugin has no configuration to change".to_string(),
            ));
        };
        if !restart && self.is_active() && (previous.rate().0 - rate).abs() < f64::EPSILON {
            return Ok(());
        }
        let Some(wanted) = previous.with_rate(SampleRate(rate)) else {
            return Err(PluginError::Other(format!(
                "plugin refused sample rate {rate} ({}): not a positive finite rate",
                standing(self.is_active(), previous.rate())
            )));
        };
        let refusal = self.drive(Event::Op(HostOp::Reconfigure(wanted)));
        self.finish_refusals();
        refusal.map_or(Ok(()), |reason| {
            Err(PluginError::Other(format!(
                "VST2 plugin refused sample rate {rate} ({}): {reason}",
                standing(self.is_active(), previous.rate())
            )))
        })
    }
}

impl PluginAudio for Vst2Instance {
    fn process(
        &mut self,
        mut buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        // A rate the plugin refused left it deactivated (the host raised
        // `IoChanged` when it happened), so the block is silence and the plugin
        // is not touched.
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
    /// A rate that is no rate is refused before the plugin is touched. The
    /// host has no other way to refuse one (`effSetSampleRate` cannot be
    /// declined), so a reconfigure that is asked for takes. The error says where
    /// the plugin stands.
    ///
    /// The plugin may also change its pins and its latency across the
    /// re-activation; the metadata follows them and the host is told.
    fn set_sample_rate(&mut self, rate: f64) -> PluginResult<()> {
        self.retarget(rate, false)
    }

    fn reactivate(&mut self) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Ok(());
        };
        if !self.is_active() {
            return Ok(());
        }
        self.retarget(previous.rate().0, true)
    }

    /// Store the level the host reports through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Always accepted: VST2 has no query a plugin could decline — the host
    /// answers whenever the plugin asks — so unlike VST3 and AU this is no
    /// restart, and `probed::VST2` carries `RENDER_MODE` for the same reason.
    /// The mode is also recorded in the core's `Prepare`, so a later activation
    /// (a rate change) keeps it.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        if let Some(loaded) = self.loaded_mut() {
            loaded.set_offline_render(mode.is_offline());
        }
        self.state.set_mode(mode);
        true
    }
}

impl PluginParams for Vst2Instance {
    /// Normalized `0..=1`, VST2's own domain; `0.0` for a parameter the plugin
    /// has no accessor for, which the shared trait flattens (the distinction
    /// stays on `Control::parameter`).
    fn get_parameter(&self, id: ParamId) -> f64 {
        self.loaded_ref()
            .and_then(|loaded| Control::parameter(loaded, id))
            .map_or(0.0, Normalized::get)
    }

    /// Normalized `0..=1` in. A direct `setParameter` on the plugin, on the
    /// thread that serves the request, which the SDK allows beside `process`.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        if let Some(control) = self.lifecycle.control() {
            control.set_parameter(id, value);
        }
    }

    /// The plugin's display string, but **only for the value it currently
    /// holds**: see [`Control::parameter_text`]. Setting the parameter in order
    /// to read its label would make a display query *audible*.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded_ref()?, id, value)
    }

    /// Parse `text` through the plugin's own `effString2Parameter`.
    ///
    /// **This writes.** The VST2 opcode is a setter with no parse-only
    /// counterpart, so the parameter lands on the parsed value as a side effect
    /// of asking. That is a real divergence from the other three formats, where
    /// this method is a pure query; it is surfaced here rather than hidden
    /// because the alternative — refusing to implement it — would leave a user
    /// unable to type a value into a VST2 field at all.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded_ref()?, id, text)
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        self.loaded_ref()
            .map(Control::parameters)
            .unwrap_or_default()
    }

    fn parameter_count(&self) -> usize {
        self.loaded_ref().map_or(0, Vst2Loaded::parameter_count)
    }
}

impl PluginEditorHost for Vst2Instance {
    /// `_scale` is unused: VST2 has no call that takes a content scale.
    fn open_editor(
        &mut self,
        parent: WindowHandle,
        _scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        // Adapt tutti-plugin's WindowHandle (own pointer) to vst2-host's
        // local WindowHandle wrapper. Safety: the handle was validated at the
        // IPC boundary in server.rs.
        let host_parent = unsafe { tutti_vst2_host::WindowHandle::from_ptr(parent.as_ptr()) };
        let loaded = self
            .loaded_mut()
            .ok_or_else(|| PluginError::Other("the plugin is lost".to_string()))?;
        loaded
            .open_editor(host_parent)
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
            .map_err(|e| translate_error(e, Path::new("")).into())
    }

    fn close_editor(&mut self) {
        if let Some(loaded) = self.loaded_mut() {
            loaded.close_editor();
        }
    }
}

impl PluginPresets for Vst2Instance {
    /// VST2 programs. The index **is** the identifier — `effProgramChange`
    /// takes a position in `[0, numPrograms)` — so unlike AU's sparse selectors
    /// these are genuinely dense. Unnamed slots are kept: dropping one would
    /// renumber every program after it.
    fn get_presets(&mut self) -> Vec<Preset> {
        self.lifecycle
            .control()
            .map(Control::presets)
            .unwrap_or_default()
    }

    /// Switch program, bracketed by `effBeginSetProgram`/`effEndSetProgram` in
    /// the host layer. `false` for an id this format cannot address.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        self.lifecycle
            .control()
            .is_some_and(|control| control.load_preset(id))
    }

    fn get_current_preset(&mut self) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset()
    }
}

impl PluginState for Vst2Instance {
    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        self.lifecycle
            .control()
            .ok_or_else(|| PluginError::State("the plugin is lost".to_string()))?
            .save_state()
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        self.lifecycle
            .control()
            .ok_or_else(|| PluginError::State("the plugin is lost".to_string()))?
            .load_state(data)
    }
}

#[cfg(all(test, feature = "vst2"))]
mod tests {
    use super::*;
    use tutti_plugin::server::{Features, PluginTail};

    /// Every `TUTTI_VST2_PROBE_*` key the reference plugin reads at
    /// construction, so one test's configuration cannot leak into the next.
    /// Listed rather than derived: the probe owns the set, and a key added
    /// there without being added here would leak silently.
    const PROBE_ENV_KEYS: &[&str] = &[
        "TUTTI_VST2_PROBE_EDITOR",
        "TUTTI_VST2_PROBE_EFFECT_NAME",
        "TUTTI_VST2_PROBE_INPUTS",
        "TUTTI_VST2_PROBE_IS_SYNTH",
        "TUTTI_VST2_PROBE_LATENCY",
        "TUTTI_VST2_PROBE_MIDI_INPUTS",
        "TUTTI_VST2_PROBE_MIDI_OUTPUTS",
        "TUTTI_VST2_PROBE_NO_CAN_REPLACING",
        "TUTTI_VST2_PROBE_NO_CHUNKS",
        "TUTTI_VST2_PROBE_OUTPUTS",
        "TUTTI_VST2_PROBE_PARAMS",
        "TUTTI_VST2_PROBE_PROGRAMS",
        "TUTTI_VST2_PROBE_SERVICED_PARAMS",
        "TUTTI_VST2_PROBE_SERVICED_PROGRAMS",
        "TUTTI_VST2_PROBE_TAIL_SIZE",
    ];

    fn clear_probe_env() {
        for key in PROBE_ENV_KEYS {
            // SAFETY: every caller holds the plugin-load lock, so no other test
            // thread is reading or writing the environment concurrently.
            unsafe { std::env::remove_var(key) };
        }
    }

    /// Load the reference plugin with `env` applied at construction.
    ///
    /// The probe reads its metadata once, while the `AEffect` is being built,
    /// so the variables must be set before `load` and are cleared immediately
    /// after — leaving them set would configure whichever test ran next.
    ///
    /// Returns the guard alongside the instance: VST2 loading is serialized on
    /// a global in the `vst` crate, and dropping the guard early would let a
    /// second load race this one.
    fn load_probe(env: &[(&str, &str)]) -> (Vst2Instance, std::sync::MutexGuard<'static, ()>) {
        let lock = crate::test_utils::plugin_load_lock();
        clear_probe_env();
        for (k, v) in env {
            // SAFETY: as in `clear_probe_env` — the lock is held.
            unsafe { std::env::set_var(k, v) };
        }
        let path = crate::test_utils::vst2_probe_path();
        let loaded = Vst2Instance::load(Path::new(path), 48_000.0, 512, false);
        clear_probe_env();
        (
            loaded.unwrap_or_else(|e| panic!("loading the VST2 probe at {path} failed: {e:?}")),
            lock,
        )
    }

    /// One of the probe's `extern "C"` exports, called through `dlopen`: the
    /// linked rlib is a separate image with separate statics. The handle is
    /// leaked so the image stays mapped and the write survives.
    fn with_probe_symbol<R>(symbol: &str, f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
        let name = format!("{symbol}\0");
        let path = crate::test_utils::vst2_probe_path();
        // SAFETY: the path is the cdylib the dev-dependency built.
        let lib = unsafe { libloading::Library::new(path) }.expect("re-open the probe");
        // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
        let sym: libloading::Symbol<*mut std::ffi::c_void> =
            unsafe { lib.get(name.as_bytes()) }.expect("probe export");
        let r = f(*sym);
        std::mem::forget(lib);
        r
    }

    fn probe_capture() -> tutti_vst2_test_plugin::ProcessCapture {
        let mut cap = tutti_vst2_test_plugin::ProcessCapture::empty();
        with_probe_symbol("tutti_vst2_probe_capture", |p| {
            let f: unsafe extern "C" fn(*mut tutti_vst2_test_plugin::ProcessCapture) -> bool =
                unsafe { std::mem::transmute(p) };
            unsafe { f(&mut cap) };
        });
        cap
    }

    fn reset_probe() {
        for symbol in [
            "tutti_vst2_probe_reset_capture",
            "tutti_vst2_probe_reset_switches",
        ] {
            with_probe_symbol(symbol, |p| {
                let f: extern "C" fn() = unsafe { std::mem::transmute(p) };
                f();
            });
        }
    }

    /// One silent stereo block of `frames` through `plugin` as the pipeline
    /// hands it, returning what it rendered into channel 0 and 1.
    fn render(plugin: &mut Vst2Instance, frames: usize) -> PluginResult<(Vec<f32>, Vec<f32>)> {
        let input = vec![0.0f32; frames];
        let mut left = vec![9.0f32; frames];
        let mut right = vec![9.0f32; frames];
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let ctx = ProcessContext::default();
        let mut out = ProcessOutput::default();
        plugin.process(
            AudioBufferMut::F32(tutti_plugin::server::AudioBuffer::new(
                &inputs,
                &mut outputs,
                48_000.0,
            )),
            &ctx,
            &mut out,
        )?;
        Ok((left, right))
    }

    /// A VST2 plugin's `audioMasterIOChanged` reaches the host as the events a
    /// CLAP or AU change does: its new latency, its new tail, and an I/O
    /// change, with the loaded snapshot brought up to date.
    ///
    /// The plugin writes a new `initialDelay` and pin count into its `AEffect`
    /// and calls the host, through the probe's export (a plugin does this on
    /// its own initiative; there is no host call that provokes it).
    ///
    /// Mutation (run): drop the `Plugin::Vst2` arm's `poll_changes` -> no
    /// events -> fails. Mutation (run): `HostState::io_changed` returning
    /// `false`/not latching (in `tutti-vst2-host`) -> no `IoChanged` -> fails.
    #[test]
    fn an_io_changed_callback_raises_latency_and_io_events() {
        use crate::plugin::{AsyncEvent, Plugin};

        let (p, _lock) = load_probe(&[]);
        let mut plugin = Plugin::Vst2(p);
        assert!(
            plugin.poll_async_events().is_empty(),
            "nothing was reported yet"
        );

        let path = crate::test_utils::vst2_probe_path();
        // SAFETY: the path is the cdylib the dev-dependency built; the symbol
        // is the probe's `#[no_mangle]` export.
        let lib = unsafe { libloading::Library::new(path) }.expect("re-open the probe");
        let answered = unsafe {
            let f: libloading::Symbol<extern "C" fn(i32, i32) -> isize> = lib
                .get(b"tutti_vst2_probe_fire_io_changed\0")
                .expect("probe export");
            f(777, 4)
        };
        // Leaked, as the other suites do: the image must stay mapped.
        std::mem::forget(lib);
        assert_eq!(answered, 1, "the host handles audioMasterIOChanged");

        let events = plugin.poll_async_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                AsyncEvent::LatencyChanged { samples } if samples.get() == 777
            )),
            "the new latency was not reported: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AsyncEvent::IoChanged)),
            "the I/O change was not reported: {events:?}"
        );
        // Irrefutable while VST2 is the only format built.
        #[cfg_attr(
            not(any(
                feature = "vst3",
                feature = "clap",
                all(feature = "au", target_os = "macos")
            )),
            expect(irrefutable_let_patterns, reason = "`Vst2` is the only variant built")
        )]
        let Plugin::Vst2(p) = &plugin else {
            unreachable!("constructed as Vst2")
        };
        assert_eq!(p.loaded().latency_samples.get(), 777);
        assert_eq!(p.loaded().inputs[0].count(), 4);
        assert!(plugin.poll_async_events().is_empty(), "consumed");
    }

    /// The declared channel counts reach `LoadedPlugin` as one bus per side.
    ///
    /// The loader's own comment calls VST2 "single-bus", and this is what that
    /// claim means downstream: a host sizing buffers reads one width per side.
    /// Asserting the *width* as well as the bus count is what fails if
    /// `single_bus` is ever handed a count where a layout was meant — the two
    /// are both integers, so a swap compiles.
    #[test]
    fn declared_channel_counts_arrive_as_one_bus_per_side() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_INPUTS", "2"),
            ("TUTTI_VST2_PROBE_OUTPUTS", "2"),
        ]);
        let loaded = p.loaded();

        assert_eq!(loaded.inputs.len(), 1, "VST2 has exactly one input bus");
        assert_eq!(loaded.outputs.len(), 1, "VST2 has exactly one output bus");
        assert_eq!(loaded.inputs[0].count(), 2, "input width");
        assert_eq!(loaded.outputs[0].count(), 2, "output width");
    }

    /// A width other than stereo survives the mapping.
    ///
    /// The stereo case above would pass against a loader that hard-coded 2, so
    /// it cannot see a mapping that ignores what the plugin declared.
    #[test]
    fn a_non_stereo_width_is_carried_rather_than_assumed() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_INPUTS", "1"),
            ("TUTTI_VST2_PROBE_OUTPUTS", "4"),
        ]);
        let loaded = p.loaded();

        assert_eq!(loaded.inputs[0].count(), 1, "mono in");
        assert_eq!(loaded.outputs[0].count(), 4, "quad out");
    }

    /// Both MIDI directions reach `Features` from the host's own resolution.
    ///
    /// Deliberately *not* asserted from the pin counts: `tutti-vst2-host`
    /// resolves each direction as "the plugin's `can_do` answer, falling back
    /// to pin counts only on `Maybe`", and this probe answers `Yes`. So a
    /// `MIDI_OUTPUTS=0` build still reports `MIDI_OUT`, correctly — an explicit
    /// yes outranks an inference. (An earlier draft of this test asserted the
    /// opposite and failed; the resolution order is the reason, and it is
    /// deliberate.)
    ///
    /// What this pins is the seam the loader owns: two independent host fields
    /// reaching two independent bits. The `Maybe`-inference path underneath is
    /// driven through `dlopen`-set switches, so it belongs to
    /// `tutti-vst2-host`'s integration suite rather than here.
    #[test]
    fn both_midi_directions_reach_features() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_MIDI_INPUTS", "16"),
            ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "16"),
        ]);
        let f = p.loaded().features;

        assert!(f.contains(Features::MIDI_IN), "declared MIDI inputs");
        assert!(f.contains(Features::MIDI_OUT), "declared MIDI outputs");
    }

    /// Transport is set unconditionally, editor only when the plugin has one.
    ///
    /// Paired deliberately: `TRANSPORT` is `insert`ed with no condition (every
    /// VST2 gets `get_time_info`), while `EDITOR` is measured. Asserting both
    /// against one plugin that has no editor is what catches the two being
    /// wired to the same source.
    #[test]
    fn transport_is_unconditional_but_editor_is_measured() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EDITOR", "0")]);
        let f = p.loaded().features;

        assert!(
            f.contains(Features::TRANSPORT),
            "every VST2 is handed a transport snapshot"
        );
        assert!(
            !f.contains(Features::EDITOR),
            "this probe declares no editor"
        );
    }

    /// An editor-bearing probe sets the bit, so the assertion above is real.
    #[test]
    fn an_editor_bearing_plugin_reports_one() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EDITOR", "1")]);
        assert!(p.loaded().features.contains(Features::EDITOR));
    }

    /// `effGetTailSize` is asked, and its **inverted** encoding decoded.
    ///
    /// VST2 is the one format whose `0` does not mean "no tail":
    ///
    /// | raw | meaning                             | decoded     |
    /// |-----|-------------------------------------|-------------|
    /// | `0` | no tail *information*; host decides | `Unknown`   |
    /// | `1` | no tail at all                      | `None`      |
    /// | `n` | `n` samples of ring-out             | `Finite(n)` |
    ///
    /// Reading a raw `0` as `None` — which `PluginTail::from_clap_samples`
    /// and `from_vst3_samples` would, since that is right for CLAP and VST3 —
    /// tells a bounce to add no decay
    /// and truncates every reverb. All three rows are asserted together
    /// because the bug is a *swap*: either zero alone still passes half a
    /// table.
    ///
    /// `TUTTI_VST2_PROBE_TAIL_SIZE` sets the **raw wire value**; the probe
    /// intercepts the opcode before vst-rs can rewrite a trait-reported `0`
    /// into `1`, which is what makes the `0` row testable at all.
    #[test]
    fn the_inverted_tail_encoding_is_decoded_not_passed_through() {
        for (raw, want, why) in [
            (
                "0",
                PluginTail::Unknown,
                "raw 0 is 'no information', not a declared silence",
            ),
            ("1", PluginTail::None, "raw 1 is VST2's 'no tail at all'"),
            (
                "48000",
                PluginTail::Finite(tutti_plugin::server::Samples(48_000)),
                "anything above 1 is a sample count",
            ),
        ] {
            let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_TAIL_SIZE", raw)]);
            assert_eq!(p.loaded().tail, want, "raw={raw}: {why}");
        }
    }

    /// A negative answer is read as `Unknown`, not clamped to a silence.
    ///
    /// VST2 gives no meaning to a negative tail. Clamping to `0` would land on
    /// whichever verdict `0` maps to, making a malformed answer
    /// indistinguishable from a deliberate one — and the probe exists to
    /// misbehave in exactly this way.
    #[test]
    fn a_negative_tail_is_unknown_rather_than_clamped() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_TAIL_SIZE", "-1")]);
        assert_eq!(p.loaded().tail, PluginTail::Unknown);
    }

    /// Speaker topology is empty for both sides.
    ///
    /// VST2 cannot be asked — the `VstSpeakerArrangement` struct is not in the
    /// forked bindings — so empty lists claim nothing rather than asserting
    /// a placement the format never reported.
    #[test]
    fn no_speaker_topology_is_claimed() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_OUTPUTS", "6")]);
        let loaded = p.loaded();

        assert!(
            loaded.input_topology.is_empty() && loaded.output_topology.is_empty(),
            "VST2 reports no speaker placement, so neither side may claim one"
        );
    }

    /// The descriptor carries the plugin's own name, not the file's.
    #[test]
    fn the_descriptor_reports_the_plugins_declared_name() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EFFECT_NAME", "tutti-probe-named")]);
        assert_eq!(p.descriptor().name, "tutti-probe-named");
    }

    /// A missing file fails at `Opening` rather than panicking.
    ///
    /// The error path has no probe involved, so it is the one case that would
    /// still run if the reference cdylib were missing.
    #[test]
    fn a_missing_file_reports_a_load_failure() {
        let _lock = crate::test_utils::plugin_load_lock();
        // `match` rather than `expect_err`: the `Ok` type is not `Debug`.
        match Vst2Instance::load(
            Path::new("/nonexistent/tutti-not-a-plugin.so"),
            48_000.0,
            512,
            false,
        ) {
            Err(BridgeError::LoadFailed { .. }) => {}
            Err(other) => panic!("expected LoadFailed, got {other:?}"),
            Ok(_) => panic!("loading a path that does not exist must fail"),
        }
    }

    /// **A rate that is no rate is refused before the plugin is touched**: the
    /// instance runs on at the configuration it has, nothing crosses the FFI
    /// seam (no suspend, no resume), the host is told nothing, and the error
    /// says where the plugin runs. A good rate afterwards takes. (The VST2 host
    /// cannot refuse a rate any other way, so there is no rollback to drive.)
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every rate
    /// -> the zero rate reaches `effSetSampleRate` and the plugin is suspended
    /// and resumed -> the `set_sample_rate` `expect_err` fails.
    #[test]
    fn a_rate_that_is_no_rate_is_refused_before_the_plugin_is_touched() {
        let (mut p, _lock) = load_probe(&[]);
        reset_probe();
        let layout = p.meta.loaded.clone();
        let prepare = p.state.prepare();
        for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = p
                .set_sample_rate(rate)
                .expect_err("a rate that is no rate is refused");
            assert!(error.to_string().contains("running at 48000"), "{error}");
            assert!(p.is_active(), "{rate}");
            let cap = probe_capture();
            assert_eq!((cap.suspend_count, cap.resume_count), (0, 0), "{rate}");
        }
        assert_eq!(p.state.prepare(), prepare, "the configuration is untouched");
        let (left, right) = render(&mut p, 64).expect("the plugin still renders");
        assert!(left.iter().all(|&v| v == 1.0), "{:?}", &left[..4]);
        assert!(right.iter().all(|&v| v == 101.0));
        assert!(p.poll().is_empty(), "the host is told nothing");
        assert_eq!(p.meta.loaded.inputs, layout.inputs);
        assert_eq!(p.meta.loaded.outputs, layout.outputs);
        p.set_sample_rate(96_000.0).expect("a real rate");
        assert_eq!(probe_capture().sample_rate, 96_000.0);
    }

    /// **A load at no rate or no block is refused, and the plugin is never
    /// resumed**: zero, negative and non-finite rates, and a block of no frames.
    ///
    /// Mutation (run): `valid_rate` accepting every rate -> the rate cases load
    /// -> the `Err` match fails. Mutation (run): `MaxBlock::new` accepting zero
    /// -> the block case loads -> fails.
    #[test]
    fn a_load_at_no_rate_or_no_block_is_refused() {
        let _lock = crate::test_utils::plugin_load_lock();
        clear_probe_env();
        reset_probe();
        let path = crate::test_utils::vst2_probe_path();
        for (rate, block) in [
            (0.0, 512),
            (-48_000.0, 512),
            (f64::NAN, 512),
            (f64::INFINITY, 512),
            (f64::NEG_INFINITY, 512),
            (48_000.0, 0),
        ] {
            match Vst2Instance::load(Path::new(path), rate, block, false) {
                Err(BridgeError::LoadFailed {
                    stage: LoadStage::Activation,
                    reason,
                    ..
                }) => assert!(reason.contains("cannot run at"), "{reason}"),
                Err(other) => panic!("{rate} Hz in {block} frames: {other:?}"),
                Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
            }
        }
        assert_eq!(probe_capture().resume_count, 0, "never resumed");
        let p = Vst2Instance::load(Path::new(path), 48_000.0, 512, false).expect("a real load");
        assert!(p.is_active());
    }

    /// **The rate the plugin already runs at is a no-op**: nothing crosses the
    /// seam, so a caller that sets it every frame does not tear the plugin down.
    ///
    /// Mutation (run): the same-rate early return removed -> the plugin is
    /// suspended and resumed again -> `suspend_count` is 1 -> fails.
    #[test]
    fn the_rate_the_plugin_runs_at_is_not_a_restart() {
        let (mut p, _lock) = load_probe(&[]);
        reset_probe();
        p.set_sample_rate(48_000.0).expect("the same rate");
        let cap = probe_capture();
        assert_eq!((cap.suspend_count, cap.resume_count), (0, 0));
    }

    /// **The render mode is live, kept across a restart, and no restart**: the
    /// plugin reads the process level the mode names from the next block, and a
    /// rate change afterwards does not put the old mode back.
    ///
    /// Mutation (run): `set_render_mode` not recording the mode in the core
    /// (`state.set_mode`) -> the rate change re-activates at realtime and the
    /// level reads 2 -> fails. Mutation (run): not storing it in the host's
    /// state -> the first level reads 2 -> fails.
    #[test]
    fn the_render_mode_is_live_and_survives_a_rate_change() {
        let (mut p, _lock) = load_probe(&[]);
        reset_probe();
        assert!(p.set_render_mode(RenderMode::Offline));
        render(&mut p, 64).expect("renders");
        let cap = probe_capture();
        assert_eq!(cap.process_level, 4, "offline from the next block");
        assert_eq!(cap.suspend_count, 0, "and no restart for it");

        p.set_sample_rate(96_000.0).expect("a rate change");
        render(&mut p, 64).expect("renders");
        assert_eq!(probe_capture().process_level, 4, "still offline");
    }

    /// **A parameter the plugin moved itself is told to the host**, once, with
    /// the index and the value (a knob turned in its editor).
    ///
    /// Mutation (run): `poll` not draining the plugin's changes -> none are told
    /// -> fails.
    #[test]
    fn a_parameter_the_plugin_moved_is_forwarded() {
        let (mut p, _lock) = load_probe(&[]);
        reset_probe();
        with_probe_symbol("tutti_vst2_probe_set_automate_burst", |f| {
            let f: extern "C" fn(u32) = unsafe { std::mem::transmute(f) };
            f(3);
        });
        render(&mut p, 64).expect("renders");
        with_probe_symbol("tutti_vst2_probe_set_automate_burst", |f| {
            let f: extern "C" fn(u32) = unsafe { std::mem::transmute(f) };
            f(0);
        });
        let moved: Vec<_> = p
            .poll()
            .into_iter()
            .filter(|e| matches!(e, AsyncEvent::ParameterChanged { .. }))
            .collect();
        assert_eq!(moved.len(), 3, "{moved:?}");
        assert!(p.poll().is_empty(), "told once");
    }

    /// **A probe reads the plugin's identity without activating it**: nothing is
    /// resumed.
    ///
    /// Mutation (run): `probe` going through `load` (an activation) -> the
    /// plugin resumes -> `resume_count` is 1 -> fails.
    #[test]
    fn a_probe_does_not_activate() {
        let _lock = crate::test_utils::plugin_load_lock();
        reset_probe();
        let descriptor =
            Vst2Instance::probe(Path::new(crate::test_utils::vst2_probe_path())).expect("probes");
        assert_eq!(descriptor.name, "Tutti VST2 Probe");
        assert_eq!(probe_capture().resume_count, 0);
    }
}
