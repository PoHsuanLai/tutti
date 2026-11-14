//! [`Served<F>`]: one plugin of any [`OutOfProcess`] format, in this process.
//!
//! The host crate of each format is the interpreter of its plugin's lifecycle
//! (activation, restarts, requests). This is the server's one shell over them:
//! it loads through [`OutOfProcess::load_served`], activates through
//! [`Loaded::activate`](tutti_plugin_types::Loaded::activate) into the realtime [`Processor`] and the main-thread
//! [`Controller`](tutti_plugin_types::Controller), and runs both through
//! [`Lifecycle`], so a format is a type argument and a row of the
//! `served!` table, not a loader.
//!
//! What the formats still differ in, that only this process does, is
//! [`ServerSide`]'s.
//!
//! What the plugin tells the host between blocks is [`Served::poll`]'s answer:
//! each [`Notice`] of the core as the [`BridgeMessage`] that carries it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tutti_plugin_ipc::BridgeMessage;
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, AutomationMode, Control, Effect, Event, Features, Hand, HostOp,
    Lifecycle, Load, LoadStage, LoadedPlugin, MAX_BLOCK_CHANNELS, MainThreadMarker, MainWorkHandle,
    MaxBlock, Normalized, Notice, OutOfProcess, ParamEdit, ParamId, ParameterInfo,
    PluginDescriptor, PluginError, PluginIo, PluginResult, Prepare, Preset, PresetId,
    ProcessContext, ProcessOutput, ProcessWake, Processor, RenderMode, RenderModeSupport, Request,
    Sample, SampleFormat, SampleRate, Samples, Seat, Stage, State, StateUse, TrackInfo,
};

mod any;
mod format;

pub(crate) use any::AnyServed;
pub(crate) use format::ServerSide;

#[cfg(test)]
mod tests;

/// What the plugin was asked to render with: the block's audio, in the width
/// the plugin was activated at.
///
/// The pipeline's one door into a plugin, so its tests drive a stand-in that
/// is not a plugin.
pub(crate) trait Render {
    /// What the plugin can consume, as it reports now.
    fn features(&self) -> Features;

    /// Renders one block, or fills it with silence when the plugin is not
    /// running (a rate it refused left it deactivated).
    ///
    /// `out` is the caller's and reused: it is cleared before it is filled.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] (or another variant) when the plugin failed
    /// the block.
    fn render(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()>;
}

/// What the session asks of the loaded plugin, with its format erased.
///
/// Object-safe, implemented by [`Served<F>`] alone; the
/// `served!` table's enum lends it as a trait object.
pub(crate) trait ServedPlugin: Render {
    /// The plugin's catalog identity.
    fn descriptor(&self) -> &PluginDescriptor;
    /// The plugin's wiring as last read: buses, latency, tail, capabilities.
    fn loaded(&self) -> &LoadedPlugin;
    /// The sample format the session renders this plugin in: 64-bit only when
    /// it was asked for **and** the plugin runs at it ([`Features::F64_AUDIO`]),
    /// never the request alone.
    fn negotiated(&self, prefer_f64: bool) -> SampleFormat {
        if prefer_f64 && self.loaded().features.contains(Features::F64_AUDIO) {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        }
    }
    /// The plugin's control surface, which is also its editor's window calls
    /// ([`EditorWindow`](tutti_plugin_types::EditorWindow)): the editor of a
    /// served plugin opens on the instance itself.
    /// `None` when the plugin is lost.
    fn control_mut(&mut self) -> Option<&mut dyn Control>;
    /// Acts on what the plugin asked of the host since the last call, and
    /// returns what the host should be told. Between blocks, on the thread
    /// that loaded the plugin.
    fn poll(&mut self, main: MainThreadMarker) -> Vec<BridgeMessage>;
    /// Brings the Processor home from the render thread and deactivates the
    /// plugin, so it can be dropped. `false` when the Processor did not come
    /// (a render thread stuck inside the plugin): dropping the plugin then
    /// leaks it.
    fn quiesce(&mut self, main: MainThreadMarker) -> bool;
    /// Follows the host's rate.
    ///
    /// # Errors
    ///
    /// The plugin is not running at `rate`: it keeps the rate it already
    /// accepted, or renders silence until a rate it takes.
    fn set_sample_rate(&mut self, main: MainThreadMarker, rate: SampleRate) -> PluginResult<()>;
    /// Tells the plugin its render mode; whether it now runs in it.
    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> bool;
    /// How many parameters the plugin has.
    fn parameter_count(&self, main: MainThreadMarker) -> usize;
    /// Every parameter.
    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo>;
    /// A parameter's value, normalized.
    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized>;
    /// Sets a parameter.
    fn set_parameter(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized);
    /// The host's automation mode.
    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode);
    /// What the host says of the plugin's track.
    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo);
    /// The plain value of `value`, by the plugin's format.
    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64>;
    /// The normalized value of a plain one, by the plugin's format.
    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized>;
    /// The plugin's own text for `value`.
    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String>;
    /// The value the plugin parses from `text`.
    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized>;
    /// The plugin's presets.
    fn presets(&mut self, main: MainThreadMarker) -> Vec<Preset>;
    /// Loads a preset; whether the plugin took it.
    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool;
    /// The preset the plugin is on.
    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId>;
    /// The plugin's state, with the writes no block has taken delivered.
    ///
    /// # Errors
    ///
    /// The plugin cannot save, or is lost.
    fn save_state(&mut self, main: MainThreadMarker, state_use: StateUse) -> PluginResult<Vec<u8>>;
    /// The plugin's state as it is, with no Processor brought home: for a
    /// caller that knows no write is waiting for a block (the session's
    /// barrier, `Session::handle_save_state`), so a save never silences one.
    ///
    /// # Errors
    ///
    /// The plugin cannot save, or is lost.
    fn save_state_live(
        &mut self,
        main: MainThreadMarker,
        state_use: StateUse,
    ) -> PluginResult<Vec<u8>>;
    /// Restores a saved state.
    ///
    /// # Errors
    ///
    /// The plugin rejects the bytes, or is lost.
    fn load_state(
        &mut self,
        main: MainThreadMarker,
        data: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()>;
}

