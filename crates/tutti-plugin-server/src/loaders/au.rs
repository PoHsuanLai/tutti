//! Audio Unit loader: [`AuInstance`] drives a `Lifecycle<AuLoaded>` and
//! presents it through the capability traits the server still dispatches on.
//!
//! The host crate (`tutti-au-host`) is the interpreter of the unit's lifecycle
//! (activation, parameters, requests); this file is the translation between the
//! core's notices and the server's [`AsyncEvent`]s. It is the same shape as the
//! CLAP loader's, and dies with it (design 021 step 5).

use std::path::Path;

use tutti_au_host::{AuLoaded, AuProcessor};
use tutti_plugin::LoadStage;
use tutti_plugin::server::{
    LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginAudio, PluginDescriptor, PluginError,
    PluginMeta, PluginParams, PluginPresets, PluginResult, PluginState, Preset, PresetId,
    ProcessContext, ProcessOutput, RenderMode,
};
use tutti_plugin_types::MainThread;
use tutti_plugin_types::{BridgeError, BridgeResult};
use tutti_plugin_types::{
    Control, Event, Hand, HostOp, Lifecycle, Load, MaxBlock, Notice, Prepare, Request, SampleRate,
    Samples, Stage, State, standing,
};

use crate::loaders::common::Meta;
use crate::plugin::AsyncEvent;

/// An Audio Unit as the server sees it.
pub struct AuInstance {
    lifecycle: Lifecycle<AuLoaded>,
    state: State,
    hand: Hand<AuProcessor>,
    meta: Meta,
    /// What the unit asked of the host since the last pass, reused.
    requests: Vec<Request>,
    /// What the host has not yet been told.
    events: Vec<AsyncEvent>,
    /// A refusal was told since the last [`finish_refusals`](Self::finish_refusals).
    refused: bool,
}

impl AuInstance {
    /// Loads and initializes an Audio Unit from a `.component` bundle path.
    ///
    /// AU plugins are registered system-wide; the path is used for identification
    /// but the actual loading goes through the component registry.
    #[cfg(test)]
    pub fn load(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
    ) -> BridgeResult<Self> {
        Self::load_with_state(main, path, sample_rate, block_size, None, &mut || {})
            .map(|(instance, _)| instance)
    }

