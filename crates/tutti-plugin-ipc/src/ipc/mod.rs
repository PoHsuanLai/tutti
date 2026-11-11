//! [`Ipc`]: a plugin served by a `plugin-server` process, as a
//! [`Loaded`] (design 021 step 4b, PR 3a).
//!
//! It wraps what a served plugin is today: the [`PluginBridge`] (control
//! socket and audio slab), the [`ProcessGuard`] that owns the server, the
//! [`EditorLoader`] the host passed, and the [`LoadedPlugin`] descriptor the
//! server reported. [`activate`](Loaded::activate) splits it into an
//! [`IpcProcessor`] (the [`Batcher`] behind
//! [`Processor::process`](tutti_plugin_types::Processor::process)) and an [`IpcController`] (the bridge's control
//! methods behind [`Control`](tutti_plugin_types::Control)).
//!
//! # No format parameter
//!
//! One `Ipc` serves every format by path, as `PluginClient` does: the
//! `plugin-server` loads whichever format the path names, and its AU loader is
//! still its own (design 021 §10.4 (c)). It gains `<F: OutOfProcess>` when
//! step 7 replaces that loader.
//!
//! # Threads
//!
//! The served control is **thread-agnostic**: the bridge answers a parameter
//! read, a state save or a render-mode write from any thread. [`Control`](tutti_plugin_types::Control)'s
//! methods take a [`MainThread`] because the
//! trait is the in-process one, and here the token is ignored, never required:
//! a caller with no token reaches the same bridge through
//! [`Ipc::bridge`] / [`IpcController::bridge`]. Nothing in this module hops to
//! the main thread. A rate, a block ceiling and a quantum are taken without a
//! restart through [`Processor::retune`](tutti_plugin_types::Processor::retune), which is what lets a graph node
//! prepare a served plugin off the main thread.
//!
//! # Reopening
//!
//! [`Reopen::reopen`] takes a token it does not need and gives a fresh,
//! stateless instance; [`Ipc::reopen_restoring`] is the same without the
//! token and with the state restored before the plugin is activated, which is
//! what a fork built on a worker thread needs (`ForkBuild::deferred`). The
//! offline wait of an export's fork is [`Ipc::watch`].

mod control;
mod processor;
#[cfg(test)]
mod tests;

pub use processor::IpcProcessor;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tutti_plugin_types::{
    BridgeError, BridgeResult, ChunkCounters, Controller, LoadStage, Loaded, LoadedPlugin,
    MainThread, ParameterChanges, ParameterQueue, PluginDescriptor, PluginError, PluginResult,
    PluginTail, Prepare, Refused, Reopen, Request, RestartRules, SampleFormat, SampleRate, Samples,
    Stuck,
};

use crate::{
    Batcher, BridgeConfig, BridgeEvent, BridgeListener, EditorLoader, OfflineWait, PluginBridge,
    ProcessGuard, SlabWriter, launch, unique_socket_path,
};

/// Where a served plugin came from: what [`Ipc::reopen_restoring`] launches
/// again.
#[derive(Clone)]
pub struct IpcOrigin {
    /// The launch's configuration. A reopen gives it a socket of its own.
    pub config: BridgeConfig,
    /// The plugin file.
    pub path: PathBuf,
    /// Loads the editor-only instance, as the first launch's did.
    pub load_editor: EditorLoader,
}

impl IpcOrigin {
    /// A fresh instance of the plugin, from the same file and configuration
    /// in a server of its own (its own socket), at `rate`, with `state`
    /// restored before it is activated. See [`Ipc::reopen_restoring`].
    ///
    /// # Errors
    ///
    /// As [`Ipc::launch`].
    pub fn reopen(&self, rate: SampleRate, state: Option<&[u8]>) -> BridgeResult<Launched> {
        let config = BridgeConfig {
            // Its own rendezvous: two bridges on one socket path is the
            // collision `unique_socket_path` exists to prevent.
            socket_path: unique_socket_path(),
            ..self.config.clone()
        };
        Ipc::launch(config, self.path.clone(), rate, state, self.load_editor)
    }
}

