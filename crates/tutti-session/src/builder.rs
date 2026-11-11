//! [`SessionBuilder`]: one ordered, fallible build of a graph, its engine and
//! the nodes the engine inserts, offline, on a device, or detached.

use std::sync::Arc;

use tutti_graph::MAX_ROOT_CHANNELS;
use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_nodes::{ClickNode, ClickSettings};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate, Samples};

use crate::graph::{Graph, LIVE_MAX_BLOCK};
use crate::{Result, Session};

/// Builds a [`Session`].
///
/// ```
/// use tutti_types::{ChannelLayout, SampleRate, Samples};
/// use tutti_session::Session;
///
/// let session = Session::builder()
///     .outputs_floor(6) // a 5.1 project renders six, whatever the device
///     .render_workers(1)
///     .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(512))
///     .unwrap();
/// assert_eq!(session.graph_ref().outputs(), 6);
/// ```
#[derive(Debug)]
pub struct SessionBuilder {
    inputs: usize,
    outputs_floor: usize,
    root: Option<ChannelTopology>,
    render_workers: usize,
    render_worker_start: Option<crate::RenderWorkerStart>,
    #[cfg(feature = "midi-hardware")]
    midi_hardware: bool,
    /// The backend the OS MIDI layer runs over, if not the platform's.
    #[cfg(feature = "midi-hardware")]
    midi_backend: Option<MidiBackend>,
    #[cfg(feature = "midi")]
    mpe_mode: tutti_midi_protocol::MpeMode,
    #[cfg(feature = "midi")]
    midi_ports: tutti_midi_runtime::MidiInputPorts,
    #[cfg(feature = "modulation")]
    unresolved_warning: std::time::Duration,
    #[cfg(feature = "plugin")]
    snapshot_interval: std::time::Duration,
    #[cfg(feature = "plugin")]
    recovery: Option<crate::RecoveryPolicy>,
}

/// A MIDI backend a host supplies, held until the session takes it.
#[cfg(feature = "midi-hardware")]
struct MidiBackend(Box<dyn tutti_midi_hardware::MidiEndpoints>);

#[cfg(feature = "midi-hardware")]
impl std::fmt::Debug for MidiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiBackend").finish_non_exhaustive()
    }
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            inputs: 0,
            outputs_floor: 2,
            root: None,
            render_workers: 1,
            render_worker_start: None,
            #[cfg(feature = "midi-hardware")]
            midi_hardware: false,
            #[cfg(feature = "midi-hardware")]
            midi_backend: None,
            #[cfg(feature = "midi")]
            mpe_mode: tutti_midi_protocol::MpeMode::Disabled,
            #[cfg(feature = "midi")]
            midi_ports: tutti_midi_runtime::MidiInputPorts::FIRST_GROUP,
            #[cfg(feature = "modulation")]
            unresolved_warning: crate::UNRESOLVED_WARNING,
            #[cfg(feature = "plugin")]
            snapshot_interval: crate::SNAPSHOT_INTERVAL,
            #[cfg(feature = "plugin")]
            recovery: Some(crate::RecoveryPolicy::default()),
        }
    }
}

/// What every build path shares: the graph, the transport, the click, the
/// MIDI nodes, and what the engine's callback state holds.
struct Parts {
    graph: Graph,
    transport: Transport,
    click: Option<NodeKey>,
    click_settings: Arc<ClickSettings>,
    compensation: Arc<PrerollTable>,
    #[cfg(feature = "midi")]
    midi: Option<crate::MidiNodes>,
    /// The reader of the tap the hardware out drains.
    #[cfg(feature = "midi")]
    midi_tap: Option<tutti_graph::EventTapReader>,
    #[cfg(feature = "midi-hardware")]
    midi_io: Option<tutti_midi_hardware::MidiSession>,
    #[cfg(feature = "midi-hardware")]
    device_events: Option<tutti_midi_hardware::DeviceEvents>,
}

impl SessionBuilder {
    /// Global graph inputs (hardware or host input channels). Default 0.
    ///
    /// A count above `u16::MAX` is refused when the session is built
    /// ([`FoldError::TooWide`](tutti_types::FoldError::TooWide)).
    #[must_use]
    pub fn inputs(mut self, inputs: usize) -> Self {
        self.inputs = inputs;
        self
    }

    /// The project's output width: the graph root is at least this wide, and
    /// at least the device's ([`root_width`]). Default 2.
    #[must_use]
    pub fn outputs_floor(mut self, outputs: usize) -> Self {
        self.outputs_floor = outputs;
        self
    }