    /// [`load`](Self::load), restoring `state` **before the unit is
    /// initialized** (`AudioUnitInitialize`), when the class info may be set. The
    /// second value is the unit's refusal of the state, if it refused: the unit
    /// is then initialized at its defaults.
    pub fn load_with_state(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        state: Option<&[u8]>,
        before_restore: &mut dyn FnMut(),
    ) -> BridgeResult<(Self, Option<String>)> {
        let loaded = AuLoaded::load(main, path, &Load::new()).map_err(|e| match e {
            PluginError::Load { stage, reason } => BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage,
                reason,
            },
            other => BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: other.to_string(),
            },
        })?;
        let initialization_failed = |reason: String| BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Initialization,
            reason,
        };
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| {
                initialization_failed(format!(
                    "the unit cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                ))
            })?;

        let mut instance = Self {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<AuLoaded>::initial_state(),
            hand: Hand::local(),
            meta: Meta::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        // Before the unit is initialized, so the initialization is the state's.
        let state_error = state.and_then(|data| {
            before_restore();
            match instance.lifecycle.control() {
                Some(control) => control.load_state(main, data).err().map(|e| {
                    let reason = match e {
                        PluginError::State(reason) => reason,
                        other => other.to_string(),
                    };
                    BridgeError::StateRestoreError(reason).to_string()
                }),
                None => Some("the plugin is lost".to_string()),
            }
        });
        let refused = instance.drive(main, Event::Op(HostOp::Activate(prepare)));
        if instance.state.stage() != Stage::Active {
            let reason = refused.map_or_else(|| "refused".to_string(), |r| r.to_string());
            return Err(initialization_failed(reason));
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
        // Taken, not cloned: the state put in its place is built without
        // allocating and is replaced below.
        let state = std::mem::replace(&mut self.state, Lifecycle::<AuLoaded>::initial_state());
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
            // `kAudioUnitProperty_ParameterList` reports which parameters
            // *exist*, so the client re-pulls the list rather than re-reading
            // values against ids that may be gone. A value change on an
            // unchanged list arrives on the parameter listener path instead.
            Notice::ParamTitlesChanged => self.events.push(AsyncEvent::ParamTitlesChanged),
            Notice::Reloaded => self.events.push(AsyncEvent::Reloaded),
            // AU has no note-expression restart flag.
            Notice::NoteExpressionChanged => {}
            Notice::Refused { rate, reason } => {
                tracing::warn!("AU refused {} Hz: {reason}", rate.0);
                self.refresh_meta(main);
                self.refused = true;
                return Some(reason);
            }
            Notice::Stuck { reason } => {
                tracing::warn!("AU would not stop: {reason}");
            }
            Notice::Lost { reason } => {
                tracing::error!("AU lost: {reason}");
                // The wire has no "lost" message, and a new one would be a
                // protocol change. `IoChanged` is the closest signal: it makes
                // the host re-read an instance that now renders silence.
                self.events.push(AsyncEvent::IoChanged);
                return Some(reason);
            }
        }
        None
    }

    /// Tells the host a unit that stayed inactive after a refusal now renders
    /// silence, so it re-reads it. A refusal a rollback undid tells nothing.
    fn finish_refusals(&mut self) {
        if std::mem::take(&mut self.refused) && !self.is_active() {
            self.events.push(AsyncEvent::IoChanged);
        }
    }

    /// Reads the unit's wiring again into the metadata the host is given.
    fn refresh_meta(&mut self, main: &MainThread) {
        if let Some(control) = self.lifecycle.control() {
            self.meta.descriptor = control.info().clone();
            self.meta.loaded = control.wiring(main);
        }
    }

    /// Acts on what the unit asked of the host since the last call (the
    /// property listener's latency, tail and parameter-list changes), and
    /// returns what the host should be told. Call it between blocks, on the
    /// thread that loaded the unit.
    pub fn poll(&mut self, main: &MainThread) -> Vec<AsyncEvent> {
        self.serve(main);
        std::mem::take(&mut self.events)
    }

    /// [`poll`](Self::poll)'s pass, leaving what the host should be told in
    /// `events`.
    fn serve(&mut self, main: &MainThread) {
        let mut requests = std::mem::take(&mut self.requests);
        requests.clear();
        if let Some(loaded) = self.loaded_mut() {
            loaded.take_requests(main, &mut requests);
        }
        if requests.is_empty() && self.state.is_quiet() {
            // The idle pass: nothing was asked and the core has nothing pending,
            // so it does not enter the lifecycle and allocates nothing.
            self.requests = requests;
            return;
        }
        for request in requests.drain(..) {
            self.drive(main, Event::Request(request));
        }
        self.drive(main, Event::Tick);
        self.requests = requests;
        self.finish_refusals();
    }

    /// The unit, whichever half of an activation holds it.
    fn loaded_ref(&self) -> Option<&AuLoaded> {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller),
            Lifecycle::Poisoned => None,
        }
    }

    /// [`loaded_ref`](Self::loaded_ref), exclusively.
    fn loaded_mut(&mut self) -> Option<&mut AuLoaded> {
        match &mut self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller),
            Lifecycle::Poisoned => None,
        }
    }

    /// Whether the unit is active: a refused rate leaves it loaded and silent.
    fn is_active(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Active { .. })
    }
}

#[cfg(test)]
impl AuInstance {
    /// The word the unit's property listener raises, for a test that needs to
    /// say a change no installed unit will make on request.
    pub(crate) fn requests_for_test(&self) -> std::sync::Arc<tutti_plugin_types::RequestWord> {
        std::sync::Arc::clone(self.loaded_ref().expect("the unit is loaded").requests())
    }

    /// Overwrite the cached latency, so a test can prove a poll replaces it
    /// rather than leaves it.
    ///
    /// Without this the assertion "`loaded()` and the event agree" is satisfied
    /// by *any* AU whose latency does not change during the test, which is
    /// every AU: both values would read the load-time figure and match, refresh
    /// or no refresh. Poisoning the cache first is what makes the two figures
    /// differ when the refresh is missing.
    pub(crate) fn poison_cached_latency(&mut self, samples: Samples) {
        self.meta.loaded.latency_samples = samples;
    }
}

