//! CLAP plugin loader: [`ClapInstance`] drives a `Lifecycle<ClapLoaded>` and
//! presents it through the capability traits the server still dispatches on.
//!
//! The host crate is the interpreter of the plugin's lifecycle (activation,
//! restarts, requests); this file is a probe, the thread pool this process
//! serves `clap.thread-pool` on, and the translation between the core's
//! notices and the server's [`AsyncEvent`]s.

use std::path::Path;
use tutti_clap_host::{ClapLoaded, ClapProcessor, LoadOptions};
use tutti_plugin::server::{
    ContentScale, EditorSize, LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginAudio,
    PluginDescriptor, PluginEditorHost, PluginError, PluginMeta, PluginParams, PluginPresets,
    PluginResult, PluginState, PresetId, WindowHandle,
};
use tutti_plugin::server::{ProcessContext, ProcessOutput, RenderMode};
use tutti_plugin::{BridgeError, LoadStage, Result};
use tutti_plugin_types::MainThread;
use tutti_plugin_types::{
    Control, Event, Hand, HostOp, Lifecycle, MaxBlock, Notice, Prepare, Request, SampleFormat,
    SampleRate, Samples, Seat, Stage, State,
};

use crate::loaders::common::Meta;
use crate::plugin::AsyncEvent;
use tutti_plugin_types::standing;

/// The pool this process serves `clap.thread-pool` requests on, built the
/// first time a plugin that implements the extension loads; `None` on a
/// machine too small for one (below) or that refuses a thread.
///
/// The engine's own pool lives in the host process, and a plugin's `process`
/// runs here, in another: a request cannot reach it. So the server keeps a
/// `WorkerPool` of its own, with its helpers asking for the audio thread's
/// priority (the highest the OS grants, not real time), as `tutti_graph::WorkerPool` requires of helpers. A
/// plugin that does not implement the extension costs no threads.
///
/// Sized by [`pool_participants`]: half the cores, at most 4, the server's
/// audio thread included. Each plugin instance has a server process of its
/// own, so every pooled instance adds a pool, and the engine's pool in the
/// host wants the same cores; half leaves the other half to them, and the cap
/// keeps a few pooled instances from oversubscribing a large machine, where
/// one block's tasks rarely keep more than a few threads busy anyway. Below
/// two participants (fewer than 4 cores) no pool is built, and requests are
/// refused: the plugin's own fallback does as well as a pool of one.
///
/// A request is one span of one block, so its helpers spin only briefly and
/// park soon after (10 µs, then 50 µs of yielding) rather than holding cores
/// the engine could use between blocks. That costs a request nothing: it
/// never waits for a helper that has not started a task (the audio thread
/// runs tasks from the start, and helpers join as they arrive), though a
/// helper preempted mid-task delays the request.
fn thread_pool() -> Option<std::sync::Arc<tutti_graph::WorkerPool>> {
    THREAD_POOL
        .get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            let participants = pool_participants(cores);
            if participants < 2 {
                return None;
            }
            tutti_graph::WorkerPool::builder()
                .num_threads(participants)
                .thread_name(|i| format!("tutti-clap-pool-{i}"))
                .spin_budget(std::time::Duration::from_micros(10))
                .idle(std::time::Duration::from_micros(50))
                .start_handler(|_| {
                    crate::server::request_audio_priority("a clap.thread-pool helper")
                })
                .build()
                .inspect_err(|e| {
                    tracing::warn!(
                        "clap.thread-pool: a {participants}-thread pool was refused, \
                         requests are refused: {e}"
                    );
                })
                .ok()
                .map(std::sync::Arc::new)
        })
        .clone()
}

/// Participants of the server's pool on a machine with `cores` cores (see
/// [`thread_pool`]): half of them, at most 4.
fn pool_participants(cores: usize) -> usize {
    (cores / 2).min(4)
}

/// [`thread_pool`]'s pool, once decided.
static THREAD_POOL: std::sync::OnceLock<Option<std::sync::Arc<tutti_graph::WorkerPool>>> =
    std::sync::OnceLock::new();

/// A plugin that is open and not yet active.
struct Opened {
    loaded: ClapLoaded,
    sample_rate: f64,
    block_size: usize,
}

/// A CLAP plugin as the server sees it.
pub struct ClapInstance {
    lifecycle: Lifecycle<ClapLoaded>,
    state: State,
    hand: Hand<ClapProcessor>,
    meta: Meta,
    /// What the plugin asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// What the host has not yet been told.
    events: Vec<AsyncEvent>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
}

/// Whether a pending write may be flushed out of band: not when the parameter
/// requires `process` and the plugin is processing.
fn flush_out_of_band(requires_process: bool, processing: bool) -> bool {
    !(requires_process && processing)
}

fn resolve(path: &Path) -> Result<(std::path::PathBuf, bool)> {
    let resolved = tutti_plugin::server::resolve_bundle(path)?;
    let differs = resolved != path;
    Ok((resolved, differs))
}