impl std::fmt::Debug for IpcOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcOrigin")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// What a served plugin is, once its server is running and its bridge is up:
/// the input of [`Ipc::new`].
pub struct IpcParts {
    /// The control socket and audio slab, as the client side.
    pub bridge: Arc<PluginBridge>,
    /// The server's lifetime: it dies when the last holder drops.
    pub guard: Arc<ProcessGuard>,
    /// The plugin's catalog identity.
    pub descriptor: PluginDescriptor,
    /// What the server reported at load.
    pub loaded: LoadedPlugin,
    /// The format the blocks travel in.
    pub format: SampleFormat,
    /// The slab's per-chunk size: the most a chunk can be.
    pub ceiling: usize,
    /// The slab's input writer.
    pub slab: SlabWriter,
    /// The rate the server was launched at.
    pub rate: SampleRate,
    /// How many parameters the plugin lists: what the bridge's parameter
    /// buffers are sized by.
    pub parameters: usize,
    /// The per-chunk wait of an offline fork (`BridgeConfig::timeout_ms`).
    pub timeout: Duration,
    /// Where it came from, or `None` for one that cannot be reopened.
    pub origin: Option<IpcOrigin>,
}

impl std::fmt::Debug for IpcParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcParts")
            .field("descriptor", &self.descriptor)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

/// What the bridge's thread reports, kept for the control side: the figures
/// the plugin re-reports, and the requests that follow from them.
struct Events {
    latency: AtomicUsize,
    tail: Mutex<PluginTail>,
    requests: Mutex<Vec<Request>>,
    forward: Mutex<Option<BridgeListener>>,
}

impl Events {
    fn on(&self, event: BridgeEvent) {
        match &event {
            BridgeEvent::LatencyChanged { samples } => {
                self.latency.store(samples.get(), Ordering::Release);
                self.ask(Request::LatencyChanged);
            }
            BridgeEvent::TailChanged { tail } => {
                *self.tail.lock().unwrap_or_else(PoisonError::into_inner) = *tail;
                self.ask(Request::TailChanged);
            }
            _ => {}
        }
        let forward = self
            .forward
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(forward) = forward {
            forward(event);
        }
    }

    fn ask(&self, request: Request) {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
    }
}

/// What the three halves of a served plugin share.
pub(crate) struct Link {
    pub(crate) bridge: Arc<PluginBridge>,
    pub(crate) guard: Arc<ProcessGuard>,
    pub(crate) descriptor: PluginDescriptor,
    pub(crate) loaded: LoadedPlugin,
    pub(crate) format: SampleFormat,
    pub(crate) ceiling: usize,
    pub(crate) parameters: usize,
    pub(crate) timeout: Duration,
    pub(crate) origin: Option<IpcOrigin>,
    pub(crate) counters: Arc<ChunkCounters>,
    events: Arc<Events>,
    /// The rate the server runs at, as bits.
    rate: AtomicU64,
    /// A fork's watch: the latches it reads back, and whether the pipeline
    /// waits for each chunk (an offline fork).
    wait: Mutex<Option<Watch>>,
}

impl Link {
    /// The latency the plugin reports now.
    pub(crate) fn latency(&self) -> Samples {
        Samples(self.events.latency.load(Ordering::Acquire))
    }

    pub(crate) fn tail(&self) -> PluginTail {
        *self
            .events
            .tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn rate(&self) -> SampleRate {
        SampleRate(f64::from_bits(self.rate.load(Ordering::Acquire)))
    }

    pub(crate) fn set_rate(&self, rate: SampleRate) {
        self.rate.store(rate.get().to_bits(), Ordering::Release);
    }

    pub(crate) fn watch(&self) -> Option<Watch> {
        self.wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The queues and points of the bridge's parameter buffers for chunks of
    /// `chunk` frames: a queue per listed parameter and the spares the
    /// node's delivery may learn an address into.
    pub(crate) fn param_room(&self, chunk: usize) -> (usize, usize) {
        (
            self.parameters + ParameterChanges::SPARE_QUEUES,
            ParameterQueue::chunk_room(chunk),
        )
    }

    /// Reserves the bridge's parameter buffers for chunks of `chunk` frames.
    /// Control thread: allocates.
    pub(crate) fn reserve(&self, chunk: usize) {
        let (queues, points) = self.param_room(chunk);
        self.bridge.reserve_param_changes(queues, points);
    }
}

/// A fork's watch on its pipeline ([`Ipc::watch`]).
#[derive(Clone)]
pub(crate) struct Watch {
    pub(crate) wait: Arc<OfflineWait>,
    /// The pipeline waits for each chunk, up to the wait's budget.
    pub(crate) offline: bool,
}

/// A served plugin that is configured and not rendering.
///
/// See the module docs. Built by [`Ipc::launch`] (a new server) or
/// [`Ipc::new`] (a server already running).
pub struct Ipc {
    pub(crate) link: Arc<Link>,
    slab: SlabWriter,
}

impl std::fmt::Debug for Ipc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ipc")
            .field("name", &self.link.descriptor.name)
            .finish_non_exhaustive()
    }
}

