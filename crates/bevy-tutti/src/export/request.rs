//! What a caller spawns, and what comes back.
//!
//! An export is an **entity**: spawn one carrying [`ExportRequest`], and either
//! observe [`ExportDone`] on it or watch for [`ExportInFlight`] to clear.
//!
//! Everything here is either a Bevy component or a type `tutti-export` already
//! owns — `ExportConfig`, `Written`, `Rendered`, `Normalize`, `RenderClock`
//! and `RenderGraph` all arrive verbatim from the engine — with one
//! exception, [`ExportError`]: the engine names a node that failed an export
//! by its graph key, and only the adapter knows which entity that is.

use std::path::PathBuf;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_tasks::Task;

use tutti_export::{ExportConfig, Normalize, RenderClock, RenderGraph, Rendered, Written};
use tutti_graph::{ForkCause, ForkFaultKind};
use tutti_types::{NodeKey, Samples};

use tutti_core::transport::OfflineTransport;

/// Which audio to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource {
    /// The whole live graph, as the master hears it.
    ///
    /// A **fork** of what the global outputs hear (`ForkTarget::Master`: a
    /// node no output reaches is not copied, and need not be forkable): every
    /// node isolated, rebound onto the request's [`ExportClock`] and reset, so
    /// it renders what the graph is *driven* to play from that timeline,
    /// starting silent — not a copy of what is sounding now. (Until design
    /// doc 013's PR 13, the `Net` runtime rendered one plain clone of the
    /// net: no isolation, the live transport bindings and running state
    /// kept.)
    Master,
    /// One node's output, isolated from everything downstream of it — "what
    /// does this point in the graph actually sound like".
    ///
    /// Names the **entity**, not its `NodeId`, for the same reason every other
    /// edge in this crate does (`PortSources`, `MasterSources`): the id is
    /// resolved when the render starts, so a node replaced between spawning the
    /// request and starting it — a crossfade, a rebuilt chain — is followed
    /// rather than rendered from a stale id.
    ///
    /// # Cost
    ///
    /// Each export does one **main-thread copy** of the graph in the frame it
    /// starts: a fork (a clone of each node's shadow — and for a hosted
    /// plugin a fresh instance in a new `plugin-server` process loaded with
    /// the live one's state, which takes half a second or more). Running several of those back to back can
    /// stall the frame long enough to underrun the audio callback — an
    /// audible glitch.
    ///
    /// That is why only one export starts per frame (see [`ExportInFlight`]).
    /// Requests wait their turn as entities; nothing is dropped. A caller with
    /// several pending that cares which goes first should spawn the best one
    /// and recompute next frame, rather than spawning all of them and hoping
    /// for an ordering this crate does not promise.
    Node(Entity),
}

/// Where the rendered audio goes.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportTarget {
    /// Encode to a file.
    ///
    /// `normalize: None` streams — the encoder pulls the graph a block at a
    /// time and no PCM is held whole. `Some(..)` is a **two-pass** render:
    /// the whole signal is held in memory so a gain can be measured from it.
    /// The choice is visible at the call site because the cost differs.
    File {
        /// Where to write. A cancelled render may leave a partial file here.
        path: PathBuf,
        /// `None` streams; `Some` measures a gain over the whole signal first,
        /// which costs holding it all in memory.
        normalize: Option<Normalize>,
    },
    /// Render into memory, one `Vec` per channel.
    Buffers,
}

