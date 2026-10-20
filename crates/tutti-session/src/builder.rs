//! [`SessionBuilder`]: one ordered, fallible build of a graph, its engine and
//! the nodes the engine inserts, offline, on a device, or detached.

use std::sync::Arc;

use tutti_core::{
    AudioNode, AudioTap, ChannelLayout, ClickNode, ClickSettings, MasterMeter, RtPublish,
    SampleRate, Samples, Transport, MAX_ROOT_CHANNELS,
};

use crate::graph::{Graph, LIVE_MAX_BLOCK};
use crate::{Result, Session};

/// Builds a [`Session`].
///
/// ```
/// use tutti_core::{ChannelLayout, SampleRate, Samples};
/// use tutti_session::Session;
///
/// let session = Session::builder()
///     .outputs_floor(6) // a 5.1 project renders six, whatever the device
///     .render_workers(1)
///     .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(512))
///     .unwrap();
/// assert_eq!(session.graph_ref().outputs(), 6);
/// ```
#[derive(Clone, Debug)]
pub struct SessionBuilder {
    inputs: usize,
    outputs_floor: usize,
    render_workers: usize,
    #[cfg(feature = "midi-hardware")]
    midi_hardware: bool,
    #[cfg(feature = "midi")]
    mpe_mode: tutti_midi_types::MpeMode,
    #[cfg(feature = "modulation")]
    unresolved_warning: std::time::Duration,
    #[cfg(feature = "plugin")]
    snapshot_interval: std::time::Duration,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            inputs: 0,
            outputs_floor: 2,
            render_workers: 1,
            #[cfg(feature = "midi-hardware")]
            midi_hardware: false,
            #[cfg(feature = "midi")]
            mpe_mode: tutti_midi_types::MpeMode::Disabled,
            #[cfg(feature = "modulation")]
            unresolved_warning: crate::UNRESOLVED_WARNING,
            #[cfg(feature = "plugin")]
            snapshot_interval: crate::SNAPSHOT_INTERVAL,
        }
    }
}

/// What every build path shares: the graph, the transport, the click, the
/// MIDI nodes, and what the engine's callback state holds.
struct Parts {
    graph: Graph,
    transport: Transport,
    click: Option<AudioNode>,
    click_settings: Arc<ClickSettings>,
    compensation: Arc<RtPublish<Vec<Samples>>>,
    #[cfg(feature = "midi")]
    midi: Option<crate::MidiNodes>,
    #[cfg(feature = "midi-hardware")]
    midi_io: Option<tutti_midi_hardware::MidiSession>,
}

impl SessionBuilder {
    /// Global graph inputs (hardware or host input channels). Default 0.
    pub fn inputs(mut self, inputs: usize) -> Self {
        self.inputs = inputs;
        self
    }

    /// The project's output width: the graph root is at least this wide, and
    /// at least the device's ([`root_width`]). Default 2.
    pub fn outputs_floor(mut self, outputs: usize) -> Self {
        self.outputs_floor = outputs;
        self
    }

    /// Threads per block, the callback included: `0` for one per core,
    /// `1` (the default) to render on the callback alone. Each export gets
    /// its own pool of the same size.
    pub fn render_workers(mut self, workers: usize) -> Self {
        self.render_workers = workers;
        self
    }

    /// Open the OS MIDI layer ([`Session::midi_io`]) and feed the engine's
    /// MIDI input node from its ports. Default off.
    #[cfg(feature = "midi-hardware")]
    pub fn midi_hardware(mut self, open: bool) -> Self {
        self.midi_hardware = open;
        self
    }

    /// The MPE mode the engine's MIDI input ingests in. Default `Disabled`.
    #[cfg(feature = "midi")]
    pub fn mpe_mode(mut self, mode: tutti_midi_types::MpeMode) -> Self {
        self.mpe_mode = mode;
        self
    }

    /// How long a modulation target may stay unresolved, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now), before it is logged and
    /// reported ([`SessionEvent::ModulationUnresolved`](crate::SessionEvent::ModulationUnresolved)).
    /// Default [`UNRESOLVED_WARNING`](crate::UNRESOLVED_WARNING), 4 s.
    #[cfg(feature = "modulation")]
    pub fn unresolved_warning(mut self, after: std::time::Duration) -> Self {
        self.unresolved_warning = after;
        self
    }

    /// How often a healthy hosted plugin's state is captured, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now)
    /// ([`SessionEvent::PluginSnapshotDue`](crate::SessionEvent::PluginSnapshotDue)).
    /// Default [`SNAPSHOT_INTERVAL`](crate::SNAPSHOT_INTERVAL), 8 s.
    #[cfg(feature = "plugin")]
    pub fn snapshot_interval(mut self, every: std::time::Duration) -> Self {
        self.snapshot_interval = every;
        self
    }

