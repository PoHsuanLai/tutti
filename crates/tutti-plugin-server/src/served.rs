//! [`Served<F>`]: one plugin of any [`OutOfProcess`] format, in this process.
//!
//! The host crate of each format is the interpreter of its plugin's lifecycle
//! (activation, restarts, requests). This is the server's one shell over them:
//! it loads through [`OutOfProcess::load_served`], activates through
//! [`Loaded::activate`] into the realtime [`Processor`] and the main-thread
//! [`Controller`](tutti_plugin_types::Controller), and runs both through
//! [`Lifecycle`], so a format is a type argument and a row of the
//! [`served!`](crate::served!) table, not a loader.
//!
//! What the formats still differ in, that only this process does, is
//! [`ServerSide`]'s.
//!
//! What the plugin tells the host between blocks is [`Served::poll`]'s answer:
//! each [`Notice`] of the core as the [`BridgeMessage`] that carries it.

use std::path::Path;

use tutti_plugin_ipc::BridgeMessage;
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, AutomationMode, Event, Features, Hand, HostOp, Lifecycle, Load,
    LoadStage, LoadedPlugin, MAX_BLOCK_CHANNELS, MainThread, MaxBlock, Normalized, Notice,
    OutOfProcess, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginIo, PluginResult,
    Prepare, Preset, PresetId, ProcessContext, ProcessOutput, Processor, RenderMode, Request,
    Sample, SampleFormat, SampleRate, Samples, Stage, State, standing,
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
/// [`served!`](crate::served!) table's enum lends it as a trait object.
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
    /// Acts on what the plugin asked of the host since the last call, and
    /// returns what the host should be told. Between blocks, on the thread
    /// that loaded the plugin.
    fn poll(&mut self, main: &MainThread) -> Vec<BridgeMessage>;
    /// Forgets the plugin's signal history, between blocks.
    fn reset(&mut self);
    /// Follows the host's rate.
    ///
    /// # Errors
    ///
    /// The plugin is not running at `rate`: it keeps the rate it already
    /// accepted, or renders silence until a rate it takes.
    fn set_sample_rate(&mut self, main: &MainThread, rate: f64) -> PluginResult<()>;
    /// Tells the plugin its render mode; whether it now runs in it.
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool;
    /// How many parameters the plugin has.
    fn parameter_count(&self, main: &MainThread) -> usize;
    /// Every parameter.
    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo>;
    /// A parameter's value, normalized.
    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized>;
    /// Sets a parameter.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized);
    /// The host's automation mode.
    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode);
    /// The plugin's own text for `value`.
    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String>;
    /// The value the plugin parses from `text`.
    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized>;
    /// The plugin's presets.
    fn presets(&mut self, main: &MainThread) -> Vec<Preset>;
    /// Loads a preset; whether the plugin took it.
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool;
    /// The preset the plugin is on.
    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId>;
    /// The plugin's state, with the writes no block has taken delivered.
    ///
    /// # Errors
    ///
    /// The plugin cannot save, or is lost.
    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>>;
    /// Restores a saved state.
    ///
    /// # Errors
    ///
    /// The plugin rejects the bytes, or is lost.
    fn load_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()>;
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
    /// What the host has not yet been told.
    events: Vec<BridgeMessage>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
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
    pub(crate) fn open(main: &MainThread, path: &Path, load: &Load) -> PluginResult<F> {
        let mut loaded = F::load_served(main, path, load)?;
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
        main: &MainThread,
        path: &Path,
        load: &Load,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> PluginResult<(Self, Option<String>)> {
        Self::activate(
            main,
            Self::open(main, path, load)?,
            sample_rate,
            block_size,
            prefer_f64,
            state,
            before_restore,
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
    /// # Errors
    ///
    /// [`PluginError::Load`] at [`LoadStage::Activation`] when no activation
    /// took.
    pub(crate) fn activate(
        main: &MainThread,
        loaded: F,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> PluginResult<(Self, Option<String>)> {
        let activation_failed = |reason: String| {
            load_failed(LoadStage::Activation, format!("activate failed: {reason}"))
        };
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| {
                activation_failed(format!(
                    "the plugin cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                ))
            })?;

        let mut this = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<F>::initial_state(),
            hand: Hand::local(),
            descriptor: PluginDescriptor::default(),
            wiring: LoadedPlugin::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        // Before the activation: see above.
        let state_error = state.and_then(|data| {
            before_restore();
            match this.lifecycle.control() {
                Some(control) => control.load_state(main, data).err().map(|e| e.to_string()),
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
    fn drive(&mut self, main: &MainThread, event: Event) -> Option<PluginError> {
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

    /// Turns a notice into what the host hears, keeping the cached wiring in
    /// step. Returns the reason when the notice is a refusal.
    fn absorb(&mut self, main: &MainThread, notice: Notice) -> Option<PluginError> {
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
            // The wire carries no note-expression change, and nothing here acts
            // on one: the host reads the list when it opens a session.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("plugin refused {} Hz: {reason}", rate.0);
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
    fn refresh_wiring(&mut self, main: &MainThread) {
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
    fn serve(&mut self, main: &MainThread) {
        // What a pass does may itself make the plugin ask for more (an
        // activation that raises an I/O change), so the word is taken again
        // until a pass finds it empty. Bounded, so a plugin that asks from
        // every callback cannot hold the thread.
        const MAX_PASSES: usize = 8;
        let mut requests = std::mem::take(&mut self.requests);
        for pass in 0..MAX_PASSES {
            requests.clear();
            F::before_requests(&self.lifecycle);
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
            for request in requests.drain(..) {
                self.drive(main, Event::Request(request));
            }
            self.drive(main, Event::Tick);
        }
        self.requests = requests;
        self.finish_refusals();
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
    ) -> Result<(), (PluginError, bool)> {
        let result = match self.drive(main, Event::Op(HostOp::Reconfigure(wanted))) {
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
fn through<P: Processor, T: Sample>(
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

    fn poll(&mut self, main: &MainThread) -> Vec<BridgeMessage> {
        self.serve(main);
        std::mem::take(&mut self.events)
    }

    fn reset(&mut self) {
        // No processor (a refused activation) has nothing to forget.
        if let Some(processor) = self.lifecycle.processor() {
            processor.reset();
        }
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
    fn set_sample_rate(&mut self, main: &MainThread, rate: f64) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "the plugin has no configuration to change".to_string(),
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
                    "plugin refused sample rate {rate} ({}): {reason}",
                    if rolled_back {
                        format!("running at {}", previous.rate().0)
                    } else {
                        "deactivated".to_string()
                    }
                ))
            })
    }

    /// A format that takes the mode live (CLAP's `render.set`, VST2's callback)
    /// is told through [`Control::set_render_mode`]; one that reads it only at
    /// activation (VST3's `setupProcessing`, AU's `AudioUnitInitialize`) is
    /// activated again in the mode, keeping its instance and state. A refusal
    /// is rolled back to the mode the plugin was running in, and answers
    /// `false`.
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        let Some(previous) = self.state.prepare() else {
            return false;
        };
        if previous.mode() == mode && self.is_active() {
            return true;
        }
        let live = self
            .lifecycle
            .control()
            .is_some_and(|control| control.set_render_mode(main, mode));
        if live {
            // The mode is a field of the `Prepare` every later activation (a
            // restart, a rate change) applies, so the core holds the new one.
            self.state.set_mode(mode);
            return true;
        }
        self.reconfigure(main, previous, previous.with_mode(mode))
            .is_ok()
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        self.lifecycle
            .control_ref()
            .map_or(0, |control| control.parameter_count(main))
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.lifecycle
            .control_ref()
            .map(|control| control.parameters(main))
            .unwrap_or_default()
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        self.lifecycle.control_ref()?.parameter(main, id)
    }

    /// Normalized `0..=1`. On an active plugin the write is queued to the
    /// Processor and reaches the plugin at the head of the next block.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        if let Some(control) = self.lifecycle.control() {
            control.set_parameter(main, id, value);
        }
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        if let Some(control) = self.lifecycle.control() {
            control.set_automation_state(main, mode);
        }
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        self.lifecycle
            .control_ref()?
            .parameter_text(main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.lifecycle
            .control_ref()?
            .parameter_value_from_text(main, id, text)
    }

    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        self.lifecycle
            .control()
            .map(|control| control.presets(main))
            .unwrap_or_default()
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        self.lifecycle
            .control()
            .is_some_and(|control| control.load_preset(main, id))
    }

    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset(main)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        // The server serves requests on the thread that renders, so a save
        // right after a write would read the old value until the next block
        // without this.
        F::settle_edits(&mut self.lifecycle, main);
        self.lifecycle.control().ok_or_else(lost)?.save_state(main)
    }

    fn load_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()> {
        self.lifecycle
            .control()
            .ok_or_else(lost)?
            .load_state(main, data)
    }
}