impl PluginMeta for AuInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for AuInstance {
    fn reset(&mut self) -> PluginResult<()> {
        // No processor (a refused activation) has nothing to forget.
        match self.lifecycle.processor() {
            Some(processor) => processor
                .flush()
                .map_err(|e| PluginError::Process(format!("AudioUnitReset refused: {e}"))),
            None => Ok(()),
        }
    }

    fn process(
        &mut self,
        mut buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        use tutti_plugin::server::AudioBufferMut;
        // A rate change the unit refused left it deactivated (the host raised
        // `IoChanged` when it happened). AU renders only while initialized, so
        // the block is silence and the unit is not touched.
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

    /// Writes `kAudioUnitProperty_OfflineRender`. The property is only honoured
    /// at `AudioUnitInitialize` (a unit that sizes an oversampling or look-ahead
    /// buffer from it can only do so there), so an active unit is reconfigured
    /// with the new mode in its `Prepare`, which the activation writes.
    ///
    /// Returns whether *this unit* accepted the property, the live probe behind
    /// `Features::RENDER_MODE`. A refused re-initialization also reports `false`:
    /// the caller asked for a mode and did not get it.
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        if self.is_active() {
            let Some(prepare) = self.state.prepare() else {
                return false;
            };
            self.drive(
                main,
                Event::Op(HostOp::Reconfigure(prepare.with_mode(mode))),
            );
            self.refresh_meta(main);
            self.finish_refusals();
            return self.is_active() && self.loaded_ref().is_some_and(|l| l.render_mode_accepted());
        }
        let accepted = self
            .lifecycle
            .control()
            .is_some_and(|control| control.set_render_mode(main, mode));
        self.state.set_mode(mode);
        accepted
    }

    /// The rate the unit already runs at is a no-op, so a per-frame caller does
    /// not tear it down sixty times a second.
    ///
    /// A refusal is rolled back: the unit is activated again at the
    /// configuration it already accepted, and the error says it is running
    /// there. When the rollback is refused too the unit is left loaded and
    /// silent, and the host is told through [`AsyncEvent::IoChanged`]; the next
    /// rate revives it. The unit may also change its bus layout across the
    /// re-initialization; the metadata follows it and the host is told the same
    /// way.
    fn set_sample_rate(&mut self, main: &MainThread, rate: f64) -> PluginResult<()> {
        let Some(previous) = self.state.prepare() else {
            return Err(PluginError::Other(
                "AU has no configuration to change".to_string(),
            ));
        };
        if self.is_active() && (previous.rate().0 - rate).abs() < f64::EPSILON {
            return Ok(());
        }
        let Some(wanted) = previous.with_rate(SampleRate(rate)) else {
            return Err(PluginError::Other(format!(
                "AU refused sample rate {rate} ({}): not a positive finite rate",
                standing(self.is_active(), previous.rate())
            )));
        };
        let result = match self.drive(main, Event::Op(HostOp::Reconfigure(wanted))) {
            None => Ok(()),
            Some(reason) => {
                // Put back what the unit accepted once.
                let rolled_back = self
                    .drive(main, Event::Op(HostOp::Activate(previous)))
                    .is_none()
                    && self.is_active();
                if rolled_back {
                    self.refresh_meta(main);
                }
                Err(PluginError::Other(format!(
                    "AU refused sample rate {rate} ({}): {reason}",
                    if rolled_back {
                        format!("running at {}", previous.rate().0)
                    } else {
                        "deactivated".to_string()
                    }
                )))
            }
        };
        if result.is_ok() {
            // The unit may take another layout at another rate.
            self.refresh_meta(main);
        }
        self.finish_refusals();
        result
    }
}

impl PluginParams for AuInstance {
    /// Normalized `0..=1`, per the [`PluginParams`] contract: `AudioUnitGet`
    /// answers in plain units, so the declared range maps it back.
    fn get_parameter(&self, main: &MainThread, id: ParamId) -> f64 {
        self.loaded_ref()
            .and_then(|loaded| Control::parameter(loaded, main, id))
            .map_or(0.0, Normalized::get)
    }