/// A plugin of format `F` that the server runs: its [`Lifecycle`], the
/// Processor's seat, the core's state, and what the host has not yet been told.
pub(crate) struct Served<F: OutOfProcess> {
    lifecycle: Lifecycle<F>,
    state: State,
    hand: Hand<F::Processor>,
    descriptor: PluginDescriptor,
    wiring: LoadedPlugin,
    /// What the plugin asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// The parameter edits the plugin made since the last pass, reused.
    edits: Vec<ParamEdit>,
    /// What the host has not yet been told.
    events: Vec<BridgeMessage>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
    /// Set by the plugin's request to be processed again (any thread), taken
    /// by `poll`, which tells the host.
    process_requested: Arc<AtomicBool>,
}

/// How long a plan that waits for the Processor to come home from the render
/// thread is given: the render thread returns it at its next poll (a few
/// milliseconds), so this is a render thread that is stuck inside the plugin.
const HOME_PATIENCE: Duration = Duration::from_secs(3);

/// How often a wait for the Processor looks again.
const HOME_STEP: Duration = Duration::from_micros(500);

/// A Processor that is still on the render thread when the plugin is dropped
/// must not outlive the Controller that destroys the plugin, so the plugin is
/// leaked instead: only a render thread stuck inside the plugin reaches it.
impl<F: OutOfProcess> Drop for Served<F> {
    fn drop(&mut self) {
        if matches!(
            self.lifecycle,
            Lifecycle::Active {
                seat: Seat::Lent,
                ..
            }
        ) {
            tracing::error!(
                "plugin-server: the render thread still holds the processor; leaking the plugin"
            );
            std::mem::forget(std::mem::replace(&mut self.lifecycle, Lifecycle::Poisoned));
        }
    }
}

fn lost() -> PluginError {
    PluginError::State("the plugin is lost".to_string())
}

fn load_failed(stage: LoadStage, reason: String) -> PluginError {
    PluginError::Load { stage, reason }
}

