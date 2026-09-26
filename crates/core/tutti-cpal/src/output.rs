//! CPAL audio I/O — callback state, RT entry point, and device stream management.
//!
//! The RT callback runs two steps per block: an optional MIDI *pre-block*
//! producer ([`MidiPreBlock`]) that delivers events into node inboxes, then the
//! graph render ([`Engine`]). Metering runs over the result.

use cpal::traits::DeviceTrait;
use std::sync::Arc;
use tutti_core::Engine;
use tutti_core::{AudioTap, MasterMeter};
use tutti_core::{ChannelLayout, InterleavedMut, SampleRate, ScopedNoDenormals};

#[cfg(feature = "midi")]
use tutti_midi_runtime::{MidiPostBlock, MidiPreBlock};

use crate::block::OutputBlock;
use crate::driver_seam::{CpalDriver, OutputSpec, RunningStream, StreamDriver};
use crate::error::{Error, Result};
use crate::faults::StreamFaults;
use crate::host::{AudioHost, DeviceHost, DeviceSelector, Direction};

/// Maximum frames per CPAL callback buffer.
///
/// The stream's mix buffer is sized to this once and never resized, so it is
/// also the largest block [`process_audio`] can be handed: a longer callback is
/// clamped and its tail silenced rather than reallocating on the audio thread.
pub const MAX_FRAMES: usize = 8192;

/// State shared between the engine and the RT audio callback.
///
/// Holds the [`Engine`] and, under `midi`, the two MIDI phases that bracket the
/// render. All are RT-safe; the callback owns the ordering
/// (`pre_block.run` → `engine.process` → `post_block.run`).
pub struct AudioCallbackState {
    pub(crate) engine: Engine,
    /// The once-per-block MIDI producer, run before the graph render to deliver
    /// events into node inboxes. `None` when no MIDI subsystem is wired.
    #[cfg(feature = "midi")]
    pub(crate) pre_block: Option<MidiPreBlock>,
    /// The once-per-block MIDI consumer, run after the graph render to fan out
    /// whatever the graph emitted. `None` when no MIDI subsystem is wired.
    ///
    /// Separate from `pre_block` rather than folded into it because the two run
    /// on opposite sides of `engine.process` — that ordering *is* the design
    /// (see [`MidiPostBlock`]), and a single object would hide it.
    #[cfg(feature = "midi")]
    pub(crate) post_block: Option<MidiPostBlock>,
    pub(crate) meter: MasterMeter,
    pub(crate) tap: AudioTap,
}

impl AudioCallbackState {
    /// Assemble the state a stream's callback reads, with no MIDI phases
    /// installed. Called once at engine build, on the control thread.
    pub fn new(engine: Engine, meter: MasterMeter, tap: AudioTap) -> Self {
        Self {
            engine,
            #[cfg(feature = "midi")]
            pre_block: None,
            #[cfg(feature = "midi")]
            post_block: None,
            meter,
            tap,
        }
    }

    /// Install the pre-block MIDI producer (called once at engine build).
    #[cfg(feature = "midi")]
    pub fn with_pre_block(mut self, pre_block: MidiPreBlock) -> Self {
        self.pre_block = Some(pre_block);
        self
    }

    /// Install the post-block MIDI consumer (called once at engine build).
    #[cfg(feature = "midi")]
    pub fn with_post_block(mut self, post_block: MidiPostBlock) -> Self {
        self.post_block = Some(post_block);
        self
    }

    /// Clear the RT processors' owner assertions, ahead of a device switch.
    ///
    /// **Currently a no-op all the way down**, and the docs here used to claim
    /// otherwise. Every call in this chain bottoms out in
    /// `AudioThreadCell::reset_owner`, whose own doc reads "No-op, kept for
    /// source compatibility. The cell pins no owner thread, so a device switch
    /// needs no reset." The cell's debug check is a *concurrent-borrow*
    /// detector (`in_use.swap`), not an owner-thread one, so moving the
    /// callback to a new CPAL thread has needed no reset since that change;
    /// `tutti_types::RtEventBuf::reset_owner` already said so and this did not.
    ///
    /// Kept rather than deleted because it is public API and because the
    /// property it guards is one a future cell might reinstate. Called from
    /// `TuttiDriver::restart` for the same reason. Do not write new code that
    /// depends on it doing something.
    ///
    /// Control-thread only, and only while no stream is running.
    pub fn reset_owners(&self) {
        self.engine.reset_owners();
        #[cfg(feature = "midi")]
        if let Some(pre_block) = &self.pre_block {
            pre_block.reset_owners();
        }
        #[cfg(feature = "midi")]
        if let Some(post_block) = &self.post_block {
            post_block.reset_owners();
        }
    }
}