    /// Normalized `0..=1` in, matching [`get_parameter`](Self::get_parameter), so
    /// the pair round-trips.
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
}

impl PluginState for AuInstance {
    fn get_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
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

impl PluginPresets for AuInstance {
    fn get_presets(&mut self, main: &MainThread) -> Vec<Preset> {
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

    fn get_current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        self.lifecycle.control()?.current_preset(main)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::main_thread;
    use tutti_plugin::server::{
        AudioBuffer as TuttiAudioBuffer, AudioBufferMut, Features, ParamFlags, ParameterChanges,
        ParameterQueue, TimeSignature, TransportInfo,
    };

    const RATE: f64 = 44_100.0;

    fn delay() -> AuInstance {
        AuInstance::load(&main_thread(), Path::new("AUDelay"), RATE, 512).expect("AUDelay loads")
    }

    /// One stereo block of `frames` through `au`, with the given context pieces.
    fn run(
        au: &mut AuInstance,
        frames: usize,
        transport: Option<&TransportInfo>,
        changes: Option<&ParameterChanges>,
    ) -> PluginResult<()> {
        let input = vec![vec![0.0f32; frames]; 2];
        let mut output = vec![vec![0.0f32; frames]; 2];
        let inputs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = TuttiAudioBuffer {
            inputs: &inputs,
            outputs: &mut outputs,
            num_samples: frames,
            sample_rate: RATE,
        };
        let mut ctx = ProcessContext::new();
        ctx.transport = transport;
        ctx.param_changes = changes;
        let mut out = ProcessOutput::default();
        au.process(AudioBufferMut::F32(buffer), &ctx, &mut out)
    }

    #[test]
    fn test_au_enumerate_and_load() {
        use tutti_au_host::component;

        let effects =
            component::enumerate_components_of_type(tutti_au_host::component::AuType::Effect);
        assert!(
            !effects.is_empty(),
            "Should find at least one AU effect on macOS"
        );

        eprintln!("Found {} AU effects", effects.len());
        for (i, info) in effects.iter().take(5).enumerate() {
            eprintln!("  [{}] {} ({})", i, info.name, info.manufacturer);
        }
    }

    #[test]
    fn test_au_parameter_list_via_trait() {
        let au = delay();
        let params = au.get_parameter_list(&main_thread());
        assert!(
            !params.is_empty(),
            "AUDelay should have parameters via PluginInstance trait"
        );
    }

    /// The loader's metadata is the unit's: an effect's buses are the unit's own
    /// and its editor is measured, its transport callbacks are offered (the
    /// `TRANSPORT` bit), and it receives no MIDI.
    #[test]
    fn a_loaded_unit_reports_its_wiring_and_capabilities() {
        let au = delay();
        let loaded = au.loaded();
        assert_eq!(loaded.inputs.len(), 1, "AUDelay has one input bus");
        assert_eq!(loaded.outputs.len(), 1, "and one output bus");
        assert!(
            loaded.features.contains(Features::TRANSPORT),
            "the loader must offer transport to every unit that accepts it"
        );
        assert!(
            !loaded.features.contains(Features::MIDI_IN),
            "an effect takes no MIDI"
        );
        assert!(
            au.descriptor().id.starts_with("au."),
            "{}",
            au.descriptor().id
        );
    }

    #[test]
    fn test_au_process_via_trait() {
        let mut au = delay();
        // No crash is the assertion for the render itself; the block must succeed.
        run(&mut au, 512, None, None).expect("a block renders");
    }

    /// The loader offers `kAudioUnitProperty_HostCallbacks` at load and publishes
    /// each block's transport into it, so a tempo-synced AU that pulls through
    /// the procs sees the block's tempo, beat, signature, distance to the next
    /// beat and a locate flag that fires once per start/seek.
    ///
    /// No Apple AU calls the procs (see `tutti-au-host`'s `au_transport` suite),
    /// so the pull is made the way AudioToolbox makes it, through the procs the
    /// loader installed. What this pins is the loader's wiring; that an AU
    /// *behaves* on the values needs a tempo-synced unit on macOS hardware and
    /// is not asserted here.
    #[test]
    fn the_loader_installs_host_callbacks_and_feeds_each_blocks_transport() {
        // kAudioUnitErr_CannotDoInCurrentContext.
        const CANNOT_DO: i32 = -10863;
        const FRAMES: usize = 512;

        let mut au = delay();

        // What the AU's procs answer now: (beat_and_tempo status, beat, tempo,
        // delta, numerator, denominator, v1 changed, v1 playing).
        let pull = |au: &AuInstance| {
            let state = au
                .loaded_ref()
                .and_then(|l| l.transport())
                .expect("callbacks installed at load");
            let p = state.test_pull();
            (
                p.status,
                p.beat,
                p.tempo,
                p.delta,
                p.numerator,
                p.denominator,
                p.changed,
                p.playing,
            )
        };

        // Nothing published: the host cannot say, so no fabricated 120 BPM.
        assert_eq!(pull(&au).0, CANNOT_DO, "before any block");
        run(&mut au, FRAMES, None, None).expect("process");
        assert_eq!(
            pull(&au).0,
            CANNOT_DO,
            "a block without transport publishes none"
        );

        // Playing, 137.5 BPM, 3/4, a quarter of the way past beat 2.
        let beat0 = 2.25;
        let start = TransportInfo::new()
            .with_sample_rate(RATE)
            .with_tempo(137.5)
            .with_time_signature(TimeSignature::of(3, 4))
            .with_playing(true)
            .with_position_beats(beat0, tutti_plugin_types::Seconds(0.0));
        run(&mut au, FRAMES, Some(&start), None).expect("process");
        let (status, beat, tempo, delta, num, den, changed, playing) = pull(&au);
        assert_eq!(status, 0);
        assert_eq!((beat, tempo), (beat0, 137.5));
        // 0.75 beat left at 60 / 137.5 s per beat: 14 432.7 frames at 44.1 kHz.
        assert_eq!(delta, 14_433, "frames to the next whole beat");
        assert_eq!((num, den), (3.0, 4));
        assert!(playing);
        assert!(changed, "the first block is a start");

        // The next block is where playing leaves the playhead: no locate.
        let next = start.with_position_beats(
            beat0 + FRAMES as f64 / RATE * 137.5 / 60.0,
            tutti_plugin_types::Seconds(0.0),
        );
        run(&mut au, FRAMES, Some(&next), None).expect("process");
        assert!(!pull(&au).6, "a continuous block is not a locate");

        // A seek is one.
        let seek = next.with_position_beats(32.0, tutti_plugin_types::Seconds(0.0));
        run(&mut au, FRAMES, Some(&seek), None).expect("process");
        let (_, beat, _, delta, _, _, changed, _) = pull(&au);
        assert_eq!(beat, 32.0);
        assert_eq!(delta, 0, "on a beat");
        assert!(changed, "a seek is reported once");

        // Stopped is published too, not skipped.
        run(&mut au, FRAMES, Some(&seek.with_playing(false)), None).expect("process");
        let (status, _, _, _, _, _, changed, playing) = pull(&au);
        assert_eq!(status, 0);
        assert!(!playing && changed, "a stop is a change");

        // A block with no transport takes it back, and the next one that has
        // one is a start again even where playing would have put the playhead.
        run(&mut au, FRAMES, Some(&next), None).expect("process");
        run(&mut au, FRAMES, None, None).expect("process");
        assert_eq!(pull(&au).0, CANNOT_DO, "no frozen beat after a gap");
        let resumed = next.with_position_beats(
            next.position.beats + 0.026,
            tutti_plugin_types::Seconds(0.0),
        );
        run(&mut au, FRAMES, Some(&resumed), None).expect("process");
        let (status, _, _, _, _, _, changed, _) = pull(&au);
        assert_eq!(status, 0);
        assert!(changed, "the block after a gap starts afresh");
    }

    /// A unit whose host callbacks were never installed (a unit that refused the
    /// property is in the same state: the host keeps no transport for it) has no
    /// `TRANSPORT` bit and is not published to: the Processor holds no state to
    /// write into, so a block with a transport renders and publishes nothing.
    ///
    /// The refusal itself is `tutti-au-host`'s
    /// `a_refused_host_callbacks_install_is_an_error`, against the probe unit
    /// that refuses it.
    #[test]
    fn a_unit_without_the_callbacks_is_not_published_to() {
        let loaded = AuLoaded::new(
            tutti_au_host::component::enumerate_components()
                .into_iter()
                .find(|c| c.name.contains("AUDelay"))
                .expect("AUDelay ships with macOS")
                .component,
            RATE,
            512,
        )
        .expect("AUDelay instantiates");
        assert!(loaded.transport().is_none(), "nothing installed it");
        let control: &dyn Control = &loaded;
        assert!(
            !control
                .wiring(&main_thread())
                .features
                .contains(Features::TRANSPORT),
            "no callbacks, no TRANSPORT bit"
        );

        let mut au = AuInstance {
            lifecycle: Lifecycle::new(loaded),
            state: Lifecycle::<AuLoaded>::initial_state(),
            hand: Hand::local(),
            meta: Meta::default(),
            requests: Vec::new(),
            events: Vec::new(),
            refused: false,
        };
        let prepare = Prepare::new(SampleRate(RATE), MaxBlock::of(Samples(512))).unwrap();
        assert!(
            au.drive(&main_thread(), Event::Op(HostOp::Activate(prepare)))
                .is_none()
        );
        let transport = TransportInfo::new().with_tempo(150.0).with_playing(true);
        run(&mut au, 512, Some(&transport), None).expect("a block with a transport renders");
        assert!(au.loaded_ref().and_then(|l| l.transport()).is_none());
    }

    /// The live half of the direct path: `PluginParams` is normalized for every
    /// format, so a write of `1.0` must reach AUDelay's cutoff as 22050 Hz and
    /// read back as `1.0`, not as the 1 Hz that writing the normalized value
    /// straight through would set.
    #[test]
    fn direct_parameter_path_is_normalized_end_to_end() {
        let mut au = delay();
        let main = main_thread();

        // The cutoff parameter is the one whose range makes the bug audible.
        let cutoff = au
            .get_parameter_list(&main)
            .into_iter()
            .find(|p| {
                p.range
                    .bounds()
                    .is_some_and(|(min, max)| min >= 10.0 && max >= 20_000.0)
            })
            .expect("AUDelay declares a wide-range cutoff");
        let (min, max) = cutoff.range.bounds().expect("a plain range");
        let addr = cutoff.id;

        au.set_parameter(&main, addr, Normalized::of(1.0));
        let plain = tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), addr.get())
            .expect("cutoff is readable");
        assert!(
            (f64::from(plain) - max).abs() < 1.0,
            "normalized 1.0 must set the top of {:?}, got {plain}",
            (min, max)
        );
        assert!(
            plain > 2.0,
            "a plain {plain} means the normalized value went through unscaled: \
             the inaudible-filter bug"
        );
        assert!((au.get_parameter(&main, addr) - 1.0).abs() < 1e-3);