impl<F: ServerSide> Served<F> {
    /// Loads the plugin `load` chooses in the file at `path`, not activated.
    ///
    /// # Errors
    ///
    /// [`PluginError::Load`], with how far the load got.
    pub(crate) fn open(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<F> {
        let mut loaded = F::load_served(main, work, path, load)?;
        loaded.adopt();
        Ok(loaded)
    }

    /// [`open`](Self::open) and [`activate`](Self::activate).
    ///
    /// # Errors
    ///
    /// As those.
    #[allow(clippy::too_many_arguments)] // one load's parameters, as the wire carries them
    pub(crate) fn load(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
        sample_rate: SampleRate,
        block_size: usize,
        prefer_f64: bool,
        state: Option<(&[u8], StateUse)>,
        before_restore: &mut dyn FnMut(),
        hand: Hand<F::Processor>,
    ) -> PluginResult<(Self, Option<String>)> {
        Self::activate(
            main,
            Self::open(main, work, path, load)?,
            sample_rate,
            block_size,
            prefer_f64,
            state,
            before_restore,
            hand,
        )
    }

    /// Activates an opened plugin at `sample_rate` for blocks of up to
    /// `block_size`, restoring `state` **before** the activation: a plugin may
    /// take its mode from the state, and the activation, and the wiring this
    /// instance reports, are then the state's. The second value is the plugin's
    /// refusal of the state, if it refused: the plugin is then activated at its
    /// defaults (a plugin at its defaults beats none).
    ///
    /// With `prefer_f64` the plugin is activated at 64-bit; one that refuses it
    /// is activated at 32-bit. What it runs at is its wiring's
    /// [`Features::F64_AUDIO`], never the request.
    ///
    /// `hand` is the Processor's way to the thread that renders: the server's
    /// is [`Hand::threaded`], whose other end is the render thread's seat
    /// ([`crate::render::End`]); a [`Hand::local`] keeps the Processor in this
    /// value, which then renders through [`Render`].
    ///
    /// # Errors
    ///
    /// [`PluginError::Load`] at [`LoadStage::Activation`] when no activation
    /// took.
    pub(crate) fn activate(
        main: MainThreadMarker,
        loaded: F,
        sample_rate: SampleRate,
        block_size: usize,
        prefer_f64: bool,
        state: Option<(&[u8], StateUse)>,
        before_restore: &mut dyn FnMut(),
        hand: Hand<F::Processor>,
    ) -> PluginResult<(Self, Option<String>)> {
        let activation_failed = |reason: String| {
            load_failed(LoadStage::Activation, format!("activate failed: {reason}"))
        };
        let prepare = MaxBlock::new(Samples(block_size))
            .map(|block| Prepare::new(sample_rate, block))
            .ok_or_else(|| {
                activation_failed(format!(
                    "the plugin cannot run in blocks of {block_size} frames"
                ))
            })?;

        let mut this = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<F>::initial_state(),
            hand,
            descriptor: PluginDescriptor::default(),
            wiring: LoadedPlugin::default(),
            requests: Vec::new(),
            edits: Vec::new(),
            events: Vec::new(),
            refused: false,
            process_requested: Arc::new(AtomicBool::new(false)),
        };
        // The wake outlives every activation: the plugin's host state is the
        // loaded plugin's.
        if let Some(control) = this.lifecycle.control() {
            let requested = Arc::clone(&this.process_requested);
            control.bind_wake(ProcessWake::new(move || {
                requested.store(true, Ordering::Release);
                crate::bell::bell().ring();
            }));
        }
        // Before the activation: see above.
        let state_error = state.and_then(|(data, state_use)| {
            before_restore();
            match this.lifecycle.control() {
                Some(control) => control
                    .load_state(main, data, state_use)
                    .err()
                    .map(|e| e.to_string()),
                None => Some(lost().to_string()),
            }
        });
        let mut refusal = this.drive(
            main,
            Event::Op(HostOp::Activate(if prefer_f64 {
                prepare.with_format(SampleFormat::Float64)
            } else {
                prepare
            })),
        );
        if prefer_f64 && refusal.is_some() && !this.lifecycle.is_poisoned() {
            // A plugin that will not take 64-bit audio runs at 32-bit: the
            // refusal was told to nobody.
            this.refused = false;
            this.events.clear();
            refusal = this.drive(main, Event::Op(HostOp::Activate(prepare)));
        }
        if this.state.stage() != Stage::Active {
            return Err(activation_failed(
                refusal.map_or_else(|| "refused".to_string(), |r| r.to_string()),
            ));
        }
        this.refresh_wiring(main);
        // Whatever the activation told the host is what the load reply carries.
        this.events.clear();
        Ok((this, state_error))
    }