    /// Declares the graph root's channels from the start
    /// ([`Session::declare_root_topology`]): the root is exactly as wide as
    /// `root`, whatever [`outputs_floor`](Self::outputs_floor) and the device
    /// say, and the build refuses a device that cannot place it
    /// ([`Error::Output`](crate::Error::Output)). Default none: the root is the engine's order for
    /// its width.
    #[must_use]
    pub fn root_topology(mut self, root: ChannelTopology) -> Self {
        self.root = Some(root);
        self
    }

    /// Threads per block, the callback included: `0` for one per core,
    /// `1` (the default) to render on the callback alone. Each export gets
    /// its own pool of the same size.
    #[must_use]
    pub fn render_workers(mut self, workers: usize) -> Self {
        self.render_workers = workers;
        self
    }

    /// A hook each helper of the live render pool runs once on its own
    /// thread, with its index (`1..`, the callback is 0), before it takes
    /// work. A host that promotes its audio callback (`SCHED_FIFO`, MMCSS, an
    /// audio workgroup) gives the helpers the same priority here: helpers
    /// below it can stall a block, helpers above it preempt the callback.
    /// Nothing promotes them by default. An export's own pool does not run
    /// it. A hook that panics or blocks behaves as
    /// [`WorkerPoolBuilder::start_handler`](tutti_graph::WorkerPoolBuilder::start_handler)
    /// documents.
    #[must_use]
    pub fn render_worker_start(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.render_worker_start = Some(crate::RenderWorkerStart::new(hook));
        self
    }

    /// Open the OS MIDI layer ([`Session::midi_io`]) and feed the engine's
    /// MIDI input node from its ports. Default off.
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub fn midi_hardware(mut self) -> Self {
        self.midi_hardware = true;
        self
    }

    /// As [`midi_hardware`](Self::midi_hardware), over `backend` instead of the
    /// platform's: a headless test's `FakeBackend`
    /// (`tutti_midi_hardware::test_support`), whose `plug` and `unplug` drive
    /// hot-plug with no device.
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub fn midi_hardware_over(
        mut self,
        backend: Box<dyn tutti_midi_hardware::MidiEndpoints>,
    ) -> Self {
        self.midi_hardware = true;
        self.midi_backend = Some(MidiBackend(backend));
        self
    }

    /// The MPE mode the engine's MIDI input ingests in. Default `Disabled`.
    #[cfg(feature = "midi")]
    #[must_use]
    pub fn mpe_mode(mut self, mode: tutti_midi_protocol::MpeMode) -> Self {
        self.mpe_mode = mode;
        self
    }

    /// The Groups the engine's MIDI input hears, from the first through
    /// `last`. Default the first Group alone, whose seventeen ports are what a
    /// one-Group device needs; each Group heard adds as many event outputs
    /// ([`tutti_midi_runtime::MidiInputPorts`]). An event of a Group past
    /// `last` is not delivered.
    #[cfg(feature = "midi")]
    #[must_use]
    pub fn midi_groups(mut self, last: tutti_ump::MidiGroup) -> Self {
        self.midi_ports = tutti_midi_runtime::MidiInputPorts::through(last);
        self
    }

    /// How long a modulation target may stay unresolved, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now), before it is logged and
    /// reported ([`SessionEvent::ModulationUnresolved`](crate::SessionEvent::ModulationUnresolved)).
    /// Default [`UNRESOLVED_WARNING`](crate::UNRESOLVED_WARNING), 4 s.
    #[cfg(feature = "modulation")]
    #[must_use]
    pub fn unresolved_warning(mut self, after: std::time::Duration) -> Self {
        self.unresolved_warning = after;
        self
    }