        au.set_parameter(&main, addr, Normalized::of(0.0));
        assert!((au.get_parameter(&main, addr)).abs() < 1e-3);
    }

    #[test]
    fn test_au_state_roundtrip_via_trait() {
        let mut au = delay();

        let state = au.get_state(&main_thread()).expect("save should succeed");
        assert!(!state.is_empty(), "State should not be empty");

        au.set_state(&main_thread(), &state)
            .expect("restore should succeed");
    }

    /// Live half: drive a real AU's automation path, assert the unit holds the
    /// native value, then assert the read path inverts it.
    ///
    /// Full-scale automation must land on the parameter's declared MAXIMUM, not
    /// on `1.0` (the denormalization), and `get_parameter` must answer `1.0`
    /// rather than the maximum (the `PluginParams` contract, normalized for every
    /// format). Both directions in one test because either alone can pass while
    /// the pair is inconsistent.
    #[test]
    fn param_automation_denormalizes_and_reads_back_normalized() {
        let mut au = delay();

        // Pick a writable parameter with a genuinely wide plain range: one
        // whose max is far from 1.0, so identity-vs-denormalized is decidable.
        let target = au
            .get_parameter_list(&main_thread())
            .into_iter()
            .find(|p| {
                p.range.bounds().is_some_and(|(_, max)| max > 2.0)
                    && p.flag(ParamFlags::READ_ONLY) == Some(false)
            })
            .expect("AUDelay should expose a wide-range writable parameter");
        let (min, max) = target
            .range
            .bounds()
            .expect("AU declares a plain range for every parameter");

        for (normalized, expected) in [
            (1.0f64, max),
            (0.0f64, min),
            (0.5f64, min + 0.5 * (max - min)),
        ] {
            let mut changes = ParameterChanges::new();
            let mut queue = ParameterQueue::with_capacity(target.id, 1);
            queue.add_point(0, normalized);
            changes.add_queue(queue);
            run(&mut au, 64, None, Some(&changes)).expect("process should succeed");

            // Tolerance scales with the range: AU stores parameters as f32, so
            // a 22 kHz range round-trips to ~1e-4 relative precision.
            let tolerance = (max - min).abs() * 1e-4;

            // The subject of this test: the automation point was normalized,
            // and the AU must hold the *native* value. Read straight off the
            // unit rather than through `get_parameter`, which normalizes: going
            // through it would assert the identity of two conversions and pass
            // even if both were wrong.
            let native = f64::from(
                tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), target.id.get())
                    .expect("param is readable"),
            );
            assert!(
                (native - expected).abs() <= tolerance,
                "param {} ('{}'): normalized {normalized} should reach the AU as \
                 {expected} in native units, got {native} (range [{min}, {max}])",
                target.id,
                target.name,
            );