    /// Build the graph at `rate` for blocks of up to `max_block`, with the
    /// device's callback `quantum` when known, `outputs` wide; insert the
    /// metronome and the engine's MIDI nodes.
    fn parts(
        &self,
        rate: SampleRate,
        max_block: Samples,
        quantum: Option<Samples>,
        outputs: usize,
    ) -> Parts {
        // Built first so its port manager can feed the graph's MIDI input.
        #[cfg(feature = "midi-hardware")]
        let midi_io = self.midi_hardware.then(|| {
            let ports = Arc::new(tutti_midi_hardware::HardwareMidiInputs::new(256));
            // It times each inbound event by turning a wall-clock delta into a
            // frame offset, which takes the device's real rate: told here,
            // before any callback runs.
            ports.set_sample_rate(rate);
            tutti_midi_hardware::MidiSession::new(ports)
        });

        let mut graph = Graph::new(self.inputs, outputs, rate, max_block, quantum);
        graph.set_render_workers(self.render_workers);
        let transport = Transport::new(rate);
        let click_settings = Arc::new(ClickSettings::new());

        // The metronome. It reads the beat, play and record state from its
        // block's `Env`, and the count-in flag off the transport (read only).
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let (click, _settings) =
            graph.insert(ClickNode::new(&transport, Arc::clone(&click_settings)));

        // The engine's MIDI as graph nodes: the hardware input (translating,
        // and ingesting MPE in the builder's mode), the clock (off until a
        // host enables it) and the hardware out the clock feeds.
        #[cfg(feature = "midi")]
        let midi = {
            #[cfg(feature = "midi-hardware")]
            let wire: Option<Arc<dyn tutti_midi_types::MidiIn>> = midi_io
                .as_ref()
                .map(|io| Arc::clone(io.ports()) as Arc<dyn tutti_midi_types::MidiIn>);
            #[cfg(not(feature = "midi-hardware"))]
            let wire: Option<Arc<dyn tutti_midi_types::MidiIn>> = None;
            let (input, input_controls) =
                graph.insert(tutti_midi_runtime::MidiInputNode::new(wire).with_mpe(self.mpe_mode));
            let (clock, clock_master) = graph.insert(tutti_midi_runtime::ClockNode::new());
            let (hardware_out, out_controls) = graph.insert(tutti_midi_runtime::MidiOutNode::new());
            graph.set_feed(hardware_out, "clock", vec![clock.into()]);
            Some(crate::MidiNodes {
                input,
                input_controls,
                clock,
                clock_master,
                hardware_out,
                out_controls,
            })
        };

        Parts {
            graph,
            transport,
            click: Some(click),
            click_settings,
            compensation: Arc::default(),
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi-hardware")]
            midi_io,
        }
    }

    /// The session over `parts`, `engine` its offline engine if any, with
    /// (`sampler`) a disk streamer when `streamer`.
    fn session(
        &self,
        parts: Parts,
        engine: Option<tutti_core::Engine>,
        streamer: bool,
    ) -> Result<Session> {
        #[cfg(feature = "sampler")]
        let disk_streamer = if streamer {
            Some(tutti_sampler::DiskStreamer::new(
                parts.graph.prepared().sample_rate(),
                tutti_sampler::DiskStreamerConfig {
                    pdc: Some(Arc::clone(&parts.compensation)),
                    ..Default::default()
                },
            )?)
        } else {
            None
        };
        #[cfg(not(feature = "sampler"))]
        let _ = streamer;
        Ok(Session {
            graph: parts.graph,
            transport: parts.transport,
            engine,
            meter: MasterMeter::new(),
            tap: AudioTap::new(),
            click: parts.click,
            click_settings: parts.click_settings,
            compensation: parts.compensation,
            latency: Samples(0),
            resumed: false,
            #[cfg(feature = "midi")]
            midi: parts.midi,
            #[cfg(feature = "midi")]
            midi_state: Default::default(),
            #[cfg(feature = "modulation")]
            mods: crate::modulation::ModSession::new(self.unresolved_warning),
            #[cfg(feature = "midi-hardware")]
            midi_io: parts.midi_io,
            #[cfg(feature = "sampler")]
            disk_streamer,
            #[cfg(feature = "device")]
            faults: None,
            #[cfg(feature = "plugin")]
            snapshot_interval: self.snapshot_interval,
            #[cfg(feature = "plugin")]
            render_mode: Default::default(),
            #[cfg(feature = "export")]
            export: Default::default(),
        })
    }

    /// An offline session: everything but a stream. The host pulls blocks
    /// with [`Session::render`] (`Engine::process`), each up to `max_block`
    /// frames per graph block (a longer one renders as consecutive graph
    /// blocks), `layout` wide; the graph root is
    /// [`root_width`]`(outputs_floor, layout)`.
    ///
    /// # Errors
    ///
    /// When the engine refuses the graph, or (`sampler`) the disk streamer
    /// cannot start.
    pub fn build_offline(
        self,
        rate: SampleRate,
        layout: ChannelLayout,
        max_block: Samples,
    ) -> Result<Session> {
        let mut parts = self.parts(
            rate,
            max_block,
            None,
            root_width(self.outputs_floor, layout),
        );
        let engine = parts.graph.engine(&parts.transport)?;
        self.session(parts, Some(engine), true)
    }