    /// How often a healthy hosted plugin's state is captured, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now)
    /// ([`SessionEvent::PluginSnapshotDue`](crate::SessionEvent::PluginSnapshotDue)).
    /// Default [`SNAPSHOT_INTERVAL`](crate::SNAPSHOT_INTERVAL), 8 s.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn snapshot_interval(mut self, every: std::time::Duration) -> Self {
        self.snapshot_interval = every;
        self
    }

    /// What a hosted plugin's death or hang does. Default
    /// `Some(RecoveryPolicy::default())`: the plugin reloads at its node with
    /// its last snapshot. `None`: its node is removed
    /// ([`SessionEvent::PluginDied`](crate::SessionEvent::PluginDied)) and the
    /// host decides, a prompt to the user for instance.
    ///
    /// It covers out-of-process plugins loaded through the session's load
    /// path ([`Session::add_plugin`](crate::Session::add_plugin)). It does not
    /// cover an in-process VST2 plugin (its crash takes the process, and a
    /// hang in it is not observed), nor one a host inserted itself with
    /// `Graph::insert_plugin` or `Graph::host_plugin` (the session does not
    /// know its path): those are removed or left as before.
    #[cfg(feature = "plugin")]
    #[must_use]
    pub fn plugin_recovery(mut self, policy: Option<crate::RecoveryPolicy>) -> Self {
        self.recovery = policy;
        self
    }

    /// Build the graph at `rate` for blocks of up to `max_block`, with the
    /// device's callback `quantum` when known, `outputs` wide; insert the
    /// metronome and the engine's MIDI nodes.
    fn parts(
        &mut self,
        rate: SampleRate,
        max_block: Samples,
        quantum: Option<Samples>,
        outputs: usize,
    ) -> Result<Parts> {
        // Built first so its port manager can feed the graph's MIDI input.
        #[cfg(feature = "midi-hardware")]
        let backend = self.midi_backend.take();
        #[cfg(feature = "midi-hardware")]
        let midi_io = self.midi_hardware.then(|| -> Result<_> {
            // It times each inbound event by turning a wall-clock delta into a
            // frame offset, which takes the device's real rate.
            let ports = Arc::new(tutti_midi_hardware::HardwareMidiInputs::new(
                tutti_midi_hardware::HardwareMidiInputs::DEFAULT_FIFO,
                rate,
            ));
            let io = match backend {
                Some(MidiBackend(backend)) => {
                    tutti_midi_hardware::MidiSession::open_over(backend, ports)?
                }
                None => tutti_midi_hardware::MidiSession::open(ports)?,
            };
            // Subscribed before anything can plug in unseen: `Phase::Poll`
            // is this subscription's only reader.
            let events = io.devices().subscribe();
            Ok((io, events))
        });
        #[cfg(feature = "midi-hardware")]
        let (midi_io, device_events) = midi_io.transpose()?.unzip();

        let transport = Transport::new(rate);
        let mut graph = Graph::new(
            self.input_layout()?,
            outputs,
            rate,
            max_block,
            quantum,
            transport.settings.tempo_reader(),
        );
        graph.set_render_workers(self.render_workers);
        graph.set_render_worker_start(self.render_worker_start.take());
        let click_settings = Arc::new(ClickSettings::new());

        // The metronome. It reads the beat, play and record state from its
        // block's `Env`, and the count-in flag off the transport (read only).
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let (click, _settings) =
            graph.insert(ClickNode::new(&transport, Arc::clone(&click_settings)));

        // The engine's MIDI as graph nodes: the hardware input (translating,
        // and ingesting MPE in the builder's mode), the clock (off until a
        // host enables it), and the event tap on the clock that is the hardware out.
        #[cfg(feature = "midi")]
        let midi = {
            // The node owns its drain: the read state on the port set, built
            // here on the control thread.
            #[cfg(feature = "midi-hardware")]
            let wire: Option<Box<dyn tutti_midi_runtime::MidiIn>> = midi_io.as_ref().map(|io| {
                Box::new(tutti_midi_hardware::HardwareMidiDrain::new(Arc::clone(
                    io.ports(),
                ))) as Box<dyn tutti_midi_runtime::MidiIn>
            });
            #[cfg(not(feature = "midi-hardware"))]
            let wire: Option<Box<dyn tutti_midi_runtime::MidiIn>> = None;
            let (input, input_controls) = graph.add(
                tutti_midi_runtime::MidiInputNode::new(wire)
                    .hearing(self.midi_ports)
                    .with_mpe(self.mpe_mode),
            );
            let (clock, clock_master) = graph.insert(tutti_midi_runtime::ClockNode::new());
            // The hardware out: a tap on the clock's event output, aligned to
            // the outputs' time so MIDI leaves with the audio (doc 017, E3).
            let tap = graph
                .open_event_tap(
                    clock.into(),
                    tutti_graph::TapAlign::ToOutput,
                    crate::midi::MIDI_OUT_TAP_CAPACITY,
                )
                .expect("the clock node is in the graph with its event output");
            (
                Some(crate::MidiNodes {
                    input,
                    input_ports: self.midi_ports,
                    input_controls,
                    clock,
                    clock_master,
                }),
                Some(tap),
            )
        };
        #[cfg(feature = "midi")]
        let (midi, midi_tap) = midi;

        Ok(Parts {
            graph,
            transport,
            click: Some(click),
            click_settings,
            compensation: Arc::default(),
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi")]
            midi_tap,
            #[cfg(feature = "midi-hardware")]
            midi_io,
            #[cfg(feature = "midi-hardware")]
            device_events,
        })
    }

    /// The session over `parts`, `engine` its offline engine if any, with
    /// (`sampler`) a disk streamer when `streamer`.
    fn session(
        &self,
        parts: Parts,
        engine: Option<tutti_engine::Engine>,
        streamer: bool,
    ) -> Result<Session> {
        #[cfg(feature = "sampler")]
        let disk_streamer = if streamer {
            Some(tutti_sampler::DiskStreamer::new(
                parts.graph.prepared().sample_rate(),
                tutti_sampler::DiskStreamerConfig::default(),
            )?)
        } else {
            None
        };
        #[cfg(not(feature = "sampler"))]
        let _ = streamer;
        Ok(Session {
            graph: parts.graph,
            transport: parts.transport,
            load: engine.as_ref().map(|e| e.load_meter().clone()),
            heard: None,
            root_request: self.root.clone(),
            root_refusal: None,
            engine,
            click: parts.click,
            click_settings: parts.click_settings,
            compensation: parts.compensation,
            latency: Samples(0),
            resumed: false,
            silenced_seen: 0,
            #[cfg(feature = "midi")]
            midi: parts.midi,
            #[cfg(all(feature = "midi", not(feature = "midi-hardware")))]
            midi_state: crate::midi::MidiState::with_hardware_tap(parts.midi_tap),
            #[cfg(feature = "midi-hardware")]
            midi_state: crate::midi::MidiState::with_device_events(
                parts.device_events,
                parts.midi_tap,
            ),
            #[cfg(feature = "modulation")]
            mods: crate::modulation::ModSession::new(self.unresolved_warning),
            #[cfg(feature = "midi-hardware")]
            midi_io: parts.midi_io,
            #[cfg(feature = "sampler")]
            disk_streamer,
            #[cfg(feature = "device")]
            faults: None,
            #[cfg(feature = "device")]
            device_watch: None,
            #[cfg(feature = "device")]
            device_events: Vec::new(),
            #[cfg(feature = "device")]
            output_listing: None,
            #[cfg(feature = "device")]
            outputs_listed: false,
            #[cfg(feature = "plugin")]
            snapshot_interval: self.snapshot_interval,
            #[cfg(feature = "plugin")]
            recovery: self.recovery,
            #[cfg(feature = "plugin")]
            plugins_now: std::time::Instant::now(),
            #[cfg(feature = "plugin")]
            render_mode: Default::default(),
            #[cfg(feature = "export")]
            bounce: Default::default(),
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
    /// When the engine refuses the graph, `layout` has no channels or cannot take
    /// the root ([`Error::Output`](crate::Error::Output)),
    /// (`midi-hardware`) the MIDI device watch cannot start, or (`sampler`) the disk streamer cannot start.
    pub fn build_offline(
        mut self,
        rate: SampleRate,
        layout: ChannelLayout,
        max_block: Samples,
    ) -> Result<Session> {
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let mut parts = self.parts(rate, max_block, None, root)?;
        let engine = parts.graph.engine(&parts.transport)?;
        // The width of every block the host pulls.
        engine.support_output(layout)?;
        self.declare(&mut parts.graph, layout)?;
        let heard = engine.heard_taps();
        let mut session = self.session(parts, Some(engine), true)?;
        session.heard = Some((heard, layout));
        Ok(session)
    }

    /// A session with no engine: the executor stays on this side, every
    /// commit is applied at once, and [`Session::render_frame`] renders it
    /// (or [`Graph::take_audio_side`] hands it to a test). No metronome and
    /// no MIDI nodes are inserted, and no disk streamer is started: the
    /// graph is exactly `outputs_floor` wide (a declared
    /// [`root_topology`](Self::root_topology)'s width, if there is one), empty,
    /// at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declared root topology is empty or wider
    /// than the engine renders; there is no device to place it on.
    pub fn build_detached(self, rate: SampleRate) -> Result<Session> {
        let outputs = self
            .root
            .as_ref()
            .map_or(self.outputs_floor, |r| r.positions().len());
        let transport = Transport::new(rate);
        let mut graph = Graph::new(
            self.input_layout()?,
            outputs,
            rate,
            LIVE_MAX_BLOCK,
            None,
            transport.settings.tempo_reader(),
        );
        graph.set_render_workers(self.render_workers);
        graph.declare_root(self.root.clone(), None)?;
        self.session(
            Parts {
                graph,
                transport,
                click: None,
                click_settings: Arc::new(ClickSettings::new()),
                compensation: Arc::default(),
                #[cfg(feature = "midi")]
                midi: None,
                #[cfg(feature = "midi")]
                midi_tap: None,
                #[cfg(feature = "midi-hardware")]
                midi_io: None,
                #[cfg(feature = "midi-hardware")]
                device_events: None,
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
        mut self,
        mut audio_engine: tutti_cpal::AudioEngine,
        start: impl FnOnce(
            &mut tutti_cpal::AudioEngine,
            Arc<tutti_cpal::AudioCallbackState>,
        ) -> tutti_cpal::Result<()>,
    ) -> Result<(Session, crate::Device)> {
        let spec = audio_engine.spec().clone();
        let root = self.root_width(spec.channels);
        self.check_placement(root, spec.channels)?;
        let mut parts = self.parts(spec.sample_rate, LIVE_MAX_BLOCK, spec.quantum, root)?;
        let engine = parts.graph.engine(&parts.transport)?;
        self.declare(&mut parts.graph, spec.channels)?;
        let mut session = self.session(parts, None, true)?;
        session.load = Some(engine.load_meter().clone());
        session.heard = Some((engine.heard_taps(), spec.channels));
        let state = Arc::new(tutti_cpal::AudioCallbackState::new(engine));
        start(&mut audio_engine, Arc::clone(&state))?;
        let device = crate::Device::new(tutti_cpal::TuttiDriver::new(audio_engine, state));
        session.faults = Some((device.faults(), device.faults().count()));
        Ok((session, device))
    }
}

impl SessionBuilder {
    /// The root's width for `device`: the declared topology's, or else
    /// [`root_width`] of the floor.
    /// The global input count as a layout, or the refusal of a count none holds.
    fn input_layout(&self) -> Result<ChannelLayout> {
        Ok(ChannelLayout::try_from(self.inputs)
            .map_err(|_| tutti_types::FoldError::TooWide(self.inputs))?)
    }

    fn root_width(&self, device: ChannelLayout) -> usize {
        self.root.as_ref().map_or_else(
            || root_width(self.outputs_floor, device),
            |root| root.positions().len(),
        )
    }

    /// Refuses a `(root, device)` pair whose root has a channel the device has
    /// no place for, before anything is built: the engine would render it
    /// silent. A declared root is placed from its topology (and refused if it
    /// is empty or wider than the engine renders); an undeclared one is a bare
    /// width.
    ///
    /// The root is at least as wide as the device up to [`MAX_ROOT_CHANNELS`],
    /// so what this refuses of an undeclared root is a project wider than the
    /// device where either width has no channel order (a 6-channel project on
    /// a 5-channel interface).
    fn check_placement(&self, root: usize, device: ChannelLayout) -> Result<()> {
        match &self.root {
            // The same check `Graph::declare_root` makes a step later; made
            // here so a refused root builds nothing (no engine, no device
            // thread). Removing this arm alone changes no outcome, only
            // how much is built before the refusal.
            Some(declared) => Graph::check_root(declared, device)?,
            None => {
                let width = ChannelLayout::try_from(root)
                    .map_err(|_| tutti_types::FoldError::TooWide(root))?;
                tutti_types::Fold::widths(width, device)?;
            }
        }
        Ok(())
    }

    /// Declares the builder's root on the graph the engine was just built
    /// over, which publishes it to the engine.
    fn declare(&self, graph: &mut Graph, device: ChannelLayout) -> Result<()> {
        if self.root.is_some() {
            graph.declare_root(self.root.clone(), Some(device))?;
        }
        Ok(())
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
/// the extra device channels permanently silent: the fold zero-fills
/// rather than upmixing, so nothing downstream can recover them.
///
/// The [`MAX_ROOT_CHANNELS`] clamp is applied here, at construction:
/// `Engine::new` bounds the editor to that many global outputs, so a wider
/// root would be refused at its first commit and never play. An export's
/// fork is prepared on its own, with no such cap.
pub fn root_width(project: usize, device: ChannelLayout) -> usize {
    project
        .max(usize::from(device.count()))
        .clamp(1, MAX_ROOT_CHANNELS)
}
