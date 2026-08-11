//! What a caller spawns, and what comes back.
//!
//! An export is an **entity**: spawn one carrying [`ExportRequest`], and either
//! observe [`ExportDone`] on it or watch for [`ExportInFlight`] to clear.
//!
//! Everything here is either a Bevy component or a type `tutti-export` already
//! owns. The adapter mints no export vocabulary of its own — `ExportConfig`,
//! `Written`, `Rendered`, `Normalize` and `RenderClock` all arrive verbatim
//! from the engine.

use std::path::PathBuf;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_tasks::Task;

use tutti_export::{ExportConfig, Normalize, RenderClock, Rendered, Written};

use tutti_core::transport::OfflineTransport;

/// Which audio to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource {
    /// The whole live graph, as the master hears it. One clone of the net, no
    /// per-node isolation.
    Master,
    /// One node's output, isolated from everything downstream of it — "what
    /// does this point in the graph actually sound like".
    ///
    /// Names the **entity**, not its `NodeId`, for the same reason every other
    /// edge in this crate does (`AudioSources`, `MasterSources`): the id is
    /// resolved when the render starts, so a node replaced between spawning the
    /// request and starting it — a crossfade, a rebuilt chain — is followed
    /// rather than rendered from a stale id.
    ///
    /// # Cost
    ///
    /// Each of these does one **main-thread deep clone** of the live net
    /// (`clone_isolated` → `DynClone` of every DSP node) in the frame it
    /// starts. Running several of those back to back can stall the frame long
    /// enough to underrun the audio callback — an audible glitch.
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
    /// The clock the render advances, one block at a time. Use
    /// `tutti_export::FrozenClock` for a graph with no time-dependent nodes, or
    /// an `OfflineTimeline` seeded at the beat you want to render from.
    pub clock: Arc<dyn RenderClock>,
    /// The timeline transport-aware nodes are rebound onto, for
    /// [`ExportSource::Node`].
    ///
    /// Normally the *same object* as `clock`: the renderer advances the clock
    /// and the nodes read this, so two different timelines means the nodes watch
    /// a playhead nothing moves. [`ExportRequest::on_timeline`] sets both at
    /// once and is the way to say it.
    ///
    /// `None` builds a default at the render's sample rate, 120 BPM, from beat
    /// 0 — right for a graph with no musical time (an effect tail, a synth
    /// patch), wrong for anything placed on a timeline.
    ///
    /// Ignored for [`ExportSource::Master`], which keeps its live bindings.
    pub offline: Option<OfflineTransport>,
    /// Optional last look at the net before it leaves the main thread — see
    /// [`PrepareNet`]. Use [`ExportRequest::new`] when there is nothing to do.
    pub prepare: Option<PrepareNet>,
}

impl ExportRequest {
    /// A request with no [`prepare`](ExportRequest::prepare) hook.
    pub fn new(
        source: ExportSource,
        target: ExportTarget,
        config: ExportConfig,
        clock: Arc<dyn RenderClock>,
    ) -> Self {
        Self {
            source,
            target,
            config,
            clock,
            offline: None,
            prepare: None,
        }
    }

    /// Render against `timeline`: the renderer advances it, and every
    /// transport-aware node is rebound onto it.
    ///
    /// Sets both ends from one argument, because they are the same object —
    /// which is the only configuration that is ever correct. Takes anything that
    /// is both a [`RenderClock`] and a `Timeline`; `OfflineTimeline` is the
    /// usual one.
    pub fn on_timeline<T>(mut self, timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::Timeline + 'static,
    {
        self.clock = timeline.clone() as Arc<dyn RenderClock>;
        self.offline = Some(timeline as OfflineTransport);
        self
    }

    /// Attach a hook that runs on the net before the render starts.
    pub fn with_prepare(
        mut self,
        prepare: impl Fn(PreparedNet, &World) + Send + Sync + 'static,
    ) -> Self {
        self.prepare = Some(Box::new(prepare));
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
    task: Task<tutti_export::Result<ExportOutput>>,
}

impl ExportInFlight {
    pub(crate) fn new(task: Task<tutti_export::Result<ExportOutput>>) -> Self {
        Self { task }
    }

    pub(crate) fn poll(&mut self) -> Option<tutti_export::Result<ExportOutput>> {
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
/// use std::sync::Arc;
///
/// use bevy_app::prelude::*;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::prelude::*;
/// use tutti_export::{ExportConfig, FrozenClock};
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
///             Arc::new(FrozenClock),
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
    pub result: tutti_export::Result<ExportOutput>,
}

/// The net an export is about to render, handed back to the caller for a last
/// look before it leaves the main thread.
///
/// [`ExportRequest::prepare`] receives this together with `&World`. Two things
/// need it, and neither is something this crate can do on the caller's behalf:
///
/// - **Filling voices.** An isolated clone is born *empty* — `isolate()` drops
///   the voice pools' command channels and clears their voices, so nothing
///   downstream of a sampler makes a sound until something re-inserts them. The
///   data needed to do that (which clips exist, their decoded audio) lives in
///   the app's ECS.
/// - **Shaping the output.** Widening the net to the master's channel count and
///   piping a node to the output, say — policy about what "the export" means,
///   which differs per host.
///
/// `ctx` is `Some` only for [`ExportSource::Node`], and carries the render's
/// offline transport: voices built here must bind to it, not to the live one,
/// or they read a playhead nothing advances. For [`ExportSource::Master`] it is
/// `None` — that net keeps its live transport bindings, which the caller's own
/// clock drives.
pub struct PreparedNet<'a> {
    /// The net about to be rendered. Mutate it here or not at all — after this
    /// it moves to the task pool.
    pub net: &'a mut tutti_core::dsp::Net,
    /// The render's offline transport, `Some` only for [`ExportSource::Node`].
    /// Voices built in the hook must bind to *this*, not the live transport.
    pub ctx: Option<&'a OfflineTransport>,
}

/// A caller's hook into the net, run on the main thread before the render is
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
/// *request* means the caller that knows what this net needs is the one that
/// says so.
pub type PrepareNet = Box<dyn Fn(PreparedNet, &World) + Send + Sync>;