    /// A session with no engine: the executor stays on this side, every
    /// commit is applied at once, and [`Session::render_frame`] renders it
    /// (or [`Graph::take_audio_side`] hands it to a test). No metronome and
    /// no MIDI nodes are inserted, and no disk streamer is started: the
    /// graph is exactly `outputs_floor` wide, empty, at `rate`.
    ///
    /// # Errors
    ///
    /// None today; `Result` for the build paths' common shape.
    pub fn build_detached(self, rate: SampleRate) -> Result<Session> {
        let mut graph = Graph::new(self.inputs, self.outputs_floor, rate, LIVE_MAX_BLOCK, None);
        graph.set_render_workers(self.render_workers);
        self.session(
            Parts {
                graph,
                transport: Transport::new(rate),
                click: None,
                click_settings: Arc::new(ClickSettings::new()),
                compensation: Arc::default(),
                #[cfg(feature = "midi")]
                midi: None,
                #[cfg(feature = "midi-hardware")]
                midi_io: None,
            },
            None,
            false,
        )
    }

    /// A session on the output device `device` selects: the device opened
    /// first, the graph built at its rate and callback size, the callback
    /// live on return. The [`Device`](crate::Device) stays on this thread.
    ///
    /// # Errors
    ///
    /// When the device cannot be opened or started, or the graph cannot be
    /// built over it.
    #[cfg(feature = "device")]
    pub fn build_device(
        self,
        device: tutti_cpal::DeviceSelector,
    ) -> Result<(Session, crate::Device)> {
        let host = tutti_cpal::DeviceHost::open(tutti_cpal::AudioHost::Default)?;
        let engine = tutti_cpal::AudioEngine::open(host, device)?;
        self.build_on(engine, |engine, state| engine.start(state))
    }

    /// [`build_device`](Self::build_device) on a stream driver of the
    /// caller's choosing, `spec` standing for the config its device reports:
    /// the device-free build (a `tutti_cpal::ManualStreamDriver`, for a test
    /// that renders the callback itself).
    ///
    /// # Errors
    ///
    /// When the driver cannot start, or the graph cannot be built.
    #[cfg(feature = "device")]
    pub fn build_device_on<D>(
        self,
        spec: tutti_cpal::OutputSpec,
        driver: D,
    ) -> Result<(Session, crate::Device)>
    where
        D: tutti_cpal::StreamDriver,
        D::Running: 'static,
    {
        self.build_on(tutti_cpal::AudioEngine::from_spec(spec), |engine, state| {
            engine.start_with(state, driver)
        })
    }

    /// Build over an opened `audio_engine` and start it with `start`.
    ///
    /// **The graph runs at the device's rate.** Every node is prepared at
    /// the graph's rate, so a graph at any other rate would run every
    /// oscillator off pitch.
    #[cfg(feature = "device")]
    fn build_on(
        self,
        mut audio_engine: tutti_cpal::AudioEngine,
        start: impl FnOnce(
            &mut tutti_cpal::AudioEngine,
            Arc<tutti_cpal::AudioCallbackState>,
        ) -> tutti_cpal::Result<()>,
    ) -> Result<(Session, crate::Device)> {
        let spec = audio_engine.spec().clone();
        let mut parts = self.parts(
            spec.sample_rate,
            LIVE_MAX_BLOCK,
            spec.quantum,
            root_width(self.outputs_floor, spec.channels),
        );
        let engine = parts.graph.engine(&parts.transport)?;
        let mut session = self.session(parts, None, true)?;
        // Consumers read the meter directly, so it measures from the start.
        session.meter.enable();
        let state = Arc::new(tutti_cpal::AudioCallbackState::new(
            engine,
            session.meter.clone(),
            session.tap.clone(),
        ));
        start(&mut audio_engine, Arc::clone(&state))?;
        let device = crate::Device::new(tutti_cpal::TuttiDriver::from_parts(audio_engine, state));
        session.faults = Some((device.faults(), device.faults().count()));
        Ok((session, device))
    }
}

/// How wide the graph root is built, given what the project asks for and
/// what the device presents.
///
/// **These are two independent numbers, and neither may narrow the other.**
///
/// `project` is the project's width. A 5.1 project on a stereo laptop must
/// still render six channels: `Engine::process_segment` folds them into the
/// device buffer through the shared ITU matrices every block. Clamping the
/// root down to the device would make that fold a no-op that hides real
/// channel loss.
///
/// The device is a **floor** because a root narrower than the device leaves
/// the extra device channels permanently silent: `fold_frame` zero-fills
/// rather than upmixing, so nothing downstream can recover them.
///
/// The [`MAX_ROOT_CHANNELS`] clamp is applied here, at construction:
/// `Engine::new` bounds the editor to that many global outputs, so a wider
/// root would be refused at its first commit and never play. An export's
/// fork is prepared on its own, with no such cap.
pub fn root_width(project: usize, device: ChannelLayout) -> usize {
    project
        .max(device.count() as usize)
        .clamp(1, MAX_ROOT_CHANNELS)
}