/// What a reopen did with the state it was given.
#[derive(Debug)]
pub struct Launched {
    /// The fresh instance, not activated.
    pub ipc: Ipc,
    /// `None` when no state was given, else the plugin's refusal as text (the
    /// plugin is then loaded at its defaults, which beats no plugin).
    pub restored: Option<std::result::Result<(), String>>,
}

impl Ipc {
    /// A served plugin over a running server. Installs the bridge's listener
    /// (the figures the plugin re-reports); a host that wants the events too
    /// forwards them with [`forward_events`](Self::forward_events).
    pub fn new(parts: IpcParts) -> Self {
        let IpcParts {
            bridge,
            guard,
            descriptor,
            loaded,
            format,
            ceiling,
            slab,
            rate,
            parameters,
            timeout,
            origin,
        } = parts;
        let counters = Arc::new(ChunkCounters::default());
        {
            let slab = Arc::clone(bridge.audio_buffer());
            counters.attach(move |seq| slab.has_output(seq));
        }
        let events = Arc::new(Events {
            latency: AtomicUsize::new(loaded.latency_samples.get()),
            tail: Mutex::new(loaded.tail),
            requests: Mutex::new(Vec::new()),
            forward: Mutex::new(None),
        });
        let listening = Arc::clone(&events);
        bridge.set_listener(Some(Arc::new(move |event| listening.on(event))));
        let link = Link {
            bridge,
            guard,
            descriptor,
            loaded,
            format,
            ceiling,
            parameters,
            timeout,
            origin,
            counters,
            events,
            rate: AtomicU64::new(rate.get().to_bits()),
            wait: Mutex::new(None),
        };
        Self {
            link: Arc::new(link),
            slab,
        }
    }

    /// Launches a `plugin-server` for `path` at `rate` and returns the plugin
    /// loaded in it, with what became of `state` if one was given: it is
    /// restored **before the plugin is activated**, the order every format
    /// allows and a plugin that takes its mode from the state at activation
    /// needs.
    ///
    /// Blocks while the server launches and handshakes. Needs no main thread.
    ///
    /// # Errors
    ///
    /// The server cannot be found or spawned, the handshake fails or times
    /// out, the plugin fails to load, or the bridge cannot be set up.
    pub fn launch(
        config: BridgeConfig,
        path: PathBuf,
        rate: SampleRate,
        state: Option<&[u8]>,
        load_editor: EditorLoader,
    ) -> BridgeResult<Launched> {
        let server = launch(&config, &path, rate, state)?;
        let restored = server.state_restore.clone();
        // Guarded before the first fallible step below: an `Err` from
        // `PluginBridge::new` would otherwise drop a bare child process.
        let mut guard = ProcessGuard::launched(server.process, config.clone());
        let (bridge, thread) = PluginBridge::new(
            config.socket_path.clone(),
            Arc::clone(server.slab.slab()),
            path.clone(),
            load_editor,
            rate,
        )?;
        guard.attach(thread);
        // The parameter list, once, while loading blocks anyway: the bridge's
        // buffers are sized by it at activation, which may not make the trip.
        let parameters = bridge.parameters().map_or(0, |list| list.len());
        let ceiling = config.max_buffer_size.min(tutti_plugin_types::MAX_CHUNK);
        let timeout = Duration::from_millis(config.timeout_ms);
        let ipc = Self::new(IpcParts {
            bridge,
            guard: Arc::new(guard),
            descriptor: server.descriptor,
            loaded: server.loaded,
            format: server.format,
            ceiling,
            slab: server.slab,
            rate,
            parameters,
            timeout,
            origin: Some(IpcOrigin {
                config,
                path,
                load_editor,
            }),
        });
        Ok(Launched { ipc, restored })
    }