/// Spawn an entity with this to run an export.
///
/// The request is consumed when the render starts: it is replaced by
/// [`ExportInFlight`], and on completion [`ExportDone`] is triggered on the
/// same entity. The entity is left in place for the caller to despawn — an
/// observer added with `.observe()` at the spawn site is the usual way to
/// handle the result.
#[derive(Component)]
#[non_exhaustive]
pub struct ExportRequest {
    /// Which audio to render — the whole mix, or one node in isolation.
    pub source: ExportSource,
    /// Where the rendered audio goes.
    pub target: ExportTarget,
    /// Rate, duration, format, bit depth, channel width, resample, dither.
    ///
    /// Not every field reaches every target. [`ExportTarget::Buffers`] renders
    /// PCM and encodes nothing, so `format` and `bit_depth` are inert there —
    /// and so is `resample`, because `render_to_buffers` deliberately does not
    /// resample. Only `channels` and `dither` affect a `Buffers` render.
    pub config: ExportConfig,
    /// The render's time: what the renderer advances, block by block, and
    /// what every transport-aware node in the rendered graph reads — one
    /// object, so the two cannot disagree. See [`ExportClock`].
    pub clock: ExportClock,
    /// Optional last look at the graph before it leaves the main thread —
    /// see [`PrepareGraph`]. Use [`ExportRequest::new`] when there is nothing
    /// to do.
    pub prepare: Option<PrepareGraph>,
    /// Trim the graph's own reported latency — its compensated look-ahead —
    /// instead of `config.render.latency`. See
    /// [`trim_reported_latency`](Self::trim_reported_latency).
    pub latency_from_graph: bool,
    /// Render the graph's own reported tail instead of `config.render.tail`,
    /// capped at this many frames. See
    /// [`with_reported_tail`](Self::with_reported_tail).
    pub tail_from_graph: Option<Samples>,
}

impl ExportRequest {
    /// A request with no [`prepare`](ExportRequest::prepare) hook.
    pub fn new(
        source: ExportSource,
        target: ExportTarget,
        config: ExportConfig,
        clock: ExportClock,
    ) -> Self {
        Self {
            source,
            target,
            config,
            clock,
            prepare: None,
            latency_from_graph: false,
            tail_from_graph: None,
        }
    }

    /// Render against `timeline`: [`ExportClock::timeline`], set on a
    /// request already built.
    pub fn on_timeline<T>(mut self, timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::Timeline + 'static,
    {
        self.clock = ExportClock::timeline(timeline);
        self
    }

    /// Attach a hook that runs on the graph before the render starts.
    pub fn with_prepare(
        mut self,
        prepare: impl Fn(PreparedGraph, &World) + Send + Sync + 'static,
    ) -> Self {
        self.prepare = Some(Box::new(prepare));
        self
    }

    /// Trim the graph's reported latency from the start of the render — the
    /// figure its PDC aligned every output to (a look-ahead limiter's, a
    /// hosted plugin's) — in place of `config.render.latency`.
    ///
    /// Asked of the graph that is rendered, after the
    /// [`prepare`](Self::prepare) hook: the forked plan's worst-case output
    /// latency (`RenderGraph::reported_latency`, probed at the render's
    /// rate).
    ///
    /// A request-side switch rather than a figure because the caller cannot
    /// know it: the graph it describes is only built when the render starts.
    pub fn trim_reported_latency(mut self) -> Self {
        self.latency_from_graph = true;
        self
    }

    /// Render the graph's reported tail past `duration_seconds` — a reverb's
    /// decay, a plugin's declared tail — in place of `config.render.tail`,
    /// resolved against `cap` by `GraphTail::resolve`'s rule: a graph that
    /// never decays renders `cap`; otherwise the tail its nodes reported,
    /// at most `cap` — a node that said nothing counts as none, not as the
    /// cap. Asked like
    /// [`trim_reported_latency`](Self::trim_reported_latency), of the graph
    /// that is rendered.
    pub fn with_reported_tail(mut self, cap: Samples) -> Self {
        self.tail_from_graph = Some(cap);
        self
    }
}

/// The time an export renders in: the clock the renderer advances and the
/// timeline every transport-aware node reads, as **one** value, so there is
/// no way to hand a render one and its nodes another (a playhead nothing
/// moves, or a clock nothing reads).
///
/// - [`frozen`](Self::frozen): no musical time — a graph with nothing
///   placed on a timeline (an effect tail, a synth patch, a test tone). The
///   renderer's transport is stopped at beat 0, and the nodes are rebound
///   onto a timeline stopped at beat 0 too: a clip reader plays nothing
///   rather than replay its first block.
/// - [`timeline`](Self::timeline): an `OfflineTimeline` (or any clock that
///   is also a `Timeline`), seeded at the tempo and beat to render from, at
///   the render's rate. The renderer advances it; the nodes read it.
#[derive(Clone)]
pub struct ExportClock(Clock);

#[derive(Clone)]
enum Clock {
    Frozen,
    Timeline {
        clock: Arc<dyn RenderClock>,
        timeline: OfflineTransport,
    },
}

impl ExportClock {
    /// No musical time; see the type docs.
    pub fn frozen() -> Self {
        Self(Clock::Frozen)
    }

