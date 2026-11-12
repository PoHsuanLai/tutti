//! [`SessionBuilder`]: one ordered, fallible build of a graph, its engine and
//! the nodes the engine inserts, offline, on a device, or detached.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tutti_graph::MAX_ROOT_CHANNELS;
use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_nodes::{ClickNode, ClickSettings};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, ChannelTopology, MaxBlock, SampleRate, Samples};

use crate::graph::{Graph, LIVE_MAX_BLOCK};
use crate::{Result, Session};

/// Builds a [`Session`].
///
/// ```
/// use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};
/// use tutti_session::{Session, Workers};
///
/// let session = Session::builder()
///     .outputs_floor(ChannelLayout::from_count(6)) // a 5.1 project renders six, whatever the device
///     .render_workers(Workers::SERIAL)
///     .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, MaxBlock::of(Samples(512)))
///     .unwrap();
/// assert_eq!(session.graph_ref().outputs(), 6);
/// ```
#[derive(Debug)]
pub struct SessionBuilder {
    inputs: ChannelLayout,
    outputs_floor: ChannelLayout,
    root: Option<ChannelTopology>,
    render_workers: Workers,
    /// The platform host the output and every input the session opens use.
    #[cfg(feature = "device")]
    host: tutti_cpal::AudioHost,
    /// Where the session's inputs open, if not the sound card
    /// ([`input_opener`](Self::input_opener)).
    #[cfg(all(feature = "device", feature = "audio-io"))]
    input_opener: Option<Box<dyn crate::InputOpener>>,
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
    /// How many hardware MIDI devices may be connected at once.
    #[cfg(feature = "midi")]
    midi_devices: usize,
    #[cfg(feature = "modulation")]
    unresolved_warning: std::time::Duration,
    #[cfg(feature = "plugin")]
    snapshot_interval: std::time::Duration,
    #[cfg(feature = "plugin")]
    recovery: Option<crate::RecoveryPolicy>,
    spawner: crate::jobs::Spawner,
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
            inputs: ChannelLayout::EMPTY,
            outputs_floor: ChannelLayout::STEREO,
            root: None,
            render_workers: Workers::SERIAL,
            #[cfg(feature = "device")]
            host: tutti_cpal::AudioHost::Default,
            #[cfg(all(feature = "device", feature = "audio-io"))]
            input_opener: None,
            render_worker_start: None,
            #[cfg(feature = "midi-hardware")]
            midi_hardware: false,
            #[cfg(feature = "midi-hardware")]
            midi_backend: None,
            #[cfg(feature = "midi")]
            mpe_mode: tutti_midi_protocol::MpeMode::Disabled,
            #[cfg(feature = "midi")]
            midi_ports: tutti_midi_runtime::MidiInputPorts::FIRST_GROUP,
            #[cfg(feature = "midi")]
            midi_devices: crate::MIDI_INPUT_DEVICES,
            #[cfg(feature = "modulation")]
            unresolved_warning: crate::UNRESOLVED_WARNING,
            #[cfg(feature = "plugin")]
            snapshot_interval: crate::SNAPSHOT_INTERVAL,
            #[cfg(feature = "plugin")]
            recovery: Some(crate::RecoveryPolicy::default()),
            spawner: crate::jobs::Spawner::default(),
        }
    }
}

/// How many threads render a block, the caller's included
/// ([`SessionBuilder::render_workers`]).
///
/// Serial is [`SERIAL`](Self::SERIAL), a constant and the default: there is
/// no separate variant for it, so there is one spelling of "the callback
/// alone" (`Exactly(1)` *is* it). A pool of more threads renders every block
/// bit-identically to the serial one; it pays when the graph is wide and its
/// nodes heavy.
///
/// ```
/// use tutti_session::Workers;
///
/// assert_eq!(Workers::default(), Workers::SERIAL);
/// assert_eq!(Workers::of(4).threads(), 4);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Workers {
    /// One thread for each core the OS reports (one, when it reports none).
    PerCore,
    /// This many threads. One renders on the caller alone.
    Exactly(NonZeroUsize),
}

impl Workers {
    /// The callback alone: the default.
    pub const SERIAL: Self = Self::Exactly(NonZeroUsize::MIN);

    /// `threads` threads, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `threads` is zero.
    #[must_use]
    pub const fn of(threads: usize) -> Self {
        match NonZeroUsize::new(threads) {
            Some(n) => Self::Exactly(n),
            None => panic!("a render pool has at least one thread: the caller's"),
        }
    }

    /// The thread count this asks for, resolved now (`PerCore` reads the
    /// OS's figure).
    #[must_use]
    pub fn threads(self) -> usize {
        match self {
            Self::PerCore => std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
            Self::Exactly(n) => n.get(),
        }
    }
}