impl ClapInstance {
    /// Load and activate a CLAP plugin.
    ///
    /// If `prefer_f64` is `true` and the plugin advertises 64-bit support on
    /// every port at activation, the instance is activated at 64-bit;
    /// otherwise 32-bit. The chosen format is what `loaded().features` reports
    /// as `Features::F64_AUDIO`, so the session's negotiated format and the
    /// activation agree: the session sends the buffer width it negotiated, and
    /// an instance activated in the other width would refuse every block.
    ///
    /// Activation re-reads the plugin's audio ports, so the buses and the
    /// 64-bit answer in the metadata are the ones the plugin has when it is
    /// activated, not the ones it had when it was opened.
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
    /// activated**: CLAP's `state.load` is allowed on an inactive plugin, and a
    /// plugin may take its mode from the state when it is activated. The second
    /// value is the plugin's refusal of the state, if it refused: the plugin is
    /// then activated at its defaults (a plugin at its defaults beats none), and
    /// the activation's buses and latency, which the load reply carries, are
    /// those of the state when it took it.
    pub fn load_with_state(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> Result<(Self, Option<String>)> {
        Self::activate(
            main,
            Self::open(main, path, sample_rate, block_size)?,
            path,
            prefer_f64,
            state,
            before_restore,
        )
    }

    /// Opens the plugin, without activating it.
    fn open(main: &MainThread, path: &Path, sample_rate: f64, block_size: usize) -> Result<Opened> {
        let (resolved, differs) = resolve(path)?;
        let mut options = LoadOptions::new();
        if differs {
            options = options.library(resolved);
        }
        let mut loaded =
            ClapLoaded::load(main, path, options).map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: match e {
                    tutti_clap_host::ClapError::LoadFailed { stage, .. } => stage,
                    _ => LoadStage::Opening,
                },
                reason: e.to_string(),
            })?;
        if loaded.supports_thread_pool() {
            loaded.set_thread_pool(thread_pool());
        }
        Ok(Opened {
            loaded,
            sample_rate,
            block_size,
        })
    }

    /// Activates an opened plugin in the format the session will process.
    fn activate(
        main: &MainThread,
        opened: Opened,
        path: &Path,
        prefer_f64: bool,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> Result<(Self, Option<String>)> {
        let Opened {
            mut loaded,
            sample_rate,
            block_size,
        } = opened;
        let activation_failed = |reason: String| BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Activation,
            reason: format!("activate failed: {reason}"),
        };
        // 64-bit only when it was asked for: the session's wire format follows
        // its preference (f32 by default), not the plugin's capability.
        // `negotiate_format` reads the ports again first, so a plugin that
        // dropped its 64-bit flags since it was opened loads as 32-bit.
        let requested = if prefer_f64 {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        };
        let format = loaded
            .negotiate_format(main, requested)
            .map_err(|e| activation_failed(e.to_string()))?;
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| {
                activation_failed(format!(
                    "the plugin cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                ))
            })?
            .with_format(format);

        let mut instance = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<ClapLoaded>::initial_state(),
            hand: Hand::local(),
            meta: Meta::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        // Before the activation: see `load_with_state`.
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
            return Err(activation_failed(reason));
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
        let state = std::mem::replace(&mut self.state, Lifecycle::<ClapLoaded>::initial_state());
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
            // CLAP has no note-expression restart flag.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("CLAP plugin refused {} Hz: {reason}", rate.0);
                // Whether the host must re-read the instance depends on whether
                // a rollback revives it: `finish_refusals` decides.
                self.refresh_meta(main);
                self.refused = true;
                return Some(reason);
            }
            Notice::Stuck { reason } => {
                tracing::warn!("CLAP plugin would not stop: {reason}");
            }
            Notice::Lost { reason } => {
                tracing::error!("CLAP plugin lost: {reason}");
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

    /// Delivers the writes the Processor has not drained, so a state save or a
    /// fork sees them. The server serves requests on the thread that renders, so
    /// without this a save right after a write would read the old value until
    /// the next block.
    ///
    /// Each is applied through an out-of-band `params.flush`, except that a
    /// parameter flagged `REQUIRES_PROCESS` is never flushed while the plugin is
    /// processing (CLAP wants it in event order, in a block): it goes back to the
    /// queue. Only what was pending is flushed, at the moment it is asked for,
    /// so a value the plugin or its editor has since moved is not re-asserted by
    /// every write.
    fn settle_edits(&mut self, main: &MainThread) {
        if let Some(control) = self.lifecycle.control() {
            control.flush_edits(main);
        }
        let mut pending = Vec::new();
        let processing = match &mut self.lifecycle {
            Lifecycle::Active {
                seat: Seat::Held(processor),
                ..
            } => {
                processor.take_edits(&mut pending);
                processor.is_processing()
            }
            _ => return,
        };
        for (id, value) in pending {
            let requires = self
                .loaded_ref()
                .is_some_and(|loaded| loaded.param_requires_process(main, id.get()));
            if flush_out_of_band(requires, processing) {
                if let Some(loaded) = self.loaded_mut() {
                    Control::set_parameter(loaded, main, id, value);
                }
            } else if let Some(control) = self.lifecycle.control() {
                control.set_parameter(main, id, value);
            }
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
    /// Its main-thread work runs first, since it may ask for the rest: the
    /// `on_main_thread` a `request_callback` wanted, the timers that are due and
    /// the file descriptors that are ready. Then the core decides what the
    /// requests need (a restart for `request_restart`, an `audio-ports.rescan`
    /// that changes buffers, or a `latency.changed`; a re-read for the rest) and
    /// the lifecycle performs it. Call it between blocks, on the thread that
    /// loaded the plugin. A pass in which nothing was asked, no timer is due, no
    /// descriptor is ready and the core has nothing pending (the idle pass every
    /// dispatch takes) does not enter the lifecycle and allocates nothing; a
    /// pass with work allocates, as a rate change does.
    ///
    /// Timers and callbacks are served only while the host keeps calling this:
    /// a loop that blocks in `recv` for the next request serves none until one
    /// arrives.
    pub fn poll(&mut self, main: &MainThread) -> Vec<AsyncEvent> {
        self.serve(main);
        std::mem::take(&mut self.events)
    }

    /// [`poll`](Self::poll)'s pass, leaving what the host should be told in
    /// `events`.
    fn serve(&mut self, main: &MainThread) {
        // The main-thread work a pass runs (`on_main_thread`, a timer) may
        // itself ask for more, and the plugin's asking is the word being
        // raised again: take it again until a pass finds it empty. Bounded, so
        // a plugin that asks from every callback cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for pass in 0..MAX_PASSES {
            requests.clear();
            if let Some(loaded) = self.loaded_mut() {
                // Timers and descriptors are polled once per pass of the host's
                // loop; only what the plugin raised is taken again.
                if pass == 0 {
                    loaded.take_requests(main, &mut requests);
                } else {
                    loaded.take_raised(&mut requests);
                }
            }
            let asked = !requests.is_empty();
            if !asked && self.state.is_quiet() {
                // The idle pass: nothing was asked, no timer is due, no
                // descriptor is ready and the core has nothing pending, so it
                // does not enter the lifecycle and allocates nothing. All a tick
                // would do is retry what a full edit ring refused.
                if let Some(control) = self.lifecycle.control() {
                    control.flush_edits(main);
                }
                break;
            }
            if !asked && pass > 0 {
                break;
            }
            for request in requests.drain(..) {
                self.drive(main, Event::Request(request));
            }
            self.drive(main, Event::Tick);
        }
        self.requests = requests;
        self.finish_refusals();
    }

    /// The plugin, whichever half of an activation holds it.
    fn loaded_ref(&self) -> Option<&ClapLoaded> {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller.loaded()),
            Lifecycle::Poisoned => None,
        }
    }

    /// [`loaded_ref`](Self::loaded_ref), exclusively.
    fn loaded_mut(&mut self) -> Option<&mut ClapLoaded> {
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
}

impl PluginMeta for ClapInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for ClapInstance {
    fn reset(&mut self) -> PluginResult<()> {
        // No processor (a refused activation) has nothing to forget.
        if let Some(processor) = self.lifecycle.processor() {
            tutti_plugin_types::Processor::reset(processor);
        }
        Ok(())
    }

    fn process(
        &mut self,
        mut buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        use tutti_plugin::server::AudioBufferMut;
        // A rate change the plugin refused left it deactivated (the host raised
        // `IoChanged` when it happened). CLAP allows `process` only while
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

    /// Forward to `clap.render`, reporting whether the plugin took it.
    ///
    /// `clap_plugin_render.set` is `[main-thread]` (`ext/render.h:33`) and
    /// carries no activation constraint, so unlike VST3 and AU this needs no
    /// deactivate/reactivate bracket — the live instance accepts it.
    ///
    /// `false` here is a real answer, not a failure to ask: the extension is
    /// optional by design (*"If this information does not influence your
    /// rendering code, then don't implement this extension"*), so a plugin
    /// that does not export it has genuinely declined.
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        let offline = mode.is_offline();
        let accepted = self
            .loaded_mut()
            .is_some_and(|loaded| loaded.set_render_mode(main, offline));
        // The mode is a field of the `Prepare` every later activation (a
        // restart, a rate change) applies, so the core holds the new one or the
        // next restart would put the old mode back. Nothing restarts now: the
        // plugin took the mode live above.
        self.state.set_mode(mode);
        accepted
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
    /// The plugin may also change its bus layout across the re-activation; the
    /// metadata follows it and the host is told the same way.
    fn set_sample_rate(&mut self, main: &MainThread, rate: f64) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "CLAP plugin has no configuration to change".to_string(),
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
                Err(PluginError::Other(format!(
                    "CLAP plugin refused sample rate {rate} ({}): {reason}",
                    if rolled_back {
                        format!("running at {}", previous.rate().0)
                    } else {
                        "deactivated".to_string()
                    }
                )))
            }
        };
        self.finish_refusals();
        result
    }
}

impl PluginParams for ClapInstance {
    /// Normalized `0..=1`, per the [`PluginParams`] contract.
    fn get_parameter(&self, main: &MainThread, id: ParamId) -> f64 {
        self.loaded_ref()
            .and_then(|loaded| Control::parameter(loaded, main, id))
            .map_or(0.0, Normalized::get)
    }

    /// Normalized `0..=1` in. On an active plugin the write is queued to the
    /// Processor and reaches the plugin at the head of the next block.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        if let Some(control) = self.lifecycle.control() {
            control.set_parameter(main, id, value);
        }
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded_ref()?, main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded_ref()?, main, id, text)
    }

    fn get_parameter_list(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.loaded_ref()
            .map(|loaded| Control::parameters(loaded, main))
            .unwrap_or_default()
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        self.loaded_ref()
            .map_or(0, |loaded| loaded.parameter_count(main))
    }
}

impl PluginEditorHost for ClapInstance {
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        let loaded = self
            .loaded_mut()
            .ok_or_else(|| PluginError::Other("the plugin is lost".to_string()))?;
        loaded
            .open_editor(main, parent, scale)
            .map(|s| EditorSize {
                width: s.width,
                height: s.height,
            })
            .map_err(|e| PluginError::Editor(tutti_plugin::EditorError::PluginError(e.to_string())))
    }

    fn close_editor(&mut self, main: &MainThread) {
        if let Some(loaded) = self.loaded_mut() {
            loaded.close_editor(main);
        }
    }
}

impl PluginPresets for ClapInstance {
    /// Load a preset from the path an id names, via `CLAP_EXT_PRESET_LOAD`. See
    /// [`Control::load_preset`] for
    /// which ids that is.
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        self.lifecycle
            .control()
            .is_some_and(|control| control.load_preset(main, id))
    }
}

impl PluginState for ClapInstance {
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
#[cfg(feature = "clap")]
mod tests {
    use super::*;

    /// This process's main thread. Each test is its own process under nextest, so
    /// the claim cannot be refused.
    fn main_thread() -> tutti_plugin_types::MainThread {
        tutti_plugin_types::MainThread::claim().expect("one test, one process")
    }

    use std::path::Path;
    use std::sync::atomic::Ordering;
    use tutti_plugin::server::{
        NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, ParameterChanges,
        ParameterQueue, TransportInfo,
    };
    use tutti_ump::{MidiChannel, MidiGroup};

    use tutti_clap_host::ClapLoaded as HostLoaded;
    use tutti_plugin_types::{EditorPresence, Features, PluginTail, Request};

    /// What a pass told the host that is not the layout, as these tests read it.
    #[derive(Debug, Default, PartialEq)]
    struct ClapChanges {
        latency: Option<Samples>,
        tail: Option<PluginTail>,
        param_values: bool,
        param_titles: bool,
    }

    /// The test-side views of the instance: the plugin whichever half holds it,
    /// and the pass and its results kept apart so a test can read them one at a
    /// time.
    impl ClapInstance {
        pub(crate) fn clap_loaded(&self) -> &HostLoaded {
            self.loaded_ref().expect("the plugin is not lost")
        }

        fn loaded_mut_unwrapped(&mut self) -> &mut HostLoaded {
            self.loaded_mut().expect("the plugin is not lost")
        }