    /// Steps one event through the core, applying its effects. Returns the
    /// reason of a refusal it told, if any.
    fn drive(&mut self, main: MainThreadMarker, event: Event) -> Option<PluginError> {
        let mut told = Vec::new();
        // Taken, not cloned: a clone copies the layout's `Vec`s. The state put
        // in its place is built without allocating and is replaced below.
        let state = std::mem::replace(&mut self.state, Lifecycle::<F>::initial_state());
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

    /// [`drive`](Self::drive), then, while the plan is parked for the Processor
    /// to come home from the render thread, ticks until it has: the render
    /// thread returns it at its next poll. Bounded: a render thread stuck
    /// inside the plugin leaves the plan parked and the refusal says so.
    fn drive_settled(&mut self, main: MainThreadMarker, event: Event) -> Option<PluginError> {
        let mut refusal = self.drive(main, event);
        let giving_up = Instant::now() + HOME_PATIENCE;
        while self.state.is_parked() {
            if Instant::now() >= giving_up {
                return refusal.or_else(|| {
                    Some(PluginError::Other(
                        "the render thread did not hand the processor back".to_string(),
                    ))
                });
            }
            std::thread::sleep(HOME_STEP);
            let ticked = self.drive(main, Event::Tick);
            refusal = refusal.or(ticked);
        }
        refusal
    }

    /// Brings the Processor home from the render thread without deactivating:
    /// `true` once it is held here (also when it never left), `false` when the
    /// render thread did not return it in time.
    fn bring_home(&mut self, main: MainThreadMarker) -> bool {
        if self.lifecycle.processor().is_some() {
            return true;
        }
        if !matches!(
            self.lifecycle,
            Lifecycle::Active {
                seat: Seat::Lent,
                ..
            }
        ) {
            return false;
        }
        self.lifecycle.apply(Effect::Recall, &mut self.hand, main);
        let giving_up = Instant::now() + HOME_PATIENCE;
        while self.lifecycle.processor().is_none() {
            if Instant::now() >= giving_up {
                return false;
            }
            std::thread::sleep(HOME_STEP);
            // A tick collects what the render thread returned.
            self.drive(main, Event::Tick);
        }
        true
    }

    /// Turns a notice into what the host hears, keeping the cached wiring in
    /// step. Returns the reason when the notice is a refusal.
    fn absorb(&mut self, main: MainThreadMarker, notice: Notice) -> Option<PluginError> {
        match notice {
            Notice::LatencyChanged(samples) => {
                self.wiring.latency_samples = samples;
                self.events.push(BridgeMessage::LatencyChanged { samples });
            }
            Notice::TailChanged(tail) => {
                self.wiring.tail = tail;
                self.events.push(BridgeMessage::TailChanged { tail });
            }
            Notice::IoChanged => {
                self.refresh_wiring(main);
                self.events.push(BridgeMessage::PluginIoChanged);
            }
            Notice::ParamValuesChanged => {
                self.events.push(BridgeMessage::PluginParamValuesChanged);
            }
            Notice::ParamTitlesChanged => {
                self.events.push(BridgeMessage::PluginParamTitlesChanged);
            }
            Notice::Reloaded => self.events.push(BridgeMessage::PluginReloaded),
            Notice::Transport(request) => {
                self.events
                    .push(BridgeMessage::TransportRequested { request });
            }
            // The wire carries no note-expression change, and nothing here acts
            // on one: the host reads the list when it opens a session.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("plugin refused {} Hz: {reason}", rate.get());
                // Whether the host must re-read the instance depends on whether
                // a rollback revives it: `finish_refusals` decides.
                self.refresh_wiring(main);
                self.refused = true;
                return Some(reason);
            }
            Notice::Stuck { reason } => {
                tracing::warn!("plugin would not stop: {reason}");
            }
            Notice::Lost { reason } => {
                tracing::error!("plugin lost: {reason}");
                // The wire has no "lost" message, and a new one would be a
                // protocol change. `IoChanged` is the closest signal: it makes
                // the host re-read an instance that now renders silence.
                self.events.push(BridgeMessage::PluginIoChanged);
                return Some(reason);
            }
        }
        None
    }

    /// Tells the host a plugin that stayed inactive after a refusal now renders
    /// silence, so it re-reads it. A refusal a rollback undid tells nothing.
    fn finish_refusals(&mut self) {
        if std::mem::take(&mut self.refused) && !self.is_active() {
            self.events.push(BridgeMessage::PluginIoChanged);
        }
    }