    /// Render against `timeline`: the renderer advances it, and every
    /// transport-aware node is rebound onto it. One argument, because they
    /// are the same object — the only configuration that is ever correct.
    pub fn timeline<T>(timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::Timeline + 'static,
    {
        Self(Clock::Timeline {
            clock: timeline.clone() as Arc<dyn RenderClock>,
            timeline: timeline as OfflineTransport,
        })
    }

    /// What the renderer advances.
    pub(crate) fn render_clock(&self) -> Arc<dyn RenderClock> {
        match &self.0 {
            Clock::Frozen => Arc::new(tutti_core::transport::FrozenClock),
            Clock::Timeline { clock, .. } => Arc::clone(clock),
        }
    }

    /// What the nodes are rebound onto.
    pub(crate) fn offline(&self) -> OfflineTransport {
        match &self.0 {
            Clock::Frozen => Arc::new(Stopped),
            Clock::Timeline { timeline, .. } => Arc::clone(timeline),
        }
    }
}

impl Default for ExportClock {
    /// [`frozen`](Self::frozen).
    fn default() -> Self {
        Self::frozen()
    }
}

impl std::fmt::Debug for ExportClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Clock::Frozen => f.write_str("ExportClock::Frozen"),
            Clock::Timeline { .. } => f.write_str("ExportClock::Timeline(..)"),
        }
    }
}

/// [`ExportClock::frozen`]'s timeline: stopped at beat 0, what
/// `FrozenClock` hands the renderer. Its tempo is never read by a stopped
/// transport's consumers; 120 BPM is the engine's default.
struct Stopped;