        fn is_processing(&mut self) -> bool {
            self.lifecycle
                .processor()
                .is_some_and(|p| p.is_processing())
        }

        fn service_requests(&mut self) {
            self.serve(&main_thread());
        }

        /// Everything the plugin has asked of the host and the pass has not
        /// yet taken.
        fn pending_requests(&mut self) -> Vec<Request> {
            let mut requests = Vec::new();
            self.loaded_mut_unwrapped()
                .take_requests(&main_thread(), &mut requests);
            requests
        }

        /// Raises `request` as a plugin callback would.
        fn raise(&self, request: Request) {
            request.raise(&self.clap_loaded().host_state().requests);
        }

        fn take_io_changed(&mut self) -> bool {
            let before = self.events.len();
            self.events.retain(|e| !matches!(e, AsyncEvent::IoChanged));
            self.events.len() != before
        }

        fn take_changes(&mut self) -> ClapChanges {
            let mut changes = ClapChanges::default();
            self.events.retain(|event| match event {
                AsyncEvent::LatencyChanged { samples } => {
                    changes.latency = Some(*samples);
                    false
                }
                AsyncEvent::TailChanged { tail } => {
                    changes.tail = Some(*tail);
                    false
                }
                AsyncEvent::ParamValuesChanged => {
                    changes.param_values = true;
                    false
                }
                AsyncEvent::ParamTitlesChanged => {
                    changes.param_titles = true;
                    false
                }
                _ => true,
            });
            changes
        }
    }

    /// The reference CLAP plugin, built as a dev-dependency by this same
    /// `cargo test` run. Resolved rather than hard-coded so the suite runs on
    /// any machine.
    fn clap_plugin() -> &'static str {
        crate::test_utils::clap_probe_path()
    }

    #[test]
    fn test_clap_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");

        let loaded = instance.loaded();