    /// Reads the plugin's wiring again into what the host is given.
    fn refresh_wiring(&mut self, main: MainThreadMarker) {
        if let Some(control) = self.lifecycle.control() {
            self.descriptor = control.info().clone();
            self.wiring = control.wiring(main);
        }
    }

    /// Whether the plugin is active: a refused rate leaves it loaded and
    /// silent.
    fn is_active(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Active { .. })
    }

    /// [`ServedPlugin::poll`]'s pass, leaving what the host should be told in
    /// `events`.
    ///
    /// The plugin's main-thread work runs first (`take_requests` serves what
    /// the format has: CLAP's callbacks, timers and descriptors), since it may
    /// ask for the rest; a later pass takes only what was raised meanwhile
    /// ([`ServerSide::take_raised`]). Then the core decides what the requests need (a
    /// restart, a re-read) and the lifecycle performs it. A pass in which
    /// nothing was asked and the core has nothing pending (the idle pass every
    /// dispatch takes) does not enter the lifecycle and allocates nothing; a
    /// pass with work allocates, as a rate change does.
    fn serve(&mut self, main: MainThreadMarker) {
        // What a pass does may itself make the plugin ask for more (an
        // activation that raises an I/O change), so the word is taken again
        // until a pass finds it empty. Bounded, so a plugin that asks from
        // every callback cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for pass in 0..MAX_PASSES {
            requests.clear();
            if pass == 0 {
                if let Some(control) = self.lifecycle.control() {
                    control.take_requests(main, &mut requests);
                }
            } else {
                F::take_raised(&mut self.lifecycle, main, &mut requests);
            }
            let asked = !requests.is_empty();
            if !asked && self.state.is_quiet() {
                // The idle pass: all a tick would do is retry what a full edit
                // ring refused.
                if let Some(control) = self.lifecycle.control() {
                    control.flush_edits(main);
                }
                break;
            }
            if !asked && pass > 0 {
                break;
            }
            // A restart a request plans needs the Processor home, which the
            // render thread returns within a poll: the plan is waited for here,
            // so what it tells the host (a latency that moved) is told in this
            // pass, in order with the message that caused it.
            for request in requests.drain(..) {
                self.drive_settled(main, Event::Request(request));
            }
            self.drive_settled(main, Event::Tick);
        }
        self.requests = requests;
        self.finish_refusals();
    }

    /// Queues what the plugin edited of its own parameters since the last pass as
    /// [`BridgeMessage::ParameterChanged`], one per [`ParamEdit`], in the order
    /// the plugin made them: a gesture's `Begin`, its changes and its `End`
    /// reach the host in that order, since the wire and the host's inbox keep
    /// the order they were queued in.
    fn forward_edits(&mut self, main: MainThreadMarker) {
        let mut edits = std::mem::take(&mut self.edits);
        edits.clear();
        F::take_edits(&mut self.lifecycle, main, &mut edits);
        self.events.extend(
            edits
                .iter()
                .map(|&edit| BridgeMessage::ParameterChanged { edit }),
        );
        self.edits = edits;
    }

    /// Deactivates and activates at `wanted`, as a rate or mode change does.
    /// A refusal is rolled back: the plugin is activated again at the
    /// configuration it already accepted. Returns the refusal and whether the
    /// rollback took, or `Ok` when `wanted` did.
    fn reconfigure(
        &mut self,
        main: MainThreadMarker,
        previous: Prepare,
        wanted: Prepare,
    ) -> Result<(), (PluginError, bool)> {
        let result = match self.drive_settled(main, Event::Op(HostOp::Reconfigure(wanted))) {
            None => {
                // The plugin may take another layout, or another latency, at
                // another rate or in another mode.
                self.refresh_wiring(main);
                Ok(())
            }
            Some(reason) => {
                // Put back what the plugin accepted once.
                let rolled_back = self
                    .drive(main, Event::Op(HostOp::Activate(previous)))
                    .is_none()
                    && self.is_active();
                if rolled_back {
                    self.refresh_wiring(main);
                }
                Err((reason, rolled_back))
            }
        };
        self.finish_refusals();
        result
    }
}

impl<F: ServerSide> Render for Served<F> {
    fn features(&self) -> Features {
        self.wiring.features
    }