/// Render one block into `output`, an interleaved device buffer that carries
/// its own width. The graph root is folded to that width (see
/// [`Engine::process`]).
///
/// # Real-time
///
/// **Runs on the audio thread. Must not allocate, lock, or block.** Every
/// buffer it touches is sized once at stream build to [`MAX_FRAMES`]; a longer
/// callback is clamped and its tail silenced rather than reallocating here.
/// `tests/rt_no_alloc.rs` gates this against a disabled allocator.
#[inline]
pub fn process_audio(state: &AudioCallbackState, output: &mut InterleavedMut<'_>) {
    let _no_denormals = ScopedNoDenormals::new();
    // The frame count is the buffer's, not a separate argument that could
    // disagree with it. `len()` is frames; the division by the stride happens
    // inside the type, once.
    #[cfg(feature = "midi")]
    let frames = output.len();
    // Pre-block MIDI: deliver this block's events into node inboxes before the
    // graph renders.
    #[cfg(feature = "midi")]
    if let Some(pre_block) = &state.pre_block {
        pre_block.run(frames);
    }
    state.engine.process(output);
    // Post-block MIDI: fan out whatever the graph emitted. Must run *after*
    // `process`, because that is what makes delivery independent of the order
    // the emitting nodes happened to be scheduled in — every consumer's poll
    // for this block has already happened, so an event always lands after it.
    #[cfg(feature = "midi")]
    if let Some(post_block) = &state.post_block {
        post_block.run();
    }
}

/// Owns the running stream and the device configuration it was built from.
///
/// The lifecycle half of the device layer: [`TuttiDriver`](crate::TuttiDriver)
/// wraps one and is what a host normally holds. Every method here runs on the
/// control thread — none is callable from the RT callback.
///
/// The running stream is a `Box<dyn RunningStream>` rather than a type
/// parameter, for the reason `tutti_io::Recorder`'s field doc gives about its
/// own driver: this is a type a caller stores in a field, and making it
/// `AudioEngine<D>` would push the choice of driver into `TuttiDriver`, into
/// `bevy-tutti`'s `NonSend`, and into every host signature that holds one.
pub struct AudioEngine {
    spec: OutputSpec,
    /// `None` for an engine built from a bare spec — no host, no device, so
    /// nothing to re-resolve on start. That is what makes a device-free
    /// lifecycle test possible.
    target: Option<(DeviceHost, DeviceSelector)>,
    faults: Arc<StreamFaults>,
    running: Option<Box<dyn RunningStream>>,
}

// Hand-rolled: the running stream is not `Debug`. Reports the configuration a
// host would want in a log line; whether a stream exists is `is_running`.
impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("spec", &self.spec)
            .field("is_running", &self.is_running())
            .field("faults", &self.faults.count())
            .finish_non_exhaustive()
    }
}

impl AudioEngine {
    /// Open a device on the default host and read its config, without starting
    /// a stream. `None` selects the host's default device.
    ///
    /// # Errors
    /// [`Error::InvalidDevice`] if `device_index` is out of range, or
    /// [`Error::DeviceNotAvailable`] if the device has no default output config.
    pub fn new(device_index: Option<usize>) -> Result<Self> {
        Self::open(DeviceHost::open(AudioHost::Default)?, device_index.into())
    }

    /// Open a device on a named host.
    ///
    /// # Errors
    /// As [`new`](Self::new), plus whatever resolving `sel` on `host` reports.
    pub fn open(host: DeviceHost, sel: DeviceSelector) -> Result<Self> {
        let device = host.device(Direction::Output, &sel)?;
        let config = device.default_output_config()?;
        Ok(Self {
            spec: OutputSpec::from_supported(&config),
            target: Some((host, sel)),
            faults: Arc::new(StreamFaults::new()),
            running: None,
        })
    }

    /// An engine with no host and no device: the spec is the caller's.
    ///
    /// Pairs with [`ManualStreamDriver`](crate::ManualStreamDriver) to give a
    /// complete engine lifecycle — start, render, fault, stop, restart — with
    /// no sound card anywhere. That combination is what makes the device layer
    /// testable at all; before it, every method here was unreachable from a
    /// test.
    pub fn from_spec(spec: OutputSpec) -> Self {
        Self {
            spec,
            target: None,
            faults: Arc::new(StreamFaults::new()),
            running: None,
        }
    }