    /// A fresh instance of this plugin, from the same file and configuration
    /// in a server of its own (its own socket), at the rate this one runs at,
    /// with `state` restored before it is activated. Not activated, and with
    /// none of this one's other state.
    ///
    /// **Token-free**, which [`Reopen::reopen`] cannot be: it blocks for a
    /// server launch and a state load, half a second or more, and needs
    /// nothing of the main thread, so a fork that builds on a worker (the
    /// graph's `ForkBuild::deferred`) calls it there. The state is read from
    /// the live instance on the control thread first
    /// ([`IpcController::save_state`](tutti_plugin_types::Control::save_state), or the bridge's
    /// own).
    ///
    /// # Errors
    ///
    /// This plugin has no origin (it was built by [`Ipc::new`] with none), or
    /// as [`launch`](Self::launch).
    pub fn reopen_restoring(&self, state: Option<&[u8]>) -> BridgeResult<Launched> {
        let Some(origin) = &self.link.origin else {
            return Err(BridgeError::LoadFailed {
                path: PathBuf::new(),
                stage: LoadStage::Opening,
                reason: "this served plugin has no origin to reopen".into(),
            });
        };
        origin.reopen(self.link.rate(), state)
    }

    /// Watches this plugin as a fork: the returned [`OfflineWait`] holds what
    /// the pipeline latches (when `offline`, which makes it wait for each
    /// chunk, up to the configuration's timeout: the offline mode of a forked
    /// instance) and the latency the
    /// fork's graph is planned against. Either way, a retune settles the
    /// server's queued rate and mode changes and records that latency
    /// (`IpcProcessor::retune`), live as offline: the health reports a later
    /// move as a fault. Call before [`activate`](Loaded::activate).
    pub fn watch(&mut self, offline: bool) -> Arc<OfflineWait> {
        let wait = Arc::new(OfflineWait::new(
            self.link.timeout,
            Arc::downgrade(&self.link.guard),
        ));
        *self
            .link
            .wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Watch {
            wait: Arc::clone(&wait),
            offline,
        });
        wait
    }

    /// Sends every event the bridge's thread reports (a latency change, a
    /// parameter the plugin moved, a resync, a refused rate, a crash) to
    /// `listener` as well. Invoked on the bridge thread: cheap, and never
    /// blocking. `None` stops forwarding.
    pub fn forward_events(&self, listener: Option<BridgeListener>) {
        *self
            .link
            .events
            .forward
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = listener;
    }

    /// The bridge: the control socket and audio slab, for a caller that has
    /// no [`MainThread`] to hold (the served control is thread-agnostic).
    pub fn bridge(&self) -> &Arc<PluginBridge> {
        &self.link.bridge
    }

    /// What the plugin is: its catalog identity.
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.link.descriptor
    }

    /// What the server reported at load.
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.link.loaded
    }

    /// Where it came from, for a fork to reopen (`None`: built by
    /// [`Ipc::new`] with none).
    pub fn origin(&self) -> Option<&IpcOrigin> {
        self.link.origin.as_ref()
    }

    /// The slab's per-chunk size: the most a chunk can be.
    pub fn ceiling(&self) -> usize {
        self.link.ceiling
    }

    /// The format the blocks travel in.
    pub fn format(&self) -> SampleFormat {
        self.link.format
    }

    /// The rate the server runs at.
    pub fn rate(&self) -> SampleRate {
        self.link.rate()
    }

    /// The server's lifetime guard.
    pub fn guard(&self) -> &Arc<ProcessGuard> {
        &self.link.guard
    }

    /// Another view of this plugin's control side, for a handle: it shares the
    /// link, so it answers from the same bridge and ends the same server
    /// ([`Health::terminate`](tutti_plugin_types::Health::terminate)).
    pub fn controller(&self) -> IpcController {
        IpcController {
            link: Arc::clone(&self.link),
        }
    }

    /// The pipeline's counters: the chunk in flight and the chunks the server
    /// had not answered. For the node's controls.
    pub fn chunk_counters(&self) -> &Arc<ChunkCounters> {
        &self.link.counters
    }
}