    fn render(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        // A rate change the plugin refused left it deactivated (the host was
        // told when it happened): the block is silence and the plugin is not
        // touched.
        let Some(processor) = self.lifecycle.processor() else {
            out.clear();
            match buffer {
                AudioBufferMut::F32(mut b) => b.clear_outputs(),
                AudioBufferMut::F64(mut b) => b.clear_outputs(),
            }
            return Ok(());
        };
        // The closures are not redundant: the variant's constructor is one
        // function over fixed lifetimes, `through` wants one for any lifetime.
        #[allow(clippy::redundant_closure)]
        match buffer {
            AudioBufferMut::F32(b) => through(processor, b, ctx, out, |b| AudioBufferMut::F32(b)),
            AudioBufferMut::F64(b) => through(processor, b, ctx, out, |b| AudioBufferMut::F64(b)),
        }
    }
}

/// One block through the Processor: the same channels again in tables whose
/// borrow is this call's own, because [`PluginIo`] holds one lifetime for
/// tables and data and a `&mut [&mut _]` is invariant in the data's. An array,
/// not a `Vec`: this is the audio thread.
pub(crate) fn through<P: Processor, T: Sample>(
    processor: &mut P,
    buffer: AudioBuffer<'_, '_, T>,
    ctx: &ProcessContext<'_>,
    out: &mut ProcessOutput,
    wrap: impl for<'a> FnOnce(AudioBuffer<'a, 'a, T>) -> AudioBufferMut<'a, 'a>,
) -> PluginResult<()> {
    let AudioBuffer {
        inputs,
        outputs,
        num_samples,
        sample_rate,
    } = buffer;
    let width = outputs.len().min(MAX_BLOCK_CHANNELS);
    let mut rest = outputs.iter_mut();
    let mut table: [&mut [T]; MAX_BLOCK_CHANNELS] =
        std::array::from_fn(|_| rest.next().map_or(&mut [][..], |o| &mut **o));
    let mut io = PluginIo {
        buffer: wrap(AudioBuffer {
            inputs,
            outputs: &mut table[..width],
            num_samples,
            sample_rate,
        }),
        ctx,
        out,
    };
    processor.process(&mut io)
}