impl tutti_core::Timeline for Stopped {
    fn beat(&self) -> tutti_core::Beat {
        tutti_core::Beat(0.0)
    }
    fn tempo(&self) -> tutti_core::Bpm {
        tutti_core::Bpm(120.0)
    }
    fn is_rolling(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for ExportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn RenderClock` is not Debug.
        f.debug_struct("ExportRequest")
            .field("source", &self.source)
            .field("target", &self.target)
            .field("config", &self.config)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

/// Present while a render occupies the task pool.
///
/// **At most one of these exists at a time.** The per-request net clone is
/// main-thread work (see [`ExportSource::Node`]), so starting a batch of them in
/// one frame is what stalls the audio callback. The limit is enforced where the
/// renders start, not advertised as a run condition for callers to apply: a
/// `run_if` gate can only see the *previous* frame's state, so several requests
/// spawned in one frame would all pass it and all start together — precisely the
/// burst it was supposed to prevent.
///
/// Observe it to know whether an export is running. Gating a spawn system on
/// `not(any_with_component::<ExportInFlight>)` still works and avoids piling up
/// entities, but it is an optimization now, not the safety mechanism.
#[derive(Component)]
pub struct ExportInFlight {
    task: Task<Result<ExportOutput, ExportError>>,
}

impl ExportInFlight {
    pub(crate) fn new(task: Task<Result<ExportOutput, ExportError>>) -> Self {
        Self { task }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<ExportOutput, ExportError>> {
        bevy_tasks::block_on(bevy_tasks::futures_lite::future::poll_once(&mut self.task))
    }

    /// Abort the render, discarding whatever it has done so far.
    ///
    /// No [`ExportDone`] fires for a cancelled export. A cancelled
    /// [`ExportTarget::File`] may leave a partial file at its path — the encoder
    /// writes as it goes, and stopping mid-render does not unlink it.
    ///
    /// Despawning the entity does the same thing implicitly: dropping the
    /// component drops the task, which cancels it.
    pub fn cancel(self) {
        drop(self.task);
    }
}

/// What an export produced.
#[derive(Debug)]
pub enum ExportOutput {
    /// From [`ExportTarget::File`].
    File(Written),
    /// From [`ExportTarget::Buffers`].
    Buffers(Rendered),
}

/// Triggered on the request entity when its render finishes — successfully or
/// not.
///
/// An event rather than a result component because a completed export happens
/// **once**: polling a `Query<&ExportOutput>` every frame and removing the
/// component to avoid re-handling it is a hand-rolled one-shot. Observe it at
/// the spawn site instead, where the surrounding context is still in scope:
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::export::ExportClock;
/// use bevy_tutti::prelude::*;
/// use tutti_export::ExportConfig;
///
/// /// The panel that asked for the render — the context an observer captures
/// /// and a polled `Query<&ExportOutput>` would have to look up again.
/// #[derive(Component)]
/// struct Toast(&'static str);
///
/// fn request_export(In(view_entity): In<Entity>, mut commands: Commands) {
///     commands
///         .spawn(ExportRequest::new(
///             ExportSource::Master,
///             ExportTarget::Buffers,
///             ExportConfig::default(),
///             ExportClock::frozen(),
///         ))
///         .observe(move |done: On<ExportDone>, mut commands: Commands| {
///             // `view_entity` is captured here, still in scope.
///             let label = if done.result.is_ok() { "exported" } else { "export failed" };
///             commands.entity(view_entity).insert(Toast(label));
///         });
/// }
///
/// let mut app = App::new();
/// let view = app.world_mut().spawn_empty().id();
/// app.world_mut().run_system_cached_with(request_export, view).unwrap();
///
/// // One request entity, carrying the observer. Nothing has rendered yet —
/// // `ExportPlugin` is what starts it; the module docs run that end to end.
/// assert_eq!(app.world_mut().query::<&ExportRequest>().iter(app.world()).count(), 1);
/// ```
#[derive(EntityEvent, Debug)]
pub struct ExportDone {
    /// The request entity. Still alive and no longer carrying
    /// [`ExportInFlight`]; the caller despawns it.
    pub entity: Entity,
    /// What the render produced, or why it failed.
    pub result: Result<ExportOutput, ExportError>,
}

/// Why an export failed.
///
/// The renderer's own errors pass through as [`Render`](Self::Render). The
/// three that are about **one node** — it cannot be forked, its fork could
/// not be built, or its fork failed while rendering — name it as an
/// [`ExportNode`]: its entity and `Name`, not only the graph key the engine
/// reports, because the entity is what a host can show and act on ("freeze
/// this track", "remove that mic").
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// Everything that is not about one node: an invalid config, an I/O or
    /// encoder error, a target node with no outputs, a render the engine
    /// refused.
    #[error(transparent)]
    Render(#[from] tutti_export::Error),

    /// `node` cannot be forked for an offline render, so the graph could not
    /// be copied: it shares live state a copy would
    /// share too. A microphone monitor, an in-process VST2 plugin, a node
    /// built unforkable. Nothing was rendered; remove, freeze or bounce the
    /// node, or export a node that it does not feed.
    #[error("cannot export: {node} cannot be forked for an offline render (a mic monitor, an in-process VST2 plugin, or a node built unforkable)")]
    NotForkable {
        /// The node.
        node: ExportNode,
    },

    /// Forking `node` failed before anything rendered: a hosted plugin whose
    /// fresh instance did not load, did not match, or could not save or
    /// load its state. `cause` downcasts to the source's error
    /// (`tutti_plugin::PluginForkError` for a plugin).
    #[error("cannot export: forking {node} failed: {cause}")]
    ForkSource {
        /// The node.
        node: ExportNode,
        /// The fork source's error.
        cause: ForkCause,
    },

    /// `node`'s fork failed **while rendering** — a hosted plugin's server
    /// crashed or stopped answering, a disk-streamed voice could not read its
    /// file — so the render holds silence where its output belongs from then
    /// on. Reported rather than written as a
    /// success; a file target may already have been written, and is not a
    /// valid render.
    #[error("export failed: {node} {kind:?} during the render: {cause}")]
    ForkFailed {
        /// The node.
        node: ExportNode,
        /// Crashed, timed out, or failed (could not produce what it
        /// describes; the cause says why).
        kind: ForkFaultKind,
        /// The unit's own account (`tutti_plugin::PluginRenderFault` for a
        /// plugin; for a disk voice, the file and what went wrong with it).
        cause: ForkCause,
    },
}

/// A graph node an [`ExportError`] is about, as the app knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportNode {
    /// The entity bound to the node, if one is (`None` for a node the adapter
    /// inserted itself, such as the engine's beat clock).
    pub entity: Option<Entity>,
    /// The entity's `Name`, if it has one.
    pub name: Option<String>,
    /// The node's key in the live graph.
    pub key: NodeKey,
}

impl std::fmt::Display for ExportNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.name, self.entity) {
            (Some(name), Some(entity)) => write!(f, "node {name:?} (entity {entity})"),
            (None, Some(entity)) => write!(f, "the node on entity {entity}"),
            _ => write!(f, "graph node {:?}", self.key),
        }
    }
}

