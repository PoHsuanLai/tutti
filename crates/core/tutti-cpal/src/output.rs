//! CPAL audio I/O — callback state, RT entry point, and device stream management.
//!
//! The RT callback renders the graph ([`Engine`]) once per block; MIDI is
//! the graph's (its input, clock and out nodes run in the render). Metering
//! runs over the result.

use cpal::traits::DeviceTrait;
use std::sync::Arc;
use tutti_core::Engine;
use tutti_core::{AudioTap, MasterMeter};
use tutti_core::{ChannelLayout, InterleavedMut, SampleRate, ScopedNoDenormals};

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

/// Everything the output callback reads: the [`Engine`], the master meter and
/// the audio tap.
///
/// Built once on the control thread and shared as an `Arc` between the
/// [`TuttiDriver`](crate::TuttiDriver) (or [`AudioEngine`]) that owns the stream
/// and the stream's callback, which passes it to [`process_audio`].
pub struct AudioCallbackState {
    pub(crate) engine: Engine,
    pub(crate) meter: MasterMeter,
    pub(crate) tap: AudioTap,
}

impl AudioCallbackState {
    /// Creates the state a stream's callback reads, on the control thread.
    pub fn new(engine: Engine, meter: MasterMeter, tap: AudioTap) -> Self {
        Self { engine, meter, tap }
    }

    /// Clears the audio-thread owner checks, ahead of moving the callback to a
    /// new thread.
    ///
    /// Currently a no-op: the engine's audio-thread cells check for concurrent
    /// borrows, not for a fixed owner thread, so a device switch needs no
    /// reset. [`TuttiDriver`](crate::TuttiDriver) still calls it on every
    /// restart; do not write code that depends on it doing something.
    ///
    /// Control-thread only, and only while no stream is running.
    pub fn reset_owners(&self) {
        self.engine.reset_owners();
    }
}

/// Renders one block of the graph into `output`, an interleaved device buffer
/// that carries its own width.
///
/// The graph's root outputs are folded to that width (see [`Engine::process`]),
/// and denormals are flushed for the duration of the call. This is the render
/// the output stream runs each callback; call it directly only on a state no
/// running stream is also rendering (see
/// [`TuttiDriver::from_parts`](crate::TuttiDriver::from_parts)).
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
    state.engine.process(output);
}

