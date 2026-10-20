//! What a caller spawns, and what comes back.
//!
//! An export is an **entity**: spawn one carrying [`ExportRequest`], and either
//! observe [`ExportDone`] on it or watch for [`ExportInFlight`] to clear.
//!
//! The render itself is the session's (`tutti_session::Session::export`):
//! this module's types are what a Bevy host spawns and observes. The target,
//! output, clock and the prepared graph are the session's own
//! ([`ExportTarget`], [`ExportOutput`], [`ExportClock`], [`PreparedGraph`],
//! [`PrepareEdit`]), re-exported here. What the adapter adds: a source named
//! by entity ([`ExportSource`]), a `prepare` hook that reads the `World`
//! ([`PrepareGraph`]), and [`ExportError`]: the engine names a node that
//! failed an export by its graph key, and only the adapter knows which
//! entity that is.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_tasks::Task;

use tutti_export::{ExportConfig, RenderClock};
use tutti_graph::{ForkCause, ForkFaultKind};
use tutti_types::{NodeKey, Samples};

/// The session's: the time an export renders in.
pub use tutti_session::ExportClock;
/// The session's: what an export produced.
pub use tutti_session::ExportOutput;
/// The session's: where the rendered audio goes.
pub use tutti_session::ExportTarget;
/// The session's: the edit half of a [`PrepareGraph`] hook, run on the
/// worker once the fork is built.
pub use tutti_session::PrepareEdit;
/// The session's: the graph a [`PrepareGraph`] hook's edit receives.
pub use tutti_session::PreparedGraph;

use super::run::NodeNames;