impl Default for Workers {
    fn default() -> Self {
        Self::SERIAL
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
    /// Global graph inputs (hardware or host input channels). Default none
    /// ([`ChannelLayout::EMPTY`]).
    #[must_use]
    pub fn inputs(mut self, inputs: ChannelLayout) -> Self {
        self.inputs = inputs;
        self
    }

    /// The platform audio host the output device and every input the session
    /// opens (`Session::add_input`, feature `audio-io`) go through.
    /// Default [`AudioHost::Default`](tutti_cpal::AudioHost::Default); asking for
    /// one this build cannot reach (JACK without the `jack` feature, or with
    /// no server running) is refused when the device opens, as
    /// [`Error::Device`](crate::Error::Device) with
    /// `HostUnavailable`.
    #[cfg(feature = "device")]
    #[must_use]
    pub fn host(mut self, host: tutti_cpal::AudioHost) -> Self {
        self.host = host;
        self
    }

    /// The project's output width: the graph root is at least this wide, and
    /// at least the device's ([`root_width`]). Default stereo.
    #[must_use]
    pub fn outputs_floor(mut self, outputs: ChannelLayout) -> Self {
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

    /// Threads per block, the callback included ([`Workers`]): the callback
    /// alone by default. Each export gets its own pool of the same size.
    #[must_use]
    pub fn render_workers(mut self, workers: Workers) -> Self {
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

    /// Opens the session's inputs ([`Session::add_input`]) through `opener`
    /// instead of the sound card: a test's own capture, with no device.
    #[cfg(all(feature = "device", feature = "audio-io", feature = "testing"))]
    #[must_use]
    pub fn input_opener(mut self, opener: impl crate::InputOpener + 'static) -> Self {
        self.input_opener = Some(Box::new(opener));
        self
    }

    /// The spawner the session runs its blocking work on (design 029 §3.4):
    /// a file load, a plugin round trip, a recording's pump. It is handed
    /// each [`Job`](crate::Job) and calls [`Job::run`](crate::Job::run) on a
    /// thread of its own (`tokio::task::spawn_blocking`, a rayon pool); what
    /// the job reports lands in [`Session::update`]. The default is a named
    /// `std::thread` per job. A job the spawner drops unrun ends as failed,
    /// so a spawner that refuses work loses none silently.
    ///
    /// A recording take (`Session::record`, feature `audio-io`) holds one job for the whole take:
    /// over a bounded pool with no worker free it does not start until one is.
    ///
    /// ```
    /// # use tutti_session::Session;
    /// let builder = Session::builder().spawn_with(|job| {
    ///     std::thread::spawn(move || job.run());
    /// });
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn spawn_with(mut self, spawn: impl Fn(crate::Job) + Send + Sync + 'static) -> Self {
        self.spawner = crate::jobs::Spawner::new(spawn);
        self
    }

    /// Open the OS MIDI layer (reached through [`Session::midi`](crate::Session::midi): `connect_input`, `connect_output`, `devices`) and feed the engine's
    /// MIDI ingest node, an event inlet for each connected device. Default off.
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

    /// The session's MPE mode to begin with: what the engine's MIDI input
    /// ingests in, and what a synth added through
    /// [`Session::add_instrument`](crate::Session::add_instrument) is built
    /// to follow ([`Midi::set_mpe`](crate::Midi::set_mpe) changes it later).
    /// Default `Disabled`.
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

    /// How many hardware MIDI devices may be connected at once: the ingest
    /// node has one event input, and each device one event inlet and one
    /// translator state, for each ([`MIDI_INPUT_DEVICES`](crate::MIDI_INPUT_DEVICES)
    /// by default). Connecting one past it is refused
    /// (`tutti_midi_hardware::Error::NoFreeSlot`). At least one.
    #[cfg(feature = "midi")]
    #[must_use]
    pub fn midi_devices(mut self, devices: usize) -> Self {
        self.midi_devices = devices.max(1);
        self
    }

    /// How long a modulation target may stay unresolved, measured on
    /// [`PhaseCx::now`](crate::PhaseCx::now), before it is logged and
    /// reported ([`SessionEvent::Modulation`](crate::SessionEvent::Modulation)).
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
        max_block: MaxBlock,
        quantum: Option<MaxBlock>,
        outputs: usize,
    ) -> Result<Parts> {
        // Built first so its port list can feed the graph's MIDI input.
        #[cfg(feature = "midi-hardware")]
        let backend = self.midi_backend.take();
        #[cfg(feature = "midi-hardware")]
        let midi_io = self.midi_hardware.then(|| -> Result<_> {
            // One event inlet for each device, which its thread pushes into
            // for the frame the engine's block clock puts its arrival on: a
            // block later, so no event is late (the lead, doc 017 E5).
            let inlets = tutti_midi_runtime::DeviceInlets::new(
                self.midi_devices,
                crate::midi::MIDI_INPUT_INLET_CAPACITY,
                crate::midi::input_lead(quantum.unwrap_or(max_block).samples(), rate),
            );
            let io = match backend {
                Some(MidiBackend(backend)) => {
                    tutti_midi_hardware::MidiSession::open_over(backend, inlets)?
                }
                None => tutti_midi_hardware::MidiSession::open(inlets)?,
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
            self.inputs,
            outputs,
            rate,
            max_block,
            quantum,
            transport.settings().tempo_reader(),
        );
        graph.set_render_workers(self.render_workers);
        graph.set_render_worker_start(self.render_worker_start.take());
        let click_settings = Arc::new(ClickSettings::new());

        // The metronome. It reads the beat, play and record state from its
        // block's `Env`, and the count-in flag off the transport (read only).
        // It is not wired to the output: what it feeds is the host's
        // declaration, like every other node's.
        let (click, _settings) = graph.add(ClickNode::new(&transport, Arc::clone(&click_settings)));

        // The engine's MIDI as graph nodes: the hardware input's ingest (translating,
        // and ingesting MPE in the builder's mode), the clock (off until a
        // host enables it), and the event tap on the clock that is the hardware out.
        #[cfg(feature = "midi")]
        let midi = {
            // One event input for each device slot; the session wires a
            // device's inlet to its slot as the device connects.
            let ingest = tutti_midi_runtime::MidiIngestNode::new(self.midi_devices)
                .hearing(self.midi_ports)
                .with_mpe(self.mpe_mode);
            // A slot a new device takes starts over in the node, with no call
            // from here: it follows the inlets' epochs. Both are built from
            // `midi_devices`, so their slot counts agree.
            #[cfg(feature = "midi-hardware")]
            let ingest = match &midi_io {
                Some(io) => ingest
                    .following(io.ports().inlets())
                    .expect("the ingest node and the inlets are built with one slot count"),
                None => ingest,
            };
            let (input, input_controls) = graph.add(ingest);
            let (clock, clock_master) = graph.add(tutti_midi_runtime::ClockNode::new(
                tutti_ump::MidiGroup::FIRST,
                rate,
            ));
            // The hardware out: a tap on the clock's event output, aligned to
            // the outputs' time so MIDI leaves with the audio (doc 017, E3).
            let tap = graph
                .taps()
                .open_event(
                    clock.events_out(),
                    tutti_graph::TapAlign::ToOutput,
                    crate::midi::MIDI_OUT_TAP_CAPACITY,
                )
                .expect("the clock node is in the graph with its event output");
            (
                Some(crate::MidiNodes {
                    input: input.erase(),
                    input_ports: self.midi_ports,
                    input_devices: self.midi_devices,
                    input_controls,
                    clock: clock.erase(),
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
            click: Some(click.erase()),
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
        &mut self,
        parts: Parts,
        mut engine: Option<tutti_engine::Engine>,
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
        let load = engine
            .as_mut()
            .and_then(tutti_engine::Engine::take_load_meter);
        Ok(Session {
            graph: parts.graph,
            transport: parts.transport,
            load,
            heard: None,
            clock: engine.as_ref().map(tutti_engine::Engine::block_clock),
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
            midi_state: crate::midi::MidiState::with_hardware_tap(parts.midi_tap)
                .with_mpe(self.mpe_mode),
            #[cfg(feature = "midi-hardware")]
            midi_state: crate::midi::MidiState::with_device_events(
                parts.device_events,
                parts.midi_tap,
            )
            .with_mpe(self.mpe_mode),
            #[cfg(feature = "modulation")]
            mods: crate::modulation::ModSession::new(self.unresolved_warning),
            #[cfg(feature = "midi-hardware")]
            midi_io: parts.midi_io,
            #[cfg(feature = "sampler")]
            disk_streamer,
            #[cfg(feature = "sampler")]
            sampler_opens: 0,
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
            #[cfg(all(feature = "device", feature = "audio-io"))]
            inputs: crate::input::Inputs::new(self.host, self.input_opener.take()),
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
            jobs: crate::jobs::Jobs::new(self.spawner.clone()),
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
        max_block: MaxBlock,
    ) -> Result<Session> {
        let root = self.root_width(layout);
        self.check_placement(root, layout)?;
        let mut parts = self.parts(rate, max_block, None, root)?;
        let engine = parts.graph.engine(&parts.transport)?;
        Self::clock_midi(&parts, &engine);
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
    /// (or `Graph::take_audio_side`, behind the `testing` feature, hands it to a test). No metronome and
    /// no MIDI nodes are inserted, and no disk streamer is started: the
    /// graph is exactly `outputs_floor` wide (a declared
    /// [`root_topology`](Self::root_topology)'s width, if there is one), empty,
    /// at `rate`.
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declared root topology is empty or wider
    /// than the engine renders; there is no device to place it on.
    pub fn build_detached(mut self, rate: SampleRate) -> Result<Session> {
        let outputs = self.root.as_ref().map_or_else(
            || usize::from(self.outputs_floor.count()),
            |r| r.positions().len(),
        );
        let transport = Transport::new(rate);
        let mut graph = Graph::new(
            self.inputs,
            outputs,
            rate,
            LIVE_MAX_BLOCK,
            None,
            transport.settings().tempo_reader(),
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
    /// live on return. The [`LiveSession`](crate::LiveSession) holds the session and
    /// its [`Device`](crate::Device), which stays on this thread: keep it for
    /// as long as the sound should play.
    ///
    /// # Errors
    ///
    /// When the device cannot be opened or started, or the graph cannot be
    /// built over it.
    #[cfg(feature = "device")]
    pub fn build_device(self, device: tutti_cpal::DeviceSelector) -> Result<crate::LiveSession> {
        let host = tutti_cpal::DeviceHost::open(self.host)?;
        let engine = tutti_cpal::AudioEngine::open(host, device.clone())?;
        self.build_on(engine, device, |engine, state| engine.start(state))
    }

    /// [`build_device`](Self::build_device) on a stream driver of the
    /// caller's choosing, `spec` standing for the config its device reports:
    /// the device-free build (a `tutti_cpal::ManualStreamDriver`, for a test
    /// that renders the callback itself). Behind `testing`: a host builds on a
    /// device.
    ///
    /// # Errors
    ///
    /// When the driver cannot start, or the graph cannot be built.
    #[cfg(all(feature = "device", feature = "testing"))]
    pub fn build_device_on<D>(
        self,
        spec: tutti_cpal::OutputSpec,
        driver: D,
    ) -> Result<crate::LiveSession>
    where
        D: tutti_cpal::StreamDriver,
        D::Running: 'static,
    {
        self.build_on(
            tutti_cpal::AudioEngine::from_spec(spec),
            tutti_cpal::DeviceSelector::Default,
            |engine, state| engine.start_with(state, driver),
        )
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
        chosen: tutti_cpal::DeviceSelector,
        start: impl FnOnce(
            &mut tutti_cpal::AudioEngine,
            Arc<tutti_cpal::AudioCallbackState>,
        ) -> tutti_cpal::Result<()>,
    ) -> Result<crate::LiveSession> {
        let spec = audio_engine.spec().clone();
        let root = self.root_width(spec.channels);
        self.check_placement(root, spec.channels)?;
        let mut parts = self.parts(spec.sample_rate, LIVE_MAX_BLOCK, spec.quantum, root)?;
        let mut engine = parts.graph.engine(&parts.transport)?;
        Self::clock_midi(&parts, &engine);
        self.declare(&mut parts.graph, spec.channels)?;
        let mut session = self.session(parts, None, true)?;
        session.load = engine.take_load_meter();
        session.heard = Some((engine.heard_taps(), spec.channels));
        session.clock = Some(engine.block_clock());
        #[cfg(feature = "audio-io")]
        session.inputs.set_output_latency(audio_engine.latency());
        let state = Arc::new(tutti_cpal::AudioCallbackState::new(engine));
        start(&mut audio_engine, Arc::clone(&state))?;
        #[cfg(feature = "audio-io")]
        let input_latency = session.inputs.latency();
        #[cfg(not(feature = "audio-io"))]
        let input_latency = Arc::default();
        let device = crate::Device::new(
            tutti_cpal::TuttiDriver::new(audio_engine, state),
            chosen,
            input_latency,
        );
        session.faults = Some((device.faults(), device.faults().count()));
        Ok(crate::LiveSession::new(session, device))
    }
}

impl SessionBuilder {
    /// Gives the hardware MIDI devices' threads the engine's block clock, so
    /// each places its arrivals on frames.
    fn clock_midi(parts: &Parts, engine: &tutti_engine::Engine) {
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &parts.midi_io {
            io.ports()
                .inlets()
                .set_clock(Some(crate::midi::arrival_clock(engine.block_clock())));
        }
        #[cfg(not(feature = "midi-hardware"))]
        let _ = (parts, engine);
    }

    /// The root's width for `device`: the declared topology's, or else
    /// [`root_width`] of the floor.
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
pub fn root_width(project: ChannelLayout, device: ChannelLayout) -> usize {
    usize::from(project.count())
        .max(usize::from(device.count()))
        .clamp(1, MAX_ROOT_CHANNELS)
}