/// The main-thread half of an activated served plugin: the bridge's control
/// methods behind [`Control`](tutti_plugin_types::Control), and the latency and tail it re-reports behind
/// [`Controller`].
#[derive(Clone)]
pub struct IpcController {
    pub(crate) link: Arc<Link>,
}

impl std::fmt::Debug for IpcController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcController")
            .field("name", &self.link.descriptor.name)
            .finish_non_exhaustive()
    }
}

impl IpcController {
    /// The bridge, for a caller that has no [`MainThread`] to hold.
    pub fn bridge(&self) -> &Arc<PluginBridge> {
        &self.link.bridge
    }

    /// The server's OS process id, fixed at launch.
    pub fn server_pid(&self) -> Option<u32> {
        self.link.guard.pid()
    }
}

impl Controller for IpcController {
    type Loaded = Ipc;
    type Processor = IpcProcessor;

    /// The plugin's own latency as it reports it now. The pipeline's chunk,
    /// which out-of-process audio adds (it is submitted now and collected a
    /// chunk later), is the Processor's ([`Processor::latency`](tutti_plugin_types::Processor::latency)): the node
    /// declares both, and that is what turns the chunk into compensated delay.
    fn latency(&self, _main: &MainThread) -> Samples {
        self.link.latency()
    }

    fn tail(&self, _main: &MainThread) -> PluginTail {
        self.link.tail()
    }
}

impl Ipc {
    /// [`Loaded::activate`] with no token: the served activation reads none,
    /// so a node over a served plugin is built on any thread (a fork's
    /// deferred build, a host with no main thread).
    ///
    /// # Errors
    ///
    /// Never, today: the pipeline is built, and a rate is told to the server
    /// by a queued command that cannot be refused here.
    pub fn activate_anywhere(
        self,
        prepare: Prepare,
    ) -> Result<(IpcProcessor, IpcController), Refused<Self>> {
        let Self { link, slab } = self;
        let rate = prepare.rate();
        if rate != link.rate() {
            let _ = link.bridge.set_sample_rate_rt(rate);
            link.set_rate(rate);
        }
        let mut batcher = Batcher::new(
            link.loaded.total_inputs(),
            link.loaded.total_outputs(),
            link.format,
            link.ceiling,
            slab,
        )
        .with_counters(Arc::clone(&link.counters));
        batcher.prepare(Samples(prepare.max_block().get()), None);
        if let Some(Watch {
            wait,
            offline: true,
        }) = link.watch()
        {
            batcher.set_offline_wait(wait);
        }
        link.reserve(batcher.chunk());
        let processor = IpcProcessor::new(Arc::clone(&link), batcher);
        Ok((processor, IpcController { link }))
    }
}

impl Loaded for Ipc {
    type Processor = IpcProcessor;
    type Controller = IpcController;

    /// Nothing restarts a served plugin: a rate, a block and a latency change
    /// are re-read or taken live, as the served path does today.
    const RESTARTS: RestartRules = RestartRules::NONE;

    /// Builds the pipeline for `prepare`'s block and the Processor over it.
    /// The server is told no rate here: it was launched at one, and a node
    /// that follows another tells it through [`Processor::retune`](tutti_plugin_types::Processor::retune).
    fn activate(
        self,
        _main: &MainThread,
        prepare: Prepare,
    ) -> Result<(IpcProcessor, IpcController), Refused<Self>> {
        self.activate_anywhere(prepare)
    }

    fn deactivate(
        _main: &MainThread,
        processor: IpcProcessor,
        controller: IpcController,
    ) -> Result<Self, Stuck<IpcProcessor, IpcController>> {
        if !Arc::ptr_eq(&processor.link, &controller.link) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other(
                    "a processor and a controller of two activations".into(),
                ),
            });
        }
        let slab = processor.into_slab();
        Ok(Self {
            link: controller.link,
            slab,
        })
    }
}

impl Reopen for Ipc {
    /// [`Ipc::reopen_restoring`] with no state. The token is not needed.
    fn reopen(&self, _main: &MainThread) -> PluginResult<Self> {
        self.reopen_restoring(None)
            .map(|reopened| reopened.ipc)
            .map_err(|e| PluginError::Other(e.to_string()))
    }
}