/// An output device, its stream configuration, and the stream while it runs.
///
/// [`TuttiDriver`](crate::TuttiDriver) wraps one and is what a host normally
/// holds; use `AudioEngine` directly to open the device before the graph
/// exists (its [`sample_rate`](Self::sample_rate) is the rate to build the
/// graph at). Every method runs on the control thread.
///
/// The stream runs until [`stop`](Self::stop) or until the engine is dropped.
/// Backend errors go to [`faults`](Self::faults).
// The running stream is a `Box<dyn RunningStream>` rather than a type
// parameter so the driver choice does not leak into every type that stores an
// `AudioEngine`.
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
    /// Opens a device on the default host and reads its default output config,
    /// without starting a stream. `None` selects the host's default device.
    ///
    /// # Errors
    /// [`Error::InvalidDevice`] if `device_index` is out of range, or
    /// [`Error::DeviceNotAvailable`] if the device has no default output config.
    pub fn new(device_index: Option<usize>) -> Result<Self> {
        Self::open(DeviceHost::open(AudioHost::Default)?, device_index.into())
    }

    /// Opens a device on the given host and reads its default output config,
    /// without starting a stream.
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

    /// Creates an engine with no host and no device, at the caller's spec.
    ///
    /// Pairs with [`ManualStreamDriver`](crate::ManualStreamDriver) to run the
    /// whole lifecycle — start, render, fault, stop, restart — with no sound
    /// card. Such an engine can only be started with
    /// [`start_with`](Self::start_with); [`start`](Self::start) and
    /// [`device_name`](Self::device_name) return [`Error::InvalidDevice`].
    pub fn from_spec(spec: OutputSpec) -> Self {
        Self {
            spec,
            target: None,
            faults: Arc::new(StreamFaults::new()),
            running: None,
        }
    }

    /// Returns the configuration of the stream that is playing, or that the
    /// next start would use.
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    /// Returns the backend fault record, which survives stop and restart.
    ///
    /// Take it once at startup and poll it; CPAL's error callback returns
    /// nothing, so a fault such as an unplugged device is reported only here.
    pub fn faults(&self) -> Arc<StreamFaults> {
        Arc::clone(&self.faults)
    }

    /// Builds a stream on the selected device and starts it. Does nothing if a
    /// stream is already running.
    ///
    /// Re-reads the device's default config first, so after a
    /// [`set_device`](Self::set_device) the [`spec`](Self::spec),
    /// [`sample_rate`](Self::sample_rate) and [`channels`](Self::channels)
    /// describe the device that is actually playing. The stream is opened with
    /// a fixed buffer of [`PREFERRED_QUANTUM`](crate::PREFERRED_QUANTUM) frames
    /// clamped to the device's range, when the device reports one. Clears
    /// [`faults`](Self::faults).
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

    /// Starts a stream at the current [`spec`](Self::spec) through `driver`.
    /// Does nothing if a stream is already running.
    ///
    /// Unlike [`start`](Self::start) it resolves no device: the driver decides
    /// where the callback runs. With a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver) the caller runs each
    /// callback by hand. Clears [`faults`](Self::faults).
    ///
    /// # Errors
    /// Whatever the driver's [`open`](StreamDriver::open) reports; for
    /// [`CpalDriver`](crate::CpalDriver), [`Error::InvalidConfig`] for an
    /// unsupported sample format or [`Error::BuildStream`] /
    /// [`Error::PlayStream`] from CPAL.
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

    /// Stops and drops the running stream, if any. Idempotent.
    ///
    /// When this returns, the callback no longer runs.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop();
        }
    }

    /// Returns the sample rate of the running stream, or of the config the
    /// next start would use.
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.sample_rate
    }

    /// Returns the channel layout of the running stream, or of the config the
    /// next start would use. This is the width [`process_audio`] is handed.
    pub fn channels(&self) -> ChannelLayout {
        self.spec.channels
    }

    /// Returns whether a stream is open **and** the backend has not reported
    /// the device gone.
    ///
    /// A stream whose device was unplugged reads `false` here even though it
    /// has not been stopped; see [`StreamFaults::is_disconnected`].
    pub fn is_running(&self) -> bool {
        self.running.is_some() && !self.faults.is_disconnected()
    }

    /// Selects the device the next [`start`](Self::start) opens, by position
    /// in the enumeration; `None` means the host default. Does not disturb a
    /// running stream, and does nothing on an engine built with
    /// [`from_spec`](Self::from_spec).
    pub fn set_device(&mut self, index: Option<usize>) {
        self.select_device(index.into());
    }

    /// Selects the device the next [`start`](Self::start) opens, by name or
    /// index. Does not disturb a running stream, and does nothing on an engine
    /// built with [`from_spec`](Self::from_spec).
    pub fn select_device(&mut self, sel: DeviceSelector) {
        if let Some((_, current)) = &mut self.target {
            *current = sel;
        }
    }

    /// Returns the selected device's name, queried fresh from the host.
    ///
    /// # Errors
    /// [`Error::InvalidDevice`] if the selector no longer resolves or the
    /// engine was built with [`from_spec`](Self::from_spec), or
    /// [`Error::DeviceNameError`] if the host cannot name it.
    pub fn device_name(&self) -> Result<String> {
        let Some((host, sel)) = &self.target else {
            return Err(Error::InvalidDevice(
                "this engine was built from a bare spec and has no device".into(),
            ));
        };
        Ok(host.device(Direction::Output, sel)?.name()?)
    }

    /// Lists the default host's output devices as `(index, name)` pairs.
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
    use tutti_graph::{Editor, Prepare};
    use tutti_nodes::testing::Osc;
    use tutti_nodes::{SvfFilterNode, SvfType};

    /// Build an engine + transport pair whose graph actually renders.
    ///
    /// An empty graph renders silence without running a node — which would
    /// leave the transport assertions below reading a playhead nothing
    /// exercised. A sine through a filter into the output gives the render
    /// path real nodes to run, buffers to hand between them, and a fold to
    /// the device width. The playhead is the engine's own clock; there is no
    /// clock node in the graph.
    ///
    /// `tests/rt_no_alloc.rs` mirrors this fixture, because the allocation
    /// gates need a `#[global_allocator]` that only a test binary root can
    /// declare.
    fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
        let transport = Transport::new(sample_rate);

        let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(sample_rate), Samples(512)));
        let (source, filter) = (NodeKey(1), NodeKey(2));
        ed.insert(source, "sine", Osc::sine(Hz(220.0)));
        ed.insert(
            filter,
            "filter",
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(2_000.0), Q(0.7)),
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