    /// The configuration of the stream that is playing, or that would be.
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    /// The fault sink, which survives stop and restart.
    ///
    /// Take this once at startup and read it whenever; a fault has nowhere
    /// else to go, because CPAL's error callback returns nothing.
    pub fn faults(&self) -> Arc<StreamFaults> {
        Arc::clone(&self.faults)
    }

    /// Build a stream on the selected device and start it. A no-op if one is
    /// already running.
    ///
    /// Re-reads the device's config, so [`spec`](Self::spec) describes the
    /// stream that is actually playing rather than whatever
    /// [`new`](Self::new) saw. `set_device` + `start` (what
    /// [`TuttiDriver::restart`](crate::TuttiDriver::restart) does) reaches
    /// here with a different device than `new` read, and leaving the spec at
    /// its construction values would make `channels()` describe a device that
    /// is no longer playing while the audio itself is correct — so a reader
    /// sizing a buffer from it gets the old width with nothing to warn it.
    ///
    /// # Errors
    /// [`Error::InvalidDevice`] for an unresolvable selector,
    /// [`Error::DeviceNotAvailable`] if the config cannot be read,
    /// [`Error::InvalidConfig`] for a sample format the engine does not build,
    /// or [`Error::BuildStream`] / [`Error::PlayStream`] from CPAL.
    pub fn start(&mut self, state: Arc<AudioCallbackState>) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let device = self.resolve()?;
        self.start_with(state, CpalDriver::from_device(device))
    }

    /// Resolve the selected device and read its config into
    /// [`spec`](Self::spec), without opening a stream: the first half of
    /// [`start`](Self::start), split out so a restart can see the config the
    /// stream will run at before it runs (`TuttiDriver::restart_with`).
    pub(crate) fn resolve(&mut self) -> Result<cpal::Device> {
        let Some((host, sel)) = &self.target else {
            return Err(Error::InvalidDevice(
                "this engine was built from a bare spec and has no device to open; \
                 use `start_with` and supply a driver"
                    .into(),
            ));
        };
        let device = host.device(Direction::Output, sel)?;
        let config = device.default_output_config()?;
        self.spec = OutputSpec::from_supported(&config);
        Ok(device)
    }

    /// Replace the spec the next [`start_with`](Self::start_with) opens at:
    /// the device-free counterpart of [`resolve`](Self::resolve), where the
    /// caller is the device.
    pub(crate) fn set_spec(&mut self, spec: OutputSpec) {
        self.spec = spec;
    }

    /// [`start`](Self::start), with the caller choosing how the callback runs.
    ///
    /// The spec, the fault sink and the stop are identical; a driver decides
    /// only *where* the callback runs. So an engine over a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver) is the same engine,
    /// and a test over one exercises the shipped lifecycle rather than a
    /// stand-in for it.
    pub fn start_with<D: StreamDriver>(
        &mut self,
        state: Arc<AudioCallbackState>,
        driver: D,
    ) -> Result<()>
    where
        D::Running: 'static,
    {
        if self.is_running() {
            return Ok(());
        }
        // A restart after a disconnect must report healthy again.
        self.faults.clear();
        let block = OutputBlock::new(state, self.spec.channels);
        let running = driver.open(&self.spec, block, Arc::clone(&self.faults))?;
        self.running = Some(Box::new(running));
        Ok(())
    }

    /// Drop the stream, which stops the callback. Idempotent.
    ///
    /// Dropping is the stop: a stream runs for exactly as long as its handle
    /// lives.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop();
        }
    }

    /// Rate of the running stream, or of the config read at construction if
    /// none has started.
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.sample_rate
    }

    /// Channel layout of the running stream, or of the config read at
    /// construction if none has started. This is the width [`process_audio`]
    /// is handed.
    pub fn channels(&self) -> ChannelLayout {
        self.spec.channels
    }

    /// Whether a stream is open **and** the backend has not reported the
    /// device gone.
    ///
    /// The disconnect half is new, and is a behaviour change rather than a
    /// signature one: this used to return `true` for a stream whose device had
    /// been unplugged, because nothing read the error callback. That was the
    /// defect, not the contract.
    pub fn is_running(&self) -> bool {
        self.running.is_some() && !self.faults.is_disconnected()
    }

    /// Select the device the next [`start`](Self::start) opens. `None` means
    /// the host default. Does not disturb a running stream.
    pub fn set_device(&mut self, index: Option<usize>) {
        self.select_device(index.into());
    }

    /// Select the device by name or index. Does not disturb a running stream.
    pub fn select_device(&mut self, sel: DeviceSelector) {
        if let Some((_, current)) = &mut self.target {
            *current = sel;
        }
    }

    /// The selected device's name, queried fresh from the host.
    ///
    /// # Errors
    /// [`Error::InvalidDevice`] if the selector no longer resolves, or
    /// [`Error::DeviceNameError`] if the host cannot name it.
    pub fn device_name(&self) -> Result<String> {
        let Some((host, sel)) = &self.target else {
            return Err(Error::InvalidDevice(
                "this engine was built from a bare spec and has no device".into(),
            ));
        };
        Ok(host.device(Direction::Output, sel)?.name()?)
    }

    /// Enumerate the default host's output devices as `(index, name)` pairs.
    /// The index is positional — see [`DeviceSelector::Index`].
    ///
    /// # Errors
    /// [`Error::DevicesError`] if the host cannot enumerate.
    pub fn output_devices() -> Result<impl Iterator<Item = (usize, String)>> {
        Ok(DeviceHost::open(AudioHost::Default)?
            .output_devices()?
            .into_iter()
            .map(|d| (d.index, d.name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::graph::{Edge, InPort, OutPort, Source};
    use tutti_core::{Beat, BeatDuration, MotionEvent, Transport};
    use tutti_core::{Engine, NodeKey, SampleRate, Samples};
    use tutti_core::{Hz, Q};
    use tutti_graph::{Editor, Legacy, Prepare};
    use tutti_nodes::testing::Osc;
    use tutti_nodes::{SvfFilterNode, SvfType};

    /// Build an engine + transport pair whose graph actually renders.
    ///
    /// An empty graph renders silence without running a node — which would
    /// leave the transport assertions below reading a playhead nothing
    /// exercised. A sine through a filter into the output gives the render
    /// path real nodes to run, buffers to hand between them, and a fold to
    /// the device width. The playhead is the engine's own clock (doc 013
    /// Phase 3 PR 15: no clock node in the graph).
    ///
    /// `tests/rt_no_alloc.rs` mirrors this fixture, because the allocation
    /// gates need a `#[global_allocator]` that only a test binary root can
    /// declare.
    fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
        let transport = Transport::new(sample_rate);

        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(sample_rate), Samples(512)));
        let (source, filter) = (NodeKey(1), NodeKey(2));
        ed.insert(source, "sine", Legacy::new(Osc::sine(Hz(220.0))));
        ed.insert(
            filter,
            "filter",
            Legacy::new(SvfFilterNode::<f64>::new(
                SvfType::LowPass,
                Hz(2_000.0),
                Q(0.7),
            )),
        );
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: filter,
                port: 0,
            },
            Edge::Direct(Source::Node(OutPort {
                node: source,
                port: 0,
            })),
        );
        // The filter's single output on both device channels.
        ed.spec_mut().topology.outputs = vec![
            Source::Node(OutPort {
                node: filter,
                port: 0,
            });
            2
        ];
        ed.commit().expect("commits");
        let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
        let state = AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new());
        (transport, state)
    }

    #[test]
    fn test_transport_advances_with_graph() {
        let sample_rate = 44100.0;
        let (transport, state) = build_callback_state(sample_rate);

        transport.settings.set_tempo(120.0);
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();

        let frames = 256;
        let mut output = vec![0.0f32; frames * 2];
        process_audio(
            &state,
            &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
        );

        let expected_beat = Beat(256.0 * (120.0 / 60.0) / 44100.0);
        let actual_beat = transport.settings.beat();
        assert!(
            (actual_beat - expected_beat).abs() < BeatDuration(1e-6),
            "expected {expected_beat:?}, got {actual_beat:?}"
        );
    }

    #[test]
    fn test_transport_loop_wrapping() {
        let sample_rate = 44100.0;
        let (transport, state) = build_callback_state(sample_rate);

        transport.settings.set_tempo(120.0);
        transport.settings.loop_span.set_range(0.0, 4.0);
        transport.settings.loop_span.set_enabled(true);
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        transport.motion.seek.request(3.99);

        let frames = 1024;
        let mut output = vec![0.0f32; frames * 2];
        process_audio(
            &state,
            &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
        );

        let beat = transport.settings.beat();
        assert!(
            beat < Beat(4.0),
            "expected beat wrapped below 4.0, got {beat:?}"
        );
    }
}