/// The graph an export is about to render, handed back to the caller for a
/// last look before it leaves the main thread.
///
/// [`ExportRequest::prepare`] receives this together with `&World`. Two things
/// need it, and neither is something this crate can do on the caller's behalf:
///
/// - **Filling voices.** An isolated copy is born *empty* — `isolate()` drops
///   the voice pools' command channels and clears their voices, so nothing
///   downstream of a sampler makes a sound until something re-inserts them. The
///   data needed to do that (which clips exist, their decoded audio) lives in
///   the app's ECS.
/// - **Shaping the output.** Widening the graph to the master's channel count
///   and pointing a node at the output, say — policy about what "the export"
///   means, which differs per host.
///
/// `graph` is the engine's own [`RenderGraph`]: the fork's own editor and
/// executor, already installed. It is a native graph and nothing else (since
/// design doc 013 PR 14 tutti-export renders no `Net`, so a hook cannot swap
/// one in). Edit a fork through its `editor`
/// (insert nodes, `spec_mut`); the adapter commits whatever the hook leaves,
/// and a commit the fork refuses fails the export with the reason. A fork's
/// units are on its executor by the time the hook runs, so a unit already in
/// it cannot be reached to refill: insert a new one instead.
///
/// `ctx` is the render's offline transport, which every node in the fork was
/// rebound onto. Voices built here must bind to it, not to the live one, or
/// they read a playhead nothing advances.
pub struct PreparedGraph<'a> {
    /// The graph about to be rendered. Mutate it here or not at all — after
    /// this it moves to the task pool.
    pub graph: &'a mut RenderGraph,
    /// The render's offline transport, which the graph was rebound onto.
    /// Voices built in the hook must bind to *this*, not the live transport.
    pub ctx: &'a OfflineTransport,
}

impl PreparedGraph<'_> {
    /// A key no node in any graph this adapter builds holds, for a node the
    /// hook inserts into a fork (`editor.insert(prepared.fresh_key(), ..)`).
    ///
    /// Minted as the live graph mints its own (from `NodeId`'s process-wide
    /// counter), so it cannot collide with a forked node's key, which is its
    /// live node's; a hand-picked key such as `NodeKey(u64::MAX)` would
    /// silently replace whatever node holds it.
    pub fn fresh_key(&self) -> NodeKey {
        NodeKey(tutti_core::dsp::NodeId::new().value())
    }
}

/// A caller's hook into the graph, run on the main thread before the render is
/// handed to the pool.
///
/// `&World` is read-only on purpose: preparing a render is a *read* of app
/// state, and `&mut World` here would let the export pipeline mutate the app
/// from inside itself — the back-channel the projection arrow is not supposed
/// to have.
///
/// A plain boxed closure rather than a trait object behind a resource. A
/// registered filler would be one filler for the whole app: two plugins that
/// both needed one would silently clobber each other, and forgetting to
/// register it renders silence with no diagnostic. Attaching the hook to the
/// *request* means the caller that knows what this graph needs is the one
/// that says so.
pub type PrepareGraph = Box<dyn Fn(PreparedGraph, &World) + Send + Sync>;