/// Which audio to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource {
    /// The whole live graph, as the master hears it.
    ///
    /// A **fork** of what the global outputs hear (`ForkTarget::Master`: a
    /// node no output reaches is not copied, and need not be forkable): every
    /// node isolated, rebound onto the request's [`ExportClock`] and reset, so
    /// it renders what the graph is *driven* to play from that timeline,
    /// starting silent — not a copy of what is sounding now.
    Master,
    /// One node's output, isolated from everything downstream of it — "what
    /// does this point in the graph actually sound like".
    ///
    /// Names the **entity**, not its `NodeKey`, for the same reason every other
    /// edge in this crate does (`PortSources`, `MasterSources`): the id is
    /// resolved when the render starts, so a node replaced between spawning the
    /// request and starting it — a crossfade, a rebuilt chain — is followed
    /// rather than rendered from a stale id.
    ///
    /// # Cost
    ///
    /// Each export **gathers** its fork on the main thread in the frame it
    /// starts (`Editor::fork_gather`): the graph as it is then, a clone of
    /// each node's shadow, a hosted plugin's saved state, and an in-process
    /// VST2 plugin's second instance (built there because VST2 confines it
    /// to the main thread). The slow part of a hosted plugin's fork, a fresh
    /// instance in a new `plugin-server` process loaded with that state (half
    /// a second or more), is built on the compute pool with the render, so
    /// the frame does not wait for it. A gather is still main-thread work,
    /// and several back to back could stall the frame.
    ///
    /// That is why only one export starts per frame (see [`ExportInFlight`]).
    /// Requests wait their turn as entities; nothing is dropped. A caller with
    /// several pending that cares which goes first should spawn the best one
    /// and recompute next frame, rather than spawning all of them and hoping
    /// for an ordering this crate does not promise.
    Node(Entity),
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
    /// Optional hook: a look at the world when the export starts, and an
    /// edit of the graph before it renders — see [`PrepareGraph`]. Use
    /// [`ExportRequest::new`] when there is nothing to do.
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
    /// Creates a request with no [`prepare`](ExportRequest::prepare) hook.
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

    /// Renders against `timeline`: [`ExportClock::timeline`], set on a
    /// request already built.
    pub fn on_timeline<T>(mut self, timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::transport::OfflineClock + 'static,
    {
        self.clock = ExportClock::timeline(timeline);
        self
    }

    /// Attaches a hook in two halves (see [`PrepareGraph`]): `prepare` reads
    /// the world on the main thread in the frame the export starts, and
    /// returns the edit to make to the graph, which runs on the compute pool
    /// once the fork is built, before the render.
    ///
    /// Annotate the edit's argument (`|prepared: PreparedGraph<'_>|`) so it
    /// takes a graph of any lifetime.
    pub fn with_prepare<F, E>(mut self, prepare: F) -> Self
    where
        F: Fn(&World) -> E + Send + Sync + 'static,
        E: FnOnce(PreparedGraph<'_>) + Send + 'static,
    {
        self.prepare = Some(Box::new(move |world| Box::new(prepare(world))));
        self
    }

    /// Trims the graph's reported latency from the start of the render — the
    /// figure its PDC aligned every output to (a look-ahead limiter's, a
    /// hosted plugin's) — in place of `config.render.latency`.
    ///
    /// Asked of the graph that is rendered, after the
    /// [`prepare`](Self::prepare) hook's edit: the forked plan's worst-case output
    /// latency (`RenderGraph::reported_latency`, probed at the render's
    /// rate).
    ///
    /// A request-side switch rather than a figure because the caller cannot
    /// know it: the graph it describes is only built when the render starts.
    pub fn trim_reported_latency(mut self) -> Self {
        self.latency_from_graph = true;
        self
    }

    /// Renders the graph's reported tail past `duration_seconds` — a reverb's
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

/// Present while an export occupies the task pool: its fork building, then
/// its render.
///
/// **At most one of these exists at a time.** The per-request gather of the
/// graph fork is main-thread work (see [`ExportSource::Node`]), so starting a
/// batch of them in one frame is what could stall the audio callback. The limit is enforced where the
/// renders start ([`start_exports`](super::start_exports)), so a burst of
/// requests spawned in one frame still starts one at a time.
///
/// Observe it to know whether an export is running. Gating a spawn system on
/// `not(any_with_component::<ExportInFlight>)` avoids piling up request
/// entities, but is not needed for safety.
#[derive(Component)]
pub struct ExportInFlight {
    task: Task<tutti_session::ExportRendered>,
    /// The node bindings when the render started, which name a node a
    /// failure is about.
    names: NodeNames,
}

impl ExportInFlight {
    pub(crate) fn new(task: Task<tutti_session::ExportRendered>, names: NodeNames) -> Self {
        Self { task, names }
    }

    /// An in-flight export with no node bindings, for this crate's tests.
    #[cfg(test)]
    pub(crate) fn for_test(task: Task<tutti_session::ExportRendered>) -> Self {
        Self::new(task, NodeNames::default())
    }

    /// The finished job's return, for `Session::finish_export` on the main
    /// thread, with the names its errors map through.
    pub(crate) fn poll(&mut self) -> Option<(tutti_session::ExportRendered, &NodeNames)> {
        let done =
            bevy_tasks::block_on(bevy_tasks::futures_lite::future::poll_once(&mut self.task))?;
        Some((done, &self.names))
    }

    /// Aborts the render, discarding whatever it has done so far.
    ///
    /// No [`ExportDone`] fires for a cancelled export. A cancelled
    /// [`ExportTarget::File`] may leave a partial file at its path — the encoder
    /// writes as it goes, and stopping mid-render does not unlink it.
    ///
    /// Despawning the entity does the same thing implicitly: dropping the
    /// component drops the task, which cancels it.
    ///
    /// Whatever the export holds, whenever it is cancelled (before its fork
    /// is built, or mid-render: a render already running finishes its poll
    /// first), goes back to the session and is dropped by its next
    /// `Phase::Collect` (the frame's commit), on the main thread, where a
    /// forked in-process VST2 plugin in it must close. The export's token
    /// goes with the task, so the next frame's
    /// [`plugin_render_mode`](crate::plugin_host::plugin_render_mode) tells
    /// the hosted plugins `Realtime`.
    ///
    /// **At shutdown**, an export still in flight when the world (and its
    /// session) is dropped has nowhere to send what it holds when its task
    /// finishes or is dropped, and drops it where it is: on a pool thread,
    /// for a pinned in-process VST2 fork a close off the main thread.
    /// Nothing here waits for in-flight exports on exit (a render can run
    /// for minutes, and an exit should not); a host with such a plugin in
    /// its exports keeps updating until they have reported (`ExportInFlight`
    /// gone) before it drops the world.
    pub fn cancel(self) {
        drop(self.task);
    }
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
    /// share too. A microphone monitor, a node built unforkable. Nothing was
    /// rendered; remove, freeze or bounce the node, or export a node that it
    /// does not feed.
    #[error("cannot export: {node} cannot be forked for an offline render (a mic monitor, or a node built unforkable)")]
    NotForkable {
        /// The node.
        node: ExportNode,
    },

    /// Forking `node` failed before anything rendered: a hosted plugin whose
    /// fresh instance did not load (an in-process VST2 plugin that cannot be
    /// instantiated twice), did not match, or could not save or load its
    /// state. `cause` downcasts to the source's error
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

/// A caller's hook into the graph, in two halves: the hook runs on the
/// **main thread in the frame the export starts**, reads what it needs from
/// `&World`, and returns the **edit**, which runs on the compute pool once
/// the fork is built ([`PreparedGraph`]), before the render. The edit is a
/// pool thread's: it must not insert a main-thread-only unit (see
/// [`PrepareEdit`]).
///
/// Split because the fork is: it is gathered from the live graph in the
/// frame the export starts and built on the pool (a hosted plugin's fork
/// launches a `plugin-server`, half a second or more), so there is no graph
/// to edit while the world is readable. Reading the world when the export
/// starts keeps what the hook takes from the app consistent with the graph
/// the fork was taken from: a clip edited a frame later reaches neither.
///
/// ```rust
/// # use bevy_ecs::prelude::*;
/// # use bevy_tutti::export::{ExportClock, ExportRequest, ExportSource, ExportTarget, PreparedGraph};
/// # use tutti_export::ExportConfig;
/// #[derive(Resource)]
/// struct Level(f32);
///
/// let request = ExportRequest::new(
///     ExportSource::Master,
///     ExportTarget::Buffers,
///     ExportConfig::default(),
///     ExportClock::frozen(),
/// )
/// .with_prepare(|world: &World| {
///     // On the main thread, in the frame the export starts.
///     let level = world.resource::<Level>().0;
///     // On the compute pool, once the fork is built.
///     move |prepared: PreparedGraph<'_>| {
///         let _ = (level, prepared.graph.editor_mut());
///     }
/// });
/// # let _ = request;
/// ```
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
pub type PrepareGraph = Box<dyn Fn(&World) -> PrepareEdit + Send + Sync>;