            // And the read path inverts it.
            let read_back = au.get_parameter(&main_thread(), target.id);
            assert!(
                (read_back - normalized).abs() <= 1e-4,
                "param {} ('{}'): should read back as the normalized {normalized}, \
                 got {read_back}",
                target.id,
                target.name,
            );
        }
    }

    /// The display seam denormalizes before asking the AU, and reports the AU's
    /// silence as `None` rather than inventing a label.
    ///
    /// **This asserts an absence, and that is the honest measurement.** No Apple
    /// AU on macOS 15.6 implements `ParameterStringFromValue` (probed in
    /// `tutti-au-host`'s `au_param_display` suite, which pins the same thing one
    /// layer down), so no Apple fixture can demonstrate a *string* coming back.
    /// What is still worth pinning here is that the seam asks about the **right
    /// value**: the write path and the text path must denormalize identically,
    /// so a third-party AU answering for a `[10, 22050]` Hz cutoff does not
    /// describe `1 Hz` where the caller named full scale.
    #[test]
    fn au_parameter_text_denormalizes_and_reports_absence_honestly() {
        let mut au = delay();
        let main = main_thread();

        let target = au
            .get_parameter_list(&main)
            .into_iter()
            .find(|p| p.range.bounds().is_some_and(|(_, max)| max > 2.0))
            .expect("AUDelay should expose a wide-range parameter");
        let (min, max) = target.range.bounds().expect("AU declares a plain range");

        // The write path lands full scale on `max`, not on the raw `1.0`.
        au.set_parameter(&main, target.id, Normalized::of(1.0));
        let after =
            tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), target.id.get())
                .expect("param is readable");
        let tolerance = (max - min).abs() * 1e-4;
        assert!(
            (f64::from(after) - max).abs() <= tolerance,
            "param {} ('{}'): normalized 1.0 must denormalize to {max}, got {after}",
            target.id,
            target.name,
        );

        // The AU is asked, and declines.
        assert_eq!(
            au.parameter_text(&main, target.id, Normalized::of(1.0)),
            None,
            "param {} ('{}'): no Apple AU implements ParameterStringFromValue. \
             A Some() here means macOS gained the property: re-measure and \
             tighten this into a real round-trip rather than relaxing it.",
            target.id,
            target.name,
        );
        assert_eq!(
            au.parameter_value_from_text(&main, target.id, "1000 Hz"),
            None,
            "nor ParameterValueFromString: a fabricated number here would be \
             written into the user's preset",
        );
    }
}