        let inputs = loaded.total_inputs();
        let outputs = loaded.total_outputs();
        assert!(inputs > 0, "Expected audio inputs > 0, got {inputs}");
        assert!(outputs > 0, "Expected audio outputs > 0, got {outputs}");
        // `F64_AUDIO` is the format activated, and this load asked for f32.
        // The session suite covers the 64-bit cases
        // (`an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64` and its
        // neighbours).
        assert!(!instance.loaded().features.contains(Features::F64_AUDIO));
    }

    /// **A plugin without `clap.tail` has an unknown tail, not none.** The
    /// extension's absence read as the `0` of "no tail", so a bounce sized
    /// from it cut off every such plugin's decay. A plugin that does answer
    /// keeps its figure.
    ///
    /// Mutation (run): in `ClapLoaded::tail_figure`, change
    /// `.map_or(PluginTail::Unknown, PluginTail::from_clap_samples)` to
    /// `.map_or(PluginTail::None, PluginTail::from_clap_samples)` → the first
    /// assertion sees `None` → fails.
    #[test]
    fn a_plugin_without_the_tail_extension_has_an_unknown_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        {
            let _without = crate::test_utils::clap_probe_without_tail();
            let instance =
                ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
            assert_eq!(instance.loaded().tail, PluginTail::Unknown);
        }
        let instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        assert_eq!(
            instance.loaded().tail,
            PluginTail::Finite(Samples(
                tutti_clap_test_plugin::REPORTED_TAIL_SAMPLES as usize
            ))
        );
    }

    /// **The metadata follows the plugin's ports at activation, not at open.**
    /// The buses were read before `activate` and never refreshed, so a plugin
    /// that changed its layout in between was described by the old one.
    ///
    /// Mutation (run): delete `instance.refresh_meta()` after the activation in
    /// `ClapInstance::activate` → the metadata is the default, not the
    /// activated plugin's → the assertions fail. (No open-time copy of the
    /// buses exists any more to go stale: the wiring is read from the plugin.)
    #[test]
    fn the_buses_are_those_the_plugin_has_at_activation() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let opened = ClapInstance::open(&main_thread(), path, 44100.0, 512).expect("open");

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let instance =
            ClapInstance::activate(&main_thread(), opened, path, false, None, &mut || {})
                .expect("activate")
                .0;

        let loaded = instance.loaded();
        assert_eq!(loaded.inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(loaded.total_inputs(), 3);
        assert_eq!(loaded.total_outputs(), 3);
    }

    /// **A plugin that withdraws its 64-bit flags after it was opened loads as
    /// 32-bit.** The session asked for 64-bit on the answer the plugin gave at
    /// open; activation asks again, and the refusal is not a failed load.
    ///
    /// Mutation (run): in `ClapInstance::activate`, delete the
    /// `rescan_audio_ports()` call → the format is chosen from the flags read at
    /// open, the 64-bit activation is refused and the load fails → the `expect`
    /// fails.
    #[test]
    fn a_plugin_that_drops_its_64_bit_flags_after_open_loads_as_32_bit() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let opened = ClapInstance::open(&main_thread(), path, 44100.0, 512).expect("open");
        assert!(opened.loaded.supports_f64(), "fixture: flagged at open");

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let instance = ClapInstance::activate(&main_thread(), opened, path, true, None, &mut || {})
            .expect("falls back to 32-bit audio")
            .0;
        assert!(!instance.loaded().features.contains(Features::F64_AUDIO));
    }

    /// **A rate change that changes the plugin's bus layout tells the host
    /// and refreshes the metadata.** Only VST3 raised `IoChanged`; a CLAP
    /// plugin that re-declared its ports across a re-activation left the host
    /// with the layout it was loaded with.
    ///
    /// Mutations (run): delete `self.refresh_meta();` in the
    /// `Notice::IoChanged` arm of `ClapInstance::absorb` → the metadata stays at
    /// one bus → the first assertion fails. Delete the `AsyncEvent::IoChanged`
    /// push in that arm → the polled-event assertion fails.
    #[test]
    fn a_rate_change_that_changes_the_layout_raises_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        assert_eq!(instance.loaded().inputs.len(), 1, "fixture: one input bus");
        assert!(!instance.take_io_changed(), "nothing changed at load");

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        instance
            .set_sample_rate(&main_thread(), 48000.0)
            .expect("accepted");

        assert_eq!(instance.loaded().inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(instance.loaded().total_inputs(), 3);
        let mut plugin = crate::plugin::Plugin::Clap(instance);
        assert!(
            plugin
                .poll_async_events(&main_thread())
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged)),
            "the host is told"
        );
        assert!(
            !plugin
                .poll_async_events(&main_thread())
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged)),
            "once"
        );
    }

    /// **Setting the rate the plugin already runs at changes nothing.**
    ///
    /// The early return is what keeps a per-frame caller from tearing the plugin
    /// down sixty times a second: the core plans a restart for every
    /// `Reconfigure`, so the equality check is the bridge's.
    ///
    /// Mutation (run): delete the same-rate early return in
    /// `ClapInstance::set_sample_rate` → the plugin is activated again and the
    /// count assertion fails.
    #[test]
    fn setting_the_current_sample_rate_does_not_cycle_the_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        instance
            .set_sample_rate(&main_thread(), 44100.0)
            .expect("re-setting the current rate must succeed");

        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            0,
            "setting the rate the plugin already runs at must not deactivate and \
             re-activate it"
        );
    }

    /// **A rate change that changes nothing about the layout is silent.**
    ///
    /// Whether the layout differs is the core's decision (`step`, mutation-tested
    /// in `tutti-plugin-types`); this pins that the bridge adds no notice of its
    /// own.
    ///
    /// Mutation (run): push `AsyncEvent::IoChanged` after every
    /// `HostOp::Reconfigure` in `ClapInstance::set_sample_rate` → the assertion
    /// fails.
    #[test]
    fn a_rate_change_that_keeps_the_layout_does_not_raise_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        instance
            .set_sample_rate(&main_thread(), 48000.0)
            .expect("accepted");
        assert!(!instance.take_io_changed());
    }

    /// **A 64-bit instance that cannot be re-activated renders silence without
    /// reaching the plugin, reports the failure, and tells the host.** The
    /// plugin drops its 64-bit flags; the rate change fails and leaves it
    /// deactivated, and the session kept calling `process` on it.
    ///
    /// Mutations (run): make the `None` arm of `ClapInstance::process` (no
    /// Processor held) return an error → `expect("silence")` fails. Delete the
    /// `AsyncEvent::IoChanged` push in the `Notice::Refused` arm of `absorb` →
    /// the `take_io_changed` assertion fails (the layout alone is unchanged in
    /// this fixture).
    #[test]
    fn a_deactivated_instance_renders_silence_and_tells_the_host() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, true).expect("load");
        assert!(
            instance.loaded().features.contains(Features::F64_AUDIO),
            "fixture"
        );

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let err = instance
            .set_sample_rate(&main_thread(), 48000.0)
            .expect_err("the 64-bit flags are gone");
        assert!(err.to_string().contains("deactivated"), "got {err}");
        assert!(instance.take_io_changed(), "the host is told");

        let input = vec![vec![0.25f64; 64]; 2];
        let input_slices: Vec<&[f64]> = input.iter().map(|v| v.as_slice()).collect();
        let mut output = vec![vec![0.5f64; 64]; 2];
        let mut output_slices: Vec<&mut [f64]> =
            output.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples: 64,
            sample_rate: 44100.0,
        };
        let mut out = tutti_plugin::server::ProcessOutput::default();
        instance
            .process(
                tutti_plugin::server::AudioBufferMut::F64(buffer),
                &ProcessContext::new(),
                &mut out,
            )
            .expect("silence");
        assert!(output.iter().flatten().all(|&s| s == 0.0), "silent");
        assert!(!instance.is_processing(), "the plugin was not started");
    }

    /// The `audio-ports.rescan` flags that need the plugin deactivated.
    ///
    /// The values are `ext/audio-ports.h`'s `CLAP_AUDIO_PORTS_RESCAN_*`.
    const RESCAN_NAMES: u32 = 1 << 0;
    const RESCAN_FLAGS: u32 = 1 << 1;
    const RESCAN_LIST_AND_CHANNELS: u32 = (1 << 5) | (1 << 2);

    /// **A rescan the plugin asks for is served: the instance is
    /// re-activated, its ports are read afresh and the host is told.** The
    /// request was recorded by the host and nothing acted on it, so a plugin
    /// that changed its ports left the host with the layout it was loaded
    /// with.
    ///
    /// Mutations (run): make `host_audio_ports_rescan` raise nothing → a request
    /// is never made and nothing happens → the activation count stays 0 → the
    /// second assertion fails. Delete `self.refresh_meta();` in the
    /// `Notice::IoChanged` arm of `ClapInstance::absorb` → the host copy stays
    /// stale and the layout assertion fails.
    #[test]
    fn a_ports_rescan_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        assert_eq!(instance.loaded().inputs.len(), 1, "fixture: one input bus");
        crate::test_utils::clap_probe_reset_activation_count();

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());
        assert_eq!(
            instance.loaded().inputs.len(),
            1,
            "nothing is acted on until the server serves it"
        );

        instance.service_requests();

        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "one re-activation"
        );
        assert!(instance.clap_loaded().is_active());
        assert_eq!(instance.loaded().inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(instance.loaded().total_inputs(), 3);
        assert!(instance.take_io_changed(), "the host is told");

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "the request was consumed: not served twice"
        );
    }

    /// **The request reaches the host as `IoChanged` through the server's
    /// between-blocks poll.**
    ///
    /// Mutation (run): delete the `AsyncEvent::IoChanged` push in the
    /// `Notice::IoChanged` arm of `ClapInstance::absorb` → no event → fails.
    #[test]
    fn a_served_ports_rescan_is_polled_as_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());

        let mut plugin = crate::plugin::Plugin::Clap(instance);
        let io_changed = |plugin: &mut crate::plugin::Plugin| {
            plugin
                .poll_async_events(&main_thread())
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged))
        };
        assert!(io_changed(&mut plugin), "the host is told");
        assert_eq!(plugin.instance().loaded().total_inputs(), 3);
        assert!(!io_changed(&mut plugin), "once");
    }

    /// **A rescan that re-activates but leaves the layout as it was is
    /// silent**, and a `NAMES`-only rescan, which the host's description does
    /// not carry, does not re-activate at all.
    ///
    /// Mutation (run): make `host_audio_ports_rescan` raise `IoChanged` for
    /// every rescan, `NAMES` included → the names-only count assertion fails.
    #[test]
    fn a_rescan_that_changes_nothing_is_silent() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());
        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(!instance.take_io_changed(), "same layout, same instance");

        let _names = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_NAMES);
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());
        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "names are safe live: no re-activation"
        );
        assert!(!instance.take_io_changed());
        assert!(instance.pending_requests().is_empty(), "consumed");
    }

    /// **A `request_restart` is served the same way**: the plugin is
    /// deactivated and activated again, between blocks, and keeps running.
    ///
    /// Mutation (run): make `host_request_restart` raise nothing → the count
    /// stays 0 → fails.
    #[test]
    fn a_restart_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, false).expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        let _asked = crate::test_utils::clap_probe_request_restart();
        process_blocks(&mut instance, 1);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);

        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(!instance.take_io_changed(), "same layout, same instance");
        process_blocks(&mut instance, 1);
        assert!(instance.is_processing(), "it runs again");

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "consumed"
        );
    }

    /// **A 64-bit instance whose plugin drops its flags and asks for a rescan
    /// is left deactivated, and the host is told** even though the layout it
    /// holds did not change.
    ///
    /// Mutation (run): delete the `AsyncEvent::IoChanged` push in the
    /// `Notice::Refused` arm of `ClapInstance::absorb` → `take_io_changed` is
    /// false → fails.
    #[test]
    fn a_rescan_that_leaves_the_instance_unable_to_run_tells_the_host() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let mut instance =
            ClapInstance::load(&main_thread(), path, 44100.0, 512, true).expect("load");
        assert!(
            instance.loaded().features.contains(Features::F64_AUDIO),
            "fixture"
        );

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_FLAGS);
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());
        instance.service_requests();

        assert!(!instance.clap_loaded().is_active(), "left deactivated");
        assert!(instance.take_io_changed(), "the host is told");
    }

    /// Loads the reference plugin and takes it past one block, so it has asked
    /// for a callback (`request_callback` from `process`).
    fn loaded_and_called_back() -> ClapInstance {
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        process_blocks(&mut instance, 1);
        instance
    }

    /// **A `request_callback` is served: the plugin's `on_main_thread` runs on
    /// the server's service pass, and not before.** The flag was recorded and
    /// nothing called back, so work a plugin deferred to the main thread (a
    /// sample load finishing, a timer being registered) never ran.
    ///
    /// Mutation (run): delete the `take_requests` call from
    /// `ClapInstance::serve` → the latched timer is never registered → fails.
    #[test]
    fn a_callback_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        crate::test_utils::clap_probe_thread_reset();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        // `on_main_thread` runs this one; it registers a timer.
        let _cmd = crate::test_utils::clap_probe_thread_command(
            tutti_clap_test_plugin::CMD_REGISTER_TIMER,
        );
        process_blocks(&mut instance, 1);
        let site = tutti_clap_test_plugin::Site::OnMainThread as usize;
        let before = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(before.sites[site].visits, 0, "not before it is served");

        instance.service_requests();

        let after = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(after.sites[site].visits, 1);
        assert!(after.timer_registered, "the latched command ran in it");
    }

    /// **A timer the plugin registers ticks on the service pass.** The host
    /// answered `register_timer` with `true` and never called `on_timer`.
    ///
    /// Mutation (run): delete the `Request::Timer` push from
    /// `ClapLoaded::take_requests_with` → no tick → fails.
    #[test]
    fn a_timer_the_plugin_registers_is_ticked() {
        let _lock = crate::test_utils::plugin_load_lock();
        crate::test_utils::clap_probe_thread_reset();
        let mut instance = loaded_and_called_back();
        let _cmd = crate::test_utils::clap_probe_thread_command(
            tutti_clap_test_plugin::CMD_REGISTER_TIMER,
        );
        // Run the callback that registers the timer (the block's request has
        // gone unserved), then let the timer tick.
        instance
            .loaded_mut_unwrapped()
            .on_main_thread(&main_thread());
        let site = tutti_clap_test_plugin::Site::OnTimer as usize;
        assert!(crate::test_utils::clap_probe_thread_capture().timer_registered);
        assert_eq!(
            crate::test_utils::clap_probe_thread_capture().sites[site].visits,
            0
        );

        instance.service_requests();

        let cap = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(cap.sites[site].visits, 1, "a period-0 timer is due at once");
        assert_eq!(cap.last_fired_timer_id, cap.timer_id);
    }

    /// **A latency the plugin settles on at activation is reported after a
    /// sample-rate change**, with no `changed()` from the plugin: the figure
    /// the host holds is from the old rate.
    ///
    /// Mutation (run): delete the `AsyncEvent::LatencyChanged` push in the
    /// `Notice::LatencyChanged` arm of `ClapInstance::absorb` → no change is
    /// reported → fails.
    #[test]
    fn a_latency_that_depends_on_the_rate_is_reread_after_a_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        let before = instance.loaded().latency_samples;
        assert_eq!(instance.take_changes().latency, None);

        let _longer = crate::test_utils::clap_probe_latency_at_activation(11);
        instance
            .set_sample_rate(&main_thread(), 48000.0)
            .expect("rate change");

        let now = Samples(before.0 + 11);
        assert_eq!(instance.take_changes().latency, Some(now));
        assert_eq!(instance.loaded().latency_samples, now, "the host's copy");
        assert_eq!(instance.take_changes().latency, None, "once");
    }

    /// **A `request_restart` reports the latency the plugin has after it.**
    ///
    /// Mutation (run): make `host_request_restart` raise nothing → no
    /// re-activation, so the figure is never re-read → fails.
    #[test]
    fn a_restart_reports_the_latency_after_it() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        let before = instance.loaded().latency_samples;
        let _longer = crate::test_utils::clap_probe_latency_at_activation(7);
        let _asked = crate::test_utils::clap_probe_request_restart();
        process_blocks(&mut instance, 1);

        instance.service_requests();

        assert_eq!(instance.take_changes().latency, Some(Samples(before.0 + 7)));
    }

    /// **`latency.changed` on an active plugin is served by a re-activation,
    /// and the latency is read after it**, not from the active plugin
    /// (`clap.latency`: it may change only across an activation).
    ///
    /// Mutation (run): make `ClapController::latency` answer `Samples(0)` → the
    /// figure read after the re-activation is not the plugin's → the latency
    /// assertion fails.
    #[test]
    fn latency_changed_on_an_active_plugin_restarts_it() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        let before = instance.loaded().latency_samples;
        crate::test_utils::clap_probe_reset_activation_count();
        let _longer = crate::test_utils::clap_probe_latency_at_activation(5);
        instance.raise(Request::LatencyChanged);

        instance.service_requests();

        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert_eq!(instance.take_changes().latency, Some(Samples(before.0 + 5)));
    }

    /// **A `latency.changed()` a plugin raises from inside `activate` is
    /// answered by that activation**: one request is one activation, not a
    /// second restart, and not a loop when the plugin does it every time.
    ///
    /// Mutation (run): keep every request raised inside `activate` (the filter
    /// in `ClapLoaded::activate_plugin` always true) → the load-time
    /// announcement is still raised, so the first `service_requests`
    /// re-activates for it (the first count assertion reads 1), and each
    /// activation raises it again (no loop guard).
    #[test]
    fn a_latency_changed_inside_activate_is_not_restarted_for() {
        let _lock = crate::test_utils::plugin_load_lock();
        let _announces = crate::test_utils::clap_probe_latency_changed_in_activate();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            0,
            "the announcement made while loading was answered by the load"
        );

        // One request, one activation, whose own announcement is answered too.
        instance.raise(Request::LatencyChanged);
        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        instance.service_requests();
        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "no loop"
        );
        assert!(instance.pending_requests().is_empty());
    }

    /// **A `params.rescan` of values is reported as changed values and does not
    /// restart the plugin.** The flags were latched and nothing read them.
    ///
    /// Mutation (run): delete the `Notice::ParamValuesChanged` push in
    /// `ClapInstance::absorb` → nothing is reported → fails.
    #[test]
    fn a_values_rescan_is_reported_without_a_restart() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = loaded_and_called_back();
        crate::test_utils::clap_probe_reset_activation_count();
        let _asked = crate::test_utils::clap_probe_param_command(
            tutti_clap_test_plugin::PARAM_CMD_RESCAN_VALUES,
        );

        instance.service_requests();

        let changes = instance.take_changes();
        assert!(changes.param_values);
        assert!(!changes.param_titles);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
    }

    /// **A `params.rescan(ALL)` restarts the plugin and is reported as changed
    /// parameters**, so the host re-pulls the list.
    ///
    /// Mutations (run): make `host_params_rescan` raise nothing for `ALL` → no
    /// re-activation → the count assertion fails. Delete the
    /// `Notice::ParamTitlesChanged` push in `ClapInstance::absorb` → the report
    /// assertion fails. What the re-read changes (the ranges automation is
    /// denormalized against) is not observable with a plugin whose parameters
    /// are fixed.
    #[test]
    fn an_all_rescan_restarts_the_plugin_and_reports_the_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = loaded_and_called_back();
        crate::test_utils::clap_probe_reset_activation_count();
        let _asked = crate::test_utils::clap_probe_param_command(
            tutti_clap_test_plugin::PARAM_CMD_RESCAN_ALL,
        );

        instance.service_requests();

        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(instance.clap_loaded().is_active());
        assert!(instance.take_changes().param_titles);
    }

    #[test]
    fn test_clap_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");

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

    /// CLAP lists nothing and can still load — a coherent state, not a
    /// contradiction.
    ///
    /// The mirror of VST3, which lists richly and cannot load. CLAP's
    /// enumeration lives in the factory-level preset-discovery extension, which
    /// is queried on the *factory* rather than a loaded instance and which this
    /// host does not bind; `CLAP_EXT_PRESET_LOAD` answers only "can this be
    /// pointed at a path".
    ///
    /// So the empty list is **not** the same claim AU's empty list makes.
    /// `PRESET_LIST` is unprobed for CLAP, so a caller reads `None` — nobody
    /// asked — where AU reports `Some(false)`, meaning the unit declined. That
    /// distinction is the whole reason the two bits are separate, and asserting
    /// it here is what keeps a future "simplification" from collapsing them.
    #[test]
    fn clap_lists_nothing_and_still_reports_a_load_capability() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");

        assert!(
            instance.get_presets(&main_thread()).is_empty(),
            "CLAP has no instance-level enumeration; a non-empty list would mean \
             something invented one"
        );
        assert_eq!(
            instance.get_current_preset(&main_thread()),
            None,
            "CLAP has no current-preset query either"
        );

        let loaded = instance.loaded();
        let report = tutti_plugin::server::FeatureReport::new(loaded.probed, loaded.features);
        assert_eq!(
            report.get(tutti_plugin::server::Features::PRESET_LIST),
            None,
            "PRESET_LIST must read as unasked for CLAP, not as a declination"
        );
    }

    /// Only a path-shaped id reaches CLAP's loader.
    ///
    /// `from_location` takes a filesystem path and nothing else, so a `Number`
    /// or `Program` names no CLAP preset. There is not even a number to coerce it *to*,
    /// which is why the id is opaque rather than an integer.
    #[test]
    fn clap_refuses_an_id_that_is_not_a_path() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");

        // And the whole path still refuses, which is what a caller sees.
        assert!(!instance.load_preset(&main_thread(), &PresetId::Number(0)));
    }

    #[test]
    fn test_clap_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = instance.get_parameter(&main_thread(), first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    /// `CLAP_EVENT_PARAM_VALUE`, as `clap_sys` spells it; the server names no
    /// `clap-sys` itself.
    const CLAP_EVENT_PARAM_VALUE: u16 = 5;

    /// What the reference plugin recorded of its last `process` call.
    fn read_process_capture() -> tutti_clap_test_plugin::ProcessCapture {
        type F = unsafe extern "C" fn(*mut tutti_clap_test_plugin::ProcessCapture) -> bool;
        let mut cap = tutti_clap_test_plugin::ProcessCapture::default();
        // SAFETY: `#[no_mangle] extern "C"` in `tutti-clap-test-plugin`; the
        // handle is leaked as in `set_clap_probe_switch`.
        unsafe {
            let lib = libloading::Library::new(clap_plugin()).expect("reference plugin opens");
            let capture: libloading::Symbol<F> = lib
                .get(b"tutti_test_plugin_capture\0")
                .expect("capture exported by the reference plugin");
            assert!(capture(&mut cap), "the plugin recorded a process() call");
            std::mem::forget(lib);
        }
        cap
    }

    /// Arm (or, with 0, disarm) the reference plugin's thread-pool probe on
    /// the image the loader opens; see `set_clap_probe_render_mode` for why
    /// through `dlopen`.
    fn set_thread_pool_tasks(tasks: u32) {
        // SAFETY: `#[no_mangle] extern "C" fn(u32)` in the reference plugin.
        unsafe {
            let lib = libloading::Library::new(clap_plugin()).expect("reference plugin opens");
            let set: libloading::Symbol<unsafe extern "C" fn(u32)> = lib
                .get(b"tutti_test_plugin_set_thread_pool_tasks\0")
                .expect("thread-pool switch exported");
            set(tasks);
            // Leaked: closing it before the load would unmap the switch.
            std::mem::forget(lib);
        }
    }

    fn thread_pool_capture() -> tutti_clap_test_plugin::ThreadPoolCapture {
        // SAFETY: `#[no_mangle] extern "C" fn() -> ThreadPoolCapture`, the
        // same `#[repr(C)]` type as the rlib's.
        unsafe {
            let lib = libloading::Library::new(clap_plugin()).expect("reference plugin opens");
            let get: libloading::Symbol<
                unsafe extern "C" fn() -> tutti_clap_test_plugin::ThreadPoolCapture,
            > = lib
                .get(b"tutti_test_plugin_thread_pool_capture\0")
                .expect("thread-pool capture exported");
            let cap = get();
            std::mem::forget(lib);
            cap
        }
    }

    fn process_blocks(instance: &mut ClapInstance, blocks: usize) {
        let num_samples = 64;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        for _ in 0..blocks {
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };
            let mut out = tutti_plugin::server::ProcessOutput::default();
            instance
                .process(
                    tutti_plugin::server::AudioBufferMut::F32(buffer),
                    &ProcessContext::new(),
                    &mut out,
                )
                .expect("block processes");
        }
    }

    /// A plugin that implements `clap.thread-pool` has its requests served
    /// by the server's own pool: the host's pool is in another process.
    ///
    /// Mutation: drop the `set_thread_pool` call in `ClapInstance::load` →
    /// every request is refused → `accepted` is 0 → fails (on 4 cores or
    /// more; below that the server builds no pool and the test checks the
    /// refusal).
    #[test]
    fn a_thread_pool_plugin_has_its_requests_served_here() {
        let _lock = crate::test_utils::plugin_load_lock();
        set_thread_pool_tasks(8);
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load CLAP plugin");
        process_blocks(&mut instance, 4);
        set_thread_pool_tasks(0);
        let cap = thread_pool_capture();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        // Below 4 cores the server builds no pool, and requests are refused.
        let served = if pool_participants(cores) >= 2 { 4 } else { 0 };
        assert_eq!(cap.requests, 4, "{cap:?}");
        assert_eq!((cap.accepted, cap.refused), (served, 4 - served), "{cap:?}");
        assert_eq!(cap.accepted_incomplete, 0);
    }

    /// A plugin without the extension costs no threads: the pool is built
    /// only for one that has it.
    ///
    /// `THREAD_POOL` is process-global, so this relies on nextest running
    /// each test in its own process: under `cargo test`, a thread-pool test
    /// that ran first in the same process would have built it.
    ///
    /// Mutation: build the pool unconditionally in `ClapInstance::load` →
    /// it exists after this load → fails.
    /// Half the cores, at most 4, and below 2 no pool.
    ///
    /// Mutation: drop the `.min(4)` → 64 cores give 32 → fails.
    #[test]
    fn the_server_pool_takes_half_the_cores_up_to_four() {
        let sizes: Vec<usize> = [1, 2, 3, 4, 6, 8, 64].map(pool_participants).to_vec();
        assert_eq!(sizes, [0, 1, 1, 2, 3, 4, 4]);
    }

    #[test]
    fn a_plugin_without_a_thread_pool_builds_no_pool() {
        let _lock = crate::test_utils::plugin_load_lock();
        set_thread_pool_tasks(0);
        let _instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load CLAP plugin");
        assert!(THREAD_POOL.get().is_none());
    }

    #[test]
    fn test_clap_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        // Should not panic
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    /// The host feeds a NoteOn and collects audio back.
    ///
    /// The reference plugin is an effect, not a synth, so what this pins is the
    /// *round trip* — events reach the plugin and its output reaches the host —
    /// rather than note-to-pitch synthesis. `TagOnly` makes the output side
    /// observable: the probe writes a nonzero tag into every slot regardless of
    /// input, so a host that dropped the output buffers reads as silence.
    #[test]
    fn test_clap_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(&main_thread(), path, 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");
        crate::test_utils::set_clap_probe_render_mode(
            crate::test_utils::CLAP_PROBE_RENDER_TAG_ONLY,
        );

        let num_samples = 512;

        // First block: send a NoteOn event
        let note_on = [(
            tutti_graph::Offset::ZERO,
            tutti_fixtures::midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::of(1),
                60,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )];

        let mut has_nonzero = false;

        // Process the block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = ProcessContext::new().midi(&note_on);
            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );

            // Check output for non-zero samples
            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks (at least 4 more) to give the synth time to produce sound
        let empty_ctx = ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &empty_ctx,
                &mut out,
            );

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

    // ── Second-instance tests ──
    //
    // These ran against a second, separately-installed plugin (Surge XT at an
    // absolute macOS path). The group was headed "f64-capable", but nothing in
    // it asserts f64 — Surge does not advertise
    // `CLAP_AUDIO_PORT_SUPPORTS_64BITS` either, as `test_surge_port_flags`
    // itself notes. What the group actually covers is a *second* load of the
    // host against a different instance: stereo output, process-without-crash,
    // state round-trip, voice info. Pointing it at the reference plugin keeps
    // that coverage and lets it run anywhere.
    //
    // The f64 suite is the session's (`an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64`
    // and its neighbours): the reference plugin advertises 64-bit ports behind
    // `tutti_test_plugin_set_ports_64bit`.

    #[test]
    fn test_surge_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        );
        assert!(
            instance.is_ok(),
            "Failed to load Surge XT: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty());
        assert!(!meta.id.is_empty());
        println!("Surge XT loaded: {} ({})", meta.name, meta.id);
    }

    #[test]
    fn test_surge_port_flags() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let port_info = instance
            .clap_loaded()
            .audio_port_info(&main_thread(), 0, false);
        assert!(port_info.is_some(), "Expected at least one output port");

        let port = port_info.unwrap();
        assert!(port.layout.count() >= 2, "Expected stereo output");
    }

    #[test]
    fn test_surge_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_surge_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let num_samples = 512;
        let note_on = [(
            tutti_graph::Offset::ZERO,
            tutti_fixtures::midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::of(1),
                60,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )];

        // Process block with NoteOn + follow-up blocks.
        // Surge XT is strict about CLAP thread contracts and may not produce
        // audio when process() is called from a non-audio thread, so this only
        // verify no crash rather than asserting on output content.
        for i in 0..5 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = if i == 0 {
                ProcessContext::new().midi(&note_on)
            } else {
                ProcessContext::new()
            };

            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );
        }
    }

    /// Test Surge XT parameter setting via flush.
    #[test]
    fn test_surge_set_parameter_flush() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        instance.set_parameter(&main_thread(), param_id, Normalized::of(0.5));
    }

    // ── Group A: Plugin Lifecycle ──

    #[test]
    fn test_clap_lifecycle_active_after_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");
        // `load()` activates the plugin: the lifecycle is `Active`, which holds
        // a Controller, so the "active after load" guarantee is in the type.
        assert!(
            matches!(instance.lifecycle, Lifecycle::Active { .. }),
            "load() should yield an active instance"
        );
    }

    #[test]
    fn test_clap_lifecycle_processing_starts_on_process() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // A freshly-activated instance has not started processing yet — that
        // happens lazily on the first `process` call (the type guarantees it is
        // already activated, so there is no "auto-activate" step to test).
        assert!(
            !instance.is_processing(),
            "Should not be processing before first process()"
        );

        let num_samples = 128;
        let input_data: Vec<Vec<f32>> = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };
        let mut out = tutti_plugin::server::ProcessOutput::default();
        instance
            .process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ProcessContext::new(),
                &mut out,
            )
            .expect("process should succeed");

        assert!(
            instance.is_processing(),
            "Should be processing after first process()"
        );
    }

    // ── Group B: Polling Methods ──

    #[test]
    fn test_clap_take_initially_clear() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Nothing is pending on a fresh instance.
        assert!(instance.pending_requests().is_empty());
        let loaded = instance.clap_loaded();
        assert!(!loaded.take_process_requested());
        assert!(!loaded.take_params_flush_requested());
        assert!(!loaded.take_state_dirty());
        assert!(!loaded.take_note_ports_changed());
    }

    /// A request raised from a callback is handed over once.
    ///
    /// Mutation (run): `take_requests_with` reading the word without clearing
    /// it → the second take repeats the request.
    #[test]
    fn test_clap_requests_are_taken_once() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        for request in [
            Request::Restart,
            Request::Callback,
            Request::LatencyChanged,
            Request::TailChanged,
            Request::IoChanged,
            Request::RescanParams(tutti_plugin_types::RescanScope::All),
        ] {
            instance.raise(request);
            assert_eq!(instance.pending_requests(), [request]);
            assert!(
                instance.pending_requests().is_empty(),
                "{request:?} was taken"
            );
        }
    }

    #[test]
    fn test_clap_take_flags() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let state = instance.clap_loaded().host_state();
        state
            .lifecycle
            .process_requested
            .store(true, Ordering::Release);
        state.notes.ports_changed.store(true, Ordering::Release);
        state.processing.state_dirty.store(true, Ordering::Release);

        let loaded = instance.clap_loaded();
        assert!(loaded.take_process_requested());
        assert!(loaded.take_note_ports_changed());
        assert!(loaded.take_state_dirty());

        // Second take should be false (cleared)
        assert!(!loaded.take_process_requested());
        assert!(!loaded.take_note_ports_changed());
        assert!(!loaded.take_state_dirty());
    }

    /// **A write to an active plugin reaches a state save and a readback
    /// without waiting for a block.** The server serves requests on the thread
    /// that renders, so a save or a fork right after a write must see it; the
    /// pending write is flushed when the state is asked for, not at every write.
    ///
    /// Mutation (run): delete the `self.settle_edits()` call in
    /// `ClapInstance::get_state` → the readback is still the original and the
    /// last assertion fails; flush at every write instead (the old behaviour)
    /// → the "queued, not applied" assertion fails.
    #[test]
    fn test_clap_param_write_is_settled_for_a_state_save() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");
        let params = instance.get_parameter_list(&main_thread());
        let param_id = params[0].id;
        let original = instance.get_parameter(&main_thread(), param_id);
        let new_value = if original < 0.5 { 0.75 } else { 0.25 };

        instance.set_parameter(&main_thread(), param_id, Normalized::clamped(new_value));
        assert_eq!(
            instance.get_parameter(&main_thread(), param_id),
            original,
            "queued, not applied out of band"
        );
        instance.get_state(&main_thread()).expect("state saves");
        let readback = instance.get_parameter(&main_thread(), param_id);
        assert!(
            (readback - new_value).abs() < 0.01,
            "expected {new_value}, got {readback}"
        );
    }

    /// A parameter that requires `process` is never flushed out of band while
    /// the plugin is processing; any other is.
    ///
    /// Mutation (run): `flush_out_of_band` returning `true` → the first
    /// assertion fails; returning `false` → the others fail.
    #[test]
    fn a_requires_process_parameter_is_not_flushed_while_processing() {
        assert!(!flush_out_of_band(true, true));
        assert!(flush_out_of_band(true, false), "before the first block");
        assert!(flush_out_of_band(false, true));
        assert!(flush_out_of_band(false, false));
    }

    /// **A rate that is no rate is refused before the plugin is touched**: the
    /// instance runs on at the configuration it has, the plugin is not activated
    /// again, and the error says where it runs.
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every rate
    /// -> the zero rate reaches the reconfigure -> the `expect_err` fails (or the
    /// activation count moves).
    #[test]
    fn a_rate_that_is_no_rate_is_refused_before_the_plugin_is_touched() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        crate::test_utils::clap_probe_reset_activation_count();
        let prepare = instance.state.prepare();
        for rate in [0.0, -44_100.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = instance
                .set_sample_rate(&main_thread(), rate)
                .expect_err("a rate that is no rate is refused");
            assert!(err.to_string().contains("running at 44100"), "{err}");
            assert!(instance.is_active(), "{rate}");
        }
        assert_eq!(instance.state.prepare(), prepare);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
        instance
            .set_sample_rate(&main_thread(), 48_000.0)
            .expect("a real rate");
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
    }

    /// **A load at no rate or no block is refused** at the activation stage:
    /// zero, negative and non-finite rates, and a block of no frames. The plugin
    /// is never activated.
    ///
    /// Mutation (run): `valid_rate` accepting every rate, and `MaxBlock::new`
    /// accepting zero, each -> the matching case loads -> fails.
    #[test]
    fn a_load_at_no_rate_or_no_block_is_refused() {
        let _lock = crate::test_utils::plugin_load_lock();
        crate::test_utils::clap_probe_reset_activation_count();
        for (rate, block) in [
            (0.0, 512),
            (-44_100.0, 512),
            (f64::NAN, 512),
            (f64::INFINITY, 512),
            (f64::NEG_INFINITY, 512),
            (44_100.0, 0),
        ] {
            match ClapInstance::load(&main_thread(), Path::new(clap_plugin()), rate, block, false) {
                Err(BridgeError::LoadFailed {
                    stage: LoadStage::Activation,
                    reason,
                    ..
                }) => assert!(reason.contains("cannot run at"), "{reason}"),
                Err(other) => panic!("{rate} Hz in {block} frames: {other:?}"),
                Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
            }
        }
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
    }

    /// **A refused rate is rolled back**: the plugin is activated again at the
    /// configuration it already accepted, stays active there, renders, and the
    /// error says where it is running. The core tells the host the instance was
    /// revived (`IoChanged`), as it does for any revival.
    ///
    /// Mutation (run): delete the `HostOp::Activate(previous)` drive in
    /// `ClapInstance::set_sample_rate` → the plugin stays loaded and silent:
    /// `is_active` fails.
    #[test]
    fn a_refused_rate_is_rolled_back_and_the_plugin_keeps_running() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        let refusal = crate::test_utils::clap_probe_refuse_rate(96000.0);
        let err = instance
            .set_sample_rate(&main_thread(), 96000.0)
            .expect_err("the plugin refuses 96 kHz");
        drop(refusal);

        assert!(err.to_string().contains("running at 44100"), "{err}");
        assert!(instance.is_active(), "rolled back and running");
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert_eq!(
            crate::test_utils::clap_probe_last_accepted_rate(),
            Some(44100.0)
        );
        assert!(
            instance.take_io_changed(),
            "the core tells the host the instance was revived"
        );
        process_blocks(&mut instance, 1);
    }

    /// **A render-mode toggle does not cycle the plugin**, and the mode is kept
    /// for the activation a later restart makes.
    ///
    /// Mutation (run): drive `HostOp::Reconfigure` from `set_render_mode`
    /// instead of `State::set_mode` → the plugin is activated again and the
    /// count assertion fails; delete the `set_mode` call → the recorded mode
    /// stays `Realtime` and the mode assertion fails.
    #[test]
    fn a_render_mode_toggle_does_not_cycle_the_plugin() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        instance.set_render_mode(&main_thread(), RenderMode::Offline);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
        assert_eq!(
            instance.state.prepare().map(|p| p.mode()),
            Some(RenderMode::Offline)
        );
        instance.set_render_mode(&main_thread(), RenderMode::Realtime);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
        assert!(instance.is_active());
    }

    // ── Group C: Parameter Get/Set Roundtrip ──

    /// **A parameter written to an active plugin also reaches the next block**
    /// as a `PARAM_VALUE` at its head, in the plugin's plain range.
    ///
    /// Mutation (run): `process` not draining the edit ring → the block carries
    /// no event for the id and the `expect` fails.
    #[test]
    fn test_clap_param_set_reaches_the_next_block() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        let clap_id = param_id.get();
        let original = instance.get_parameter(&main_thread(), param_id);

        // Set to a different value
        let new_value = if original < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(&main_thread(), param_id, Normalized::clamped(new_value));
        // The bridge also applies the write now (see `set_parameter`), and the
        // queued copy reaches the next block.
        process_blocks(&mut instance, 1);

        // The reference plugin records the events it is handed, so the write is
        // seen where it lands: as a `PARAM_VALUE` for this id, in the plugin's
        // plain range, at the head of the block.
        let (min, max) = instance
            .clap_loaded()
            .parameter_range(&main_thread(), clap_id)
            .expect("the parameter declares a range");
        let capture = read_process_capture();
        let event = capture.events[..capture.event_count as usize]
            .iter()
            .find(|e| e.event_type == CLAP_EVENT_PARAM_VALUE && e.param_id == clap_id)
            .expect("the block carried the queued write");
        assert_eq!(event.time, 0, "applied at the head of the block");
        let expected = min + new_value * (max - min);
        assert!(
            (event.value - expected).abs() < 1e-9,
            "expected {expected}, the plugin was handed {}",
            event.value
        );
    }

    // ── Group D: State Save/Load Roundtrip ──

    #[test]
    fn test_clap_state_save_nonempty() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let state = instance
            .get_state(&main_thread())
            .expect("save_state should succeed");
        assert!(!state.is_empty(), "Saved state should not be empty");
    }

    #[test]
    fn test_clap_state_save_load_roundtrip() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty(), "Need at least one parameter");
        let param_id = params[0].id;

        // Save original state
        let saved = instance
            .get_state(&main_thread())
            .expect("save should succeed");
        let original_value = instance.get_parameter(&main_thread(), param_id);

        // Change parameter
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(&main_thread(), param_id, Normalized::clamped(new_value));

        // Restore state
        instance
            .set_state(&main_thread(), &saved)
            .expect("restore should succeed");

        let restored_value = instance.get_parameter(&main_thread(), param_id);
        assert!(
            (restored_value - original_value).abs() < 0.01,
            "Parameter should be restored: expected {}, got {}",
            original_value,
            restored_value
        );
    }

    // ── Group E: Audio/Note Port Enumeration ──

    #[test]
    fn test_clap_audio_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let output_count = instance
            .clap_loaded()
            .audio_port_count(&main_thread(), false);
        assert!(
            output_count > 0,
            "Synth should have at least one output port"
        );

        for i in 0..output_count {
            let info = instance
                .clap_loaded()
                .audio_port_info(&main_thread(), i, false)
                .expect("audio_port_info should return Some");
            assert!(info.layout.count() > 0, "Port {} should have channels", i);
            assert!(!info.name.is_empty(), "Port {} should have a name", i);
        }
    }

    #[test]
    fn test_clap_note_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let input_count = instance.clap_loaded().note_port_count(&main_thread(), true);
        assert!(
            input_count > 0,
            "Synth should have at least one note input port"
        );

        for i in 0..input_count {
            let info = instance
                .clap_loaded()
                .note_port_info(&main_thread(), i, true)
                .expect("note_port_info should return Some");
            assert!(!info.name.is_empty(), "Note port {} should have a name", i);
        }
    }

    // ── Group F: Latency/Tail Queries ──

    #[test]
    fn test_clap_latency_and_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Just verify no crash — values depend on plugin
        let _latency = instance.clap_loaded().latency_samples(&main_thread());
        let _tail = instance.clap_loaded().tail_samples(&main_thread());
    }

    // ── Group G: Processing with Param Automation, Expressions, Transport ──

    #[test]
    fn test_clap_process_with_param_automation() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty());

        let mut changes = ParameterChanges::new();
        let mut queue = ParameterQueue::with_capacity(params[0].id, 1);
        queue.add_point(0, 0.5);
        changes.add_queue(queue);

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().params(&changes);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_process_with_note_expression() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let note_on = [(
            tutti_graph::Offset::ZERO,
            tutti_fixtures::midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::of(1),
                60,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )];

        let mut expr_changes = NoteExpressionChanges::new();
        expr_changes.add_change(NoteExpressionValue {
            sample_offset: 0,
            note_id: -1,
            expression_type: NoteExpressionType::Volume,
            value: 0.8,
        });

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new()
            .midi(&note_on)
            .note_expression(&expr_changes);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_process_with_transport() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let transport = TransportInfo::new()
            .with_playing(true)
            .with_tempo(120.0)
            .with_time_signature(tutti_plugin::server::TimeSignature::default());

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().transport(&transport);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    // ── Group I: Surge XT Additional Coverage ──

    #[test]
    fn test_surge_state_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let params = instance.get_parameter_list(&main_thread());
        assert!(!params.is_empty());
        let param_id = params[0].id;

        let saved = instance
            .get_state(&main_thread())
            .expect("save should succeed");
        assert!(!saved.is_empty(), "Surge XT state should not be empty");

        let original_value = instance.get_parameter(&main_thread(), param_id);
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(&main_thread(), param_id, Normalized::clamped(new_value));

        instance
            .set_state(&main_thread(), &saved)
            .expect("restore should succeed");

        let restored = instance.get_parameter(&main_thread(), param_id);
        assert!(
            (restored - original_value).abs() < 0.01,
            "Surge XT param should be restored: expected {}, got {}",
            original_value,
            restored
        );
    }

    #[test]
    fn test_surge_audio_ports() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let output_count = instance
            .clap_loaded()
            .audio_port_count(&main_thread(), false);
        assert!(output_count > 0, "Surge XT should have output ports");

        let port = instance
            .clap_loaded()
            .audio_port_info(&main_thread(), 0, false)
            .expect("Should have at least one output port");
        assert!(
            port.layout.count() >= 2,
            "Expected stereo output, got {} channels",
            port.layout.count()
        );
    }

    #[test]
    fn test_surge_latency_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        let _latency = instance.clap_loaded().latency_samples(&main_thread());
        let _tail = instance.clap_loaded().tail_samples(&main_thread());
    }

    // ── Group J: GUI / Editor ──

    /// Editor presence is *answered*, not left unknown.
    ///
    /// The reference plugin exposes no `clap.gui`, so the expected answer here
    /// is `Absent` — the load path must have asked and recorded the negative.
    /// Asserting `!= Unknown` is the load-bearing half: `Unknown` is the
    /// `Default`, so a descriptor built by struct-update that never consulted
    /// the plugin would carry it, and a host that skipped the query entirely
    /// would look identical to one that asked and got "no". Pinning `Absent`
    /// specifically also keeps a host that guesses `Present` for every plugin
    /// from passing.
    #[test]
    fn test_clap_has_gui() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");
        assert_eq!(
            instance.descriptor().editor,
            EditorPresence::Absent,
            "the reference plugin exposes no clap.gui, so the host must report \
             a queried absence rather than the `Unknown` default"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_clap_gui
    fn test_clap_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let parent = create_nsview();
        assert!(!parent.is_null(), "Failed to create NSView");

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(&main_thread(), handle, ContentScale::IDENTITY);
        assert!(
            result.is_ok(),
            "open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor(&main_thread());
        release_nsview(parent);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_surge_gui
    fn test_surge_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        assert!(
            instance.descriptor().editor.is_present(),
            "Surge XT should have a GUI"
        );

        let parent = create_nsview();
        assert!(!parent.is_null());

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(&main_thread(), handle, ContentScale::IDENTITY);
        assert!(
            result.is_ok(),
            "Surge XT open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "Surge XT GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor(&main_thread());
        release_nsview(parent);
    }

    // ── Group K: Render Mode ──

    #[test]
    fn test_clap_render_mode() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Set offline mode — returns true if plugin supports render extension
        let _ = instance
            .loaded_mut_unwrapped()
            .set_render_mode(&main_thread(), true);
        // Set back to realtime
        let _ = instance
            .loaded_mut_unwrapped()
            .set_render_mode(&main_thread(), false);
        // No crash is the assertion
    }

    #[test]
    fn test_clap_has_hard_realtime_requirement() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Most synths don't have hard RT requirements
        let _has_rt = instance
            .clap_loaded()
            .has_hard_realtime_requirement(&main_thread());
    }

    // ── Group L: Voice Info ──

    #[test]
    fn test_clap_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // TAL-NoiseMaker may or may not support voice info
        if let Some(info) = instance.clap_loaded().voice_info(&main_thread()) {
            assert!(info.voice_count > 0, "voice_count should be > 0");
            assert!(info.voice_capacity > 0, "voice_capacity should be > 0");
        }
    }

    #[test]
    fn test_surge_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load Surge XT");

        // Surge XT likely supports voice info
        if let Some(info) = instance.clap_loaded().voice_info(&main_thread()) {
            assert!(info.voice_count > 0, "Surge XT voice_count should be > 0");
            assert!(
                info.voice_capacity > 0,
                "Surge XT voice_capacity should be > 0"
            );
        }
    }

    // ── Group M: Note Names ──

    #[test]
    fn test_clap_note_names() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        let count = instance.clap_loaded().note_name_count(&main_thread());
        // Iterate whatever's there — may be 0 for synths without custom note names
        for i in 0..count {
            let name = instance.clap_loaded().note_name(&main_thread(), i);
            assert!(name.is_some(), "note_name at index {} should exist", i);
            assert!(
                !name.unwrap().name.is_empty(),
                "note_name {} should have a name",
                i
            );
        }
    }

    // ── Group N: Sample Rate Cycling ──

    #[test]
    fn test_clap_sample_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Change sample rate — this deactivates, updates, and requires reactivation
        instance
            .set_sample_rate(&main_thread(), 48000.0)
            .expect("rate accepted");

        // Process should still work (auto-reactivates)
        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 48000.0,
        };

        let ctx = ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_sample_rate_cycle_multiple() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Cycle through several sample rates
        for &rate in &[48000.0, 96000.0, 44100.0, 22050.0] {
            instance
                .set_sample_rate(&main_thread(), rate)
                .expect("rate accepted");

            let num_samples = 512;
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: rate,
            };

            let ctx = ProcessContext::new();
            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );
        }
    }

    // ── Group O: State Context ──

    #[test]
    fn test_clap_state_context_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Just query — may or may not be supported
        let _supports = instance.clap_loaded().supports_state_context();
    }

    #[test]
    fn test_clap_state_context_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // Save with ForProject context (falls back to regular save if unsupported)
        let saved = instance
            .clap_loaded()
            .state_with_context(&main_thread(), tutti_clap_host::StateContext::ForProject)
            .expect("state_with_context should succeed");
        assert!(!saved.is_empty());

        // Load it back
        instance
            .loaded_mut_unwrapped()
            .set_state_with_context(
                &main_thread(),
                &saved,
                tutti_clap_host::StateContext::ForProject,
            )
            .expect("set_state_with_context should succeed");
    }

    #[test]
    fn test_clap_state_context_for_duplicate() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(
            &main_thread(),
            Path::new(clap_plugin()),
            44100.0,
            512,
            false,
        )
        .expect("Failed to load");

        // ForDuplicate context — used when duplicating a plugin instance
        let saved = instance
            .clap_loaded()
            .state_with_context(&main_thread(), tutti_clap_host::StateContext::ForDuplicate)
            .expect("save should succeed");
        assert!(!saved.is_empty());

        instance
            .loaded_mut_unwrapped()
            .set_state_with_context(
                &main_thread(),
                &saved,
                tutti_clap_host::StateContext::ForDuplicate,
            )
            .expect("load should succeed");
    }

    // Objective-C runtime FFI for NSView creation (macOS only).
    // On Apple Silicon, objc_msgSend must be cast to the correct function
    // pointer type — the variadic extern "C" declaration doesn't work.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend();
    }

    #[cfg(target_os = "macos")]
    type ObjcMsgSendFn =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;

    #[cfg(target_os = "macos")]
    fn objc_send() -> ObjcMsgSendFn {
        unsafe { std::mem::transmute(objc_msgSend as *const ()) }
    }

    /// Ensure NSApplication is initialized (required for plugin GUIs).
    #[cfg(target_os = "macos")]
    fn ensure_nsapp() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSApplication".as_ptr());
            let shared_sel = sel_registerName(c"sharedApplication".as_ptr());
            send(cls, shared_sel);
        });
    }

    #[cfg(target_os = "macos")]
    fn create_nsview() -> *mut std::ffi::c_void {
        ensure_nsapp();
        unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSView".as_ptr());
            let alloc_sel = sel_registerName(c"alloc".as_ptr());
            let init_sel = sel_registerName(c"init".as_ptr());

            let obj = send(cls, alloc_sel);
            send(obj, init_sel)
        }
    }

    #[cfg(target_os = "macos")]
    fn release_nsview(view: *mut std::ffi::c_void) {
        unsafe {
            let send = objc_send();
            let release_sel = sel_registerName(c"release".as_ptr());
            send(view, release_sel);
        }
    }
}