impl<F: ServerSide> ServedPlugin for Served<F> {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.wiring
    }

    fn control_mut(&mut self) -> Option<&mut dyn Control> {
        self.lifecycle.control()
    }

    fn poll(&mut self, main: MainThreadMarker) -> Vec<BridgeMessage> {
        self.serve(main);
        self.forward_edits(main);
        if self.process_requested.swap(false, Ordering::Acquire) {
            self.events.push(BridgeMessage::ProcessRequested);
        }
        std::mem::take(&mut self.events)
    }

    fn quiesce(&mut self, main: MainThreadMarker) -> bool {
        self.drive_settled(main, Event::Op(HostOp::Deactivate));
        !matches!(
            self.lifecycle,
            Lifecycle::Active {
                seat: Seat::Lent,
                ..
            }
        )
    }

    /// The rate the plugin already runs at is a no-op, so a per-frame caller
    /// does not tear it down sixty times a second.
    ///
    /// A refusal is rolled back: the plugin is activated again at the
    /// configuration it already accepted, and the error says it is running
    /// there. (The session still renders nothing after an error, because its
    /// clock follows the rate the host asked for.) When the rollback is refused
    /// too the plugin is left loaded and silent, and the host is told with an
    /// I/O change; the next rate revives it.
    fn set_sample_rate(&mut self, main: MainThreadMarker, rate: SampleRate) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "the plugin has no configuration to change".to_string(),
            ));
        };
        if self.is_active() && (previous.rate().get() - rate.get()).abs() < f64::EPSILON {
            return Ok(());
        }
        let wanted = previous.with_rate(rate);
        self.reconfigure(main, previous, wanted)
            .map_err(|(reason, rolled_back)| {
                PluginError::Other(format!(
                    "plugin refused sample rate {} ({}): {reason}",
                    rate.get(),
                    if rolled_back {
                        format!("running at {}", previous.rate().get())
                    } else {
                        "deactivated".to_string()
                    }
                ))
            })
    }

    /// What happens is the plugin's [`Control::render_mode_support`]: a `Live`
    /// plugin (CLAP's `render.set`, VST2's callback) is told; an
    /// `OnActivation` one (VST3's `setupProcessing`, AU's
    /// `AudioUnitInitialize`) is activated again in the mode, keeping its
    /// instance and state, and a refusal is rolled back to the mode it was
    /// running in; an `Unsupported` one is left running and answers `false`.
    /// The mode is kept for every later activation (a restart, a rate change)
    /// either way.
    fn set_render_mode(&mut self, main: MainThreadMarker, mode: RenderMode) -> bool {
        let Some(previous) = self.state.prepare() else {
            return false;
        };
        if previous.mode() == mode && self.is_active() {
            return true;
        }
        let Some(control) = self.lifecycle.control() else {
            return false;
        };
        match control.render_mode_support(main) {
            RenderModeSupport::Live => {
                let took = control.set_render_mode(main, mode);
                if took {
                    self.state.set_mode(mode);
                }
                took
            }
            RenderModeSupport::Unsupported => {
                self.state.set_mode(mode);
                false
            }
            RenderModeSupport::OnActivation => self
                .reconfigure(main, previous, previous.with_mode(mode))
                .is_ok(),
        }
    }

    fn parameter_count(&self, main: MainThreadMarker) -> usize {
        self.lifecycle
            .control_ref()
            .map_or(0, |control| control.parameter_count(main))
    }

    fn parameters(&self, main: MainThreadMarker) -> Vec<ParameterInfo> {
        self.lifecycle
            .control_ref()
            .map(|control| control.parameters(main))
            .unwrap_or_default()
    }

    fn parameter(&self, main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
        self.lifecycle.control_ref()?.parameter(main, id)
    }

    /// Normalized `0..=1`. On an active plugin the write is queued to the
    /// Processor and reaches the plugin at the head of the next block.
    fn set_parameter(&mut self, main: MainThreadMarker, id: ParamId, value: Normalized) {
        if let Some(control) = self.lifecycle.control() {
            control.set_parameter(main, id, value);
        }
    }

    fn set_automation_state(&mut self, main: MainThreadMarker, mode: AutomationMode) {
        if let Some(control) = self.lifecycle.control() {
            control.set_automation_state(main, mode);
        }
    }

    /// The wire has no reply to this, so what the plugin's `Control` answers
    /// is not sent back; the host's own queue already said whether it went.
    fn set_track_info(&mut self, main: MainThreadMarker, info: &TrackInfo) {
        if let Some(control) = self.lifecycle.control() {
            let _ = control.set_track_info(main, info);
        }
    }

    fn parameter_plain(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64> {
        self.lifecycle
            .control_ref()?
            .parameter_plain(main, id, value)
    }

    fn parameter_normalized(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        self.lifecycle
            .control_ref()?
            .parameter_normalized(main, id, plain)
    }

    fn parameter_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        self.lifecycle
            .control_ref()?
            .parameter_text(main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.lifecycle
            .control_ref()?
            .parameter_value_from_text(main, id, text)
    }

    fn presets(&mut self, main: MainThreadMarker) -> Vec<Preset> {
        self.lifecycle
            .control()
            .map(|control| control.presets(main))
            .unwrap_or_default()
    }

    fn load_preset(&mut self, main: MainThreadMarker, id: &PresetId) -> bool {
        self.lifecycle
            .control()
            .is_some_and(|control| control.load_preset(main, id))
    }

    fn current_preset(&mut self, main: MainThreadMarker) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset(main)
    }

    fn save_state(&mut self, main: MainThreadMarker, state_use: StateUse) -> PluginResult<Vec<u8>> {
        // A write waits in the Processor's ring until a block drains it, so a
        // save right after one would read the old value. Only the Processor
        // can deliver it, so it is brought home for the settle (the render
        // thread hands it back at its next poll; a block meanwhile is silence)
        // and lent again after the save. The session asks for this only when
        // no block is draining the writes.
        let home = self.bring_home(main);
        if home {
            F::settle_edits(&mut self.lifecycle, main);
        }
        let saved = self
            .lifecycle
            .control()
            .ok_or_else(lost)?
            .save_state(main, state_use);
        if home {
            // The tick lends the Processor again.
            self.drive(main, Event::Tick);
        }
        saved
    }

    fn save_state_live(
        &mut self,
        main: MainThreadMarker,
        state_use: StateUse,
    ) -> PluginResult<Vec<u8>> {
        self.lifecycle
            .control()
            .ok_or_else(lost)?
            .save_state(main, state_use)
    }

    fn load_state(
        &mut self,
        main: MainThreadMarker,
        data: &[u8],
        state_use: StateUse,
    ) -> PluginResult<()> {
        self.lifecycle
            .control()
            .ok_or_else(lost)?
            .load_state(main, data, state_use)
    }
}
