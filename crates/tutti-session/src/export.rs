//! Offline export in the session (feature `export`; design 015 §4.3): the
//! request ([`ExportRequest`]), the job ([`ExportJob`]: gathered on the
//! control thread, built and rendered on any), and its return
//! ([`Session::finish_export`]), which drops what the render leaves on the
//! main thread and releases the export's token.
//!
//! **One export at a time**: [`Session::export`] mints an export token, a
//! clone of an `Arc<()>` the session holds, which rides in the job and then
//! in what it returns ([`ExportRendered`]). An export is in flight while any
//! token lives, and another is refused
//! ([`Error::ExportInFlight`](crate::Error::ExportInFlight)). With `plugin`,
//! [`Phase::RenderMode`](crate::Phase::RenderMode) announces Offline from
//! that level: `export` runs the phase before it returns (so a plugin
//! hears Offline before the job can render a block), `finish_export` drops
//! the token and runs it again, and a job or its return dropped unfinished
//! (a cancelled task, a panic on the worker) drops its token, so the next
//! update restores Realtime. A host that may start the next export before
//! its next `Phase::RenderMode` finishes with
//! [`Session::finish_export_unannounced`] instead, so its plugins are not
//! told Realtime and then Offline again between two exports.
//!
//! **A token that never drops** (a job `mem::forget`ed, or kept unrun
//! forever) keeps the export in flight: the plugins stay Offline and every
//! later export is refused. [`Phase::Collect`](crate::Phase::Collect) warns,
//! once, when one has been in flight for [`EXPORT_STALL_WARNING`] of its
//! `now`.
//!
//! **Leftovers drop on the main thread.** The rendered graph, what a failed
//! fork build had made, an edit that never ran: each is wrapped so that
//! wherever it is dropped, it is sent back to the session instead, and
//! dropped by `finish_export` or the next
//! [`Phase::Collect`](crate::Phase::Collect). A forked in-process VST2
//! plugin closes its instance when dropped, which VST2 confines to the main
//! thread. Something sent once the session is gone drops where it is.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_core::transport::OfflineTransport;
use tutti_core::AudioNode;
use tutti_export::{ExportConfig, Normalize, RenderClock, RenderGraph, Rendered, Written};
use tutti_types::{NodeKey, Samples};

use crate::graph::AudioNodeTarget;
use crate::graph::PendingExport;
use crate::{Error, Session};

/// Which audio an export renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource {
    /// What the global outputs hear: a fork of every node an output
    /// reaches (a node none reaches is not copied, and need not be
    /// forkable).
    Mix,
    /// One node's outputs, isolated from everything downstream of it.
    Node(AudioNode),
}

/// Where the rendered audio goes.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportTarget {
    /// Encode to a file. `normalize: None` streams (the encoder pulls the
    /// graph a block at a time); `Some(..)` is a two-pass render that holds
    /// the whole signal to measure a gain from it.
    File {
        /// Where to write. An abandoned render may leave a partial file.
        path: PathBuf,
        /// `None` streams; `Some` measures a gain over the whole signal.
        normalize: Option<Normalize>,
    },
    /// Render into memory, one `Vec` per channel.
    Buffers,
}

/// What an export produced.
#[derive(Debug)]
pub enum ExportOutput {
    /// From [`ExportTarget::File`].
    File(Written),
    /// From [`ExportTarget::Buffers`].
    Buffers(Rendered),
}

/// An export: which audio, where to, how, on which clock, and an optional
/// edit of the forked graph before it renders. Free of any host's ids
/// (design 015 §4.3).
#[non_exhaustive]
pub struct ExportRequest {
    /// Which audio to render.
    pub source: ExportSource,
    /// Where it goes.
    pub target: ExportTarget,
    /// Rate, duration, format, bit depth, width, resample, dither.
    /// [`ExportTarget::Buffers`] encodes nothing, so only `channels` and
    /// `dither` reach it (`render_to_buffers` does not resample).
    pub config: ExportConfig,
    /// The render's time. See [`ExportClock`].
    pub clock: ExportClock,
    /// An edit of the forked graph, made on the job's thread once the fork
    /// is built, before the render. See [`PrepareEdit`].
    pub edit: Option<PrepareEdit>,
    /// Trim the rendered graph's reported latency instead of
    /// `config.render.latency`. See
    /// [`trim_reported_latency`](Self::trim_reported_latency).
    pub latency_from_graph: bool,
    /// Render the rendered graph's reported tail, capped at this, instead
    /// of `config.render.tail`. See
    /// [`with_reported_tail`](Self::with_reported_tail).
    pub tail_from_graph: Option<Samples>,
}

impl ExportRequest {
    /// A request with no edit and the configured latency and tail.
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
            edit: None,
            latency_from_graph: false,
            tail_from_graph: None,
        }
    }

    /// The mix into a file at `path`, streamed, on a frozen clock.
    pub fn file(path: impl Into<PathBuf>, config: ExportConfig) -> Self {
        Self::new(
            ExportSource::Mix,
            ExportTarget::File {
                path: path.into(),
                normalize: None,
            },
            config,
            ExportClock::frozen(),
        )
    }

    /// Renders against `timeline`: [`ExportClock::timeline`].
    pub fn on_timeline<T>(mut self, timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::transport::OfflineClock + 'static,
    {
        self.clock = ExportClock::timeline(timeline);
        self
    }

    /// Edits the forked graph before it renders: see [`PrepareEdit`].
    pub fn with_edit(mut self, edit: PrepareEdit) -> Self {
        self.edit = Some(edit);
        self
    }

    /// Trims the graph's reported latency from the start of the render (the
    /// figure its PDC aligned every output to: a look-ahead limiter's, a
    /// hosted plugin's) in place of `config.render.latency`. Asked of the
    /// graph that renders, after the edit (`RenderGraph::reported_latency`,
    /// at the render's rate): only then is it built.
    pub fn trim_reported_latency(mut self) -> Self {
        self.latency_from_graph = true;
        self
    }

    /// Renders the graph's reported tail past `duration_seconds` (a
    /// reverb's decay, a plugin's declared tail) in place of
    /// `config.render.tail`, by `GraphTail::resolve`'s rule against `cap`:
    /// a graph that never decays renders `cap`; otherwise the tail its
    /// nodes reported, at most `cap` (a node that said nothing counts as
    /// none). Asked like
    /// [`trim_reported_latency`](Self::trim_reported_latency).
    pub fn with_reported_tail(mut self, cap: Samples) -> Self {
        self.tail_from_graph = Some(cap);
        self
    }
}

impl std::fmt::Debug for ExportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportRequest")
            .field("source", &self.source)
            .field("target", &self.target)
            .field("config", &self.config)
            .field("clock", &self.clock)
            .field("edit", &self.edit.is_some())
            .field("latency_from_graph", &self.latency_from_graph)
            .field("tail_from_graph", &self.tail_from_graph)
            .finish()
    }
}

/// The time an export renders in: the clock the renderer advances and the
/// timeline every transport-aware node reads, as **one** value, so a render
/// cannot be handed one and its nodes another.
///
/// - [`frozen`](Self::frozen): no musical time (an effect tail, a synth
///   patch, a test tone). The renderer's transport is stopped at beat 0,
///   and the nodes are rebound onto a timeline stopped at beat 0 too.
/// - [`timeline`](Self::timeline): an `OfflineTimeline` (or any clock that
///   is also an `OfflineClock`: never the live transport), seeded at the
///   tempo and beat to render from, at the render's rate.
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

    /// Renders against `timeline`: the renderer advances it, and every
    /// transport-aware node is rebound onto it.
    pub fn timeline<T>(timeline: Arc<T>) -> Self
    where
        T: RenderClock + tutti_core::transport::OfflineClock + 'static,
    {
        Self(Clock::Timeline {
            clock: timeline.clone() as Arc<dyn RenderClock>,
            timeline: OfflineTransport::new(timeline),
        })
    }

    /// What the renderer advances.
    fn render_clock(&self) -> Arc<dyn RenderClock> {
        match &self.0 {
            Clock::Frozen => Arc::new(tutti_core::transport::FrozenClock),
            Clock::Timeline { clock, .. } => Arc::clone(clock),
        }
    }

    /// What the nodes are rebound onto.
    fn offline(&self) -> OfflineTransport {
        match &self.0 {
            Clock::Frozen => OfflineTransport::new(Arc::new(Stopped)),
            Clock::Timeline { timeline, .. } => timeline.clone(),
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
/// `FrozenClock` hands the renderer. A stopped transport's tempo is never
/// read; 120 BPM is the engine's default.
struct Stopped;

/// The frozen render's timeline: only the render reads it.
impl tutti_core::transport::OfflineClock for Stopped {}

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
    fn segment_generation(&self) -> u64 {
        // Never moves, so never jumps.
        0
    }
}

/// The graph an export is about to render, handed to its
/// [`PrepareEdit`] on the job's thread once the fork is built.
///
/// `graph` is the engine's own [`RenderGraph`], its executor installed:
/// edit it through `graph.editor_mut()` (insert nodes, `spec_mut`); the
/// job commits whatever the edit leaves, and a commit the fork refuses
/// fails the export with the reason. A fork's units are on its executor by
/// now, so a unit already in it cannot be refilled: insert a new one.
pub struct PreparedGraph<'a> {
    /// The graph about to be rendered.
    pub graph: &'a mut RenderGraph,
    /// The render's offline transport, which every node of the fork was
    /// rebound onto. A node built here binds to this, not the live one.
    pub ctx: &'a OfflineTransport,
}

impl PreparedGraph<'_> {
    /// A key no node in any graph holds, for a node the edit inserts
    /// (`editor.insert(prepared.fresh_key(), ..)`), minted from
    /// [`NodeKey::fresh`]'s process-wide counter as the live graph's are.
    pub fn fresh_key(&self) -> NodeKey {
        NodeKey::fresh()
    }
}

/// An edit of an export's forked graph, made once the fork is built,
/// before the render.
///
/// **It runs on the job's thread**, and so do the `prepare` of every node
/// it inserts and the drop of whatever it captured once it has run. It must
/// not insert, prepare or capture a unit confined to the main thread (an
/// in-process VST2 plugin); such a node goes into the live graph, where the
/// fork's gather builds it on the main thread. An edit that never runs (the
/// build failed, the job was dropped) is dropped on the main thread.
pub type PrepareEdit = Box<dyn for<'a> FnOnce(PreparedGraph<'a>) + Send>;

/// How long an export may be in flight, on the `now` the session's
/// [`Phase::Collect`](crate::Phase::Collect) is given, before it warns that
/// its token may have leaked: an hour. Generous: an offline render of a
/// long project, with plugin forks that launch servers, takes minutes.
pub const EXPORT_STALL_WARNING: Duration = Duration::from_secs(60 * 60);

/// The session's export state: the token every export holds a clone of,
/// the channel leftovers come back on, and the stall watch.
pub(crate) struct ExportState {
    token: Arc<()>,
    send: Sender<Box<dyn Send>>,
    recv: Receiver<Box<dyn Send>>,
    /// The `now` the collect first saw the current export in flight.
    since: Option<Instant>,
    /// Whether the current export's stall was warned about.
    stall_warned: bool,
    /// How many stalls were warned about: what a test reads.
    stall_warnings: usize,
}

impl Default for ExportState {
    fn default() -> Self {
        let (send, recv) = channel();
        Self {
            token: Arc::new(()),
            send,
            recv,
            since: None,
            stall_warned: false,
            stall_warnings: 0,
        }
    }
}

impl ExportState {
    /// Whether an export token lives.
    pub(crate) fn in_flight(&self) -> bool {
        Arc::strong_count(&self.token) > 1
    }

    /// Watches for a leaked token at `now`: warns once when an export has
    /// been in flight (as the collects saw it) for [`EXPORT_STALL_WARNING`].
    /// Its clock restarts when nothing is in flight.
    pub(crate) fn watch_stall(&mut self, now: Instant) {
        if !self.in_flight() {
            self.since = None;
            self.stall_warned = false;
            return;
        }
        let since = *self.since.get_or_insert(now);
        if !self.stall_warned && now.saturating_duration_since(since) >= EXPORT_STALL_WARNING {
            self.stall_warned = true;
            self.stall_warnings += 1;
            tracing::warn!(
                "an export has been in flight for over {} minutes: if its job or what it \
                 returned leaked (forgotten, or kept unrun), hosted plugins stay offline \
                 and every later export is refused (ExportInFlight)",
                EXPORT_STALL_WARNING.as_secs() / 60
            );
        }
    }

    /// Drops, on this thread, every leftover sent back so far.
    pub(crate) fn drop_leftovers(&self) {
        while let Ok(value) = self.recv.try_recv() {
            drop(value);
        }
    }
}

/// `T`, sent back to the session wherever it is dropped, unless
/// [`take`](Self::take)n first.
struct ToMain<T: Send + 'static> {
    value: Option<T>,
    to: Sender<Box<dyn Send>>,
}

impl<T: Send + 'static> ToMain<T> {
    fn new(value: T, to: Sender<Box<dyn Send>>) -> Self {
        Self {
            value: Some(value),
            to,
        }
    }

    /// The value, no longer sent anywhere: its new owner drops it.
    fn take(mut self) -> T {
        self.value.take().expect("taken once")
    }
}

impl<T: Send + 'static> std::ops::Deref for ToMain<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("not taken")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for ToMain<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("not taken")
    }
}

impl<T: Send + 'static> Drop for ToMain<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            // A session already gone hands the value back, and it drops
            // here.
            let _ = self.to.send(Box::new(value));
        }
    }
}

/// An export, gathered on the control thread: `Send`, and
/// [`run`](Self::run) builds the fork (a hosted plugin's fork launches a
/// `plugin-server` and loads its state), runs the edit, and renders, on
/// any thread. Hand what it returns to [`Session::finish_export`].
///
/// Holds the export's token: dropping it unrun ends the export (the next
/// update announces Realtime), and what it holds goes back to the session
/// to drop.
///
/// **Do not leak it.** A job (or its [`ExportRendered`]) that is
/// `mem::forget`ed, or kept unrun forever, keeps its token: the export
/// stays in flight, hosted plugins stay Offline, and every later
/// [`Session::export`] is refused with
/// [`Error::ExportInFlight`](crate::Error::ExportInFlight). Nothing else
/// reports it; the session warns once after [`EXPORT_STALL_WARNING`].
#[must_use = "an export renders nothing until it is run and finished"]
pub struct ExportJob {
    pending: ToMain<PendingExport>,
    edit: Option<ToMain<PrepareEdit>>,
    ctx: OfflineTransport,
    clock: Arc<dyn RenderClock>,
    target: ExportTarget,
    config: ExportConfig,
    latency_from_graph: bool,
    tail_from_graph: Option<Samples>,
    drops: Sender<Box<dyn Send>>,
    token: Arc<()>,
}

impl ExportJob {
    /// Builds the fork, runs the edit, commits it, and renders. Any thread;
    /// it blocks for the whole render.
    pub fn run(self) -> ExportRendered {
        let Self {
            pending,
            edit,
            ctx,
            clock,
            target,
            mut config,
            latency_from_graph,
            tail_from_graph,
            drops,
            token,
        } = self;
        let mut graph = match pending.take().build() {
            Ok(graph) => ToMain::new(graph, drops),
            Err((e, remains)) => {
                drop(ToMain::new(remains, drops));
                return ExportRendered {
                    result: Err(e),
                    graph: None,
                    _token: token,
                };
            }
        };
        if let Some(edit) = edit {
            (edit.take())(PreparedGraph {
                graph: &mut graph,
                ctx: &ctx,
            });
        }
        // A fork's editor holds whatever the edit left, uncommitted: send
        // it, and apply it here so the render's first block runs it.
        if let Err(e) = graph.commit() {
            return ExportRendered {
                result: Err(invalid(format!(
                    "the prepare hook left the forked graph uncommittable: {e}"
                ))),
                graph: Some(graph),
                _token: token,
            };
        }
        // The graph's own figures, asked of the graph that renders: after
        // the edit, which may have changed it.
        if latency_from_graph {
            config.render.latency = graph.reported_latency();
        }
        if let Some(cap) = tail_from_graph {
            // `resolve`, not `samples().unwrap_or(cap)`: a node that never
            // said (`Tail::Unknown`) is spent at what the others reported,
            // not at the cap, which would append silence.
            config.render.tail = graph.reported_tail().resolve(cap);
        }
        let result = match target {
            ExportTarget::File {
                path,
                normalize: None,
            } => tutti_export::render_to_file(&mut *graph, &config, clock.as_ref(), &path)
                .map(ExportOutput::File),
            ExportTarget::File {
                path,
                normalize: Some(normalize),
            } => tutti_export::render_normalized_to_file(
                &mut *graph,
                &config,
                clock.as_ref(),
                normalize,
                &path,
            )
            .map(ExportOutput::File),
            ExportTarget::Buffers => {
                tutti_export::render_to_buffers(&mut *graph, &config, clock.as_ref())
                    .map(ExportOutput::Buffers)
            }
        };
        ExportRendered {
            result,
            graph: Some(graph),
            _token: token,
        }
    }
}

/// What a finished [`ExportJob`] returns: the result, the rendered graph
/// (dropped on the main thread by [`Session::finish_export`]), and the
/// export's token. `Send`.
#[must_use = "finish the export with `Session::finish_export`"]
pub struct ExportRendered {
    result: tutti_export::Result<ExportOutput>,
    graph: Option<ToMain<RenderGraph>>,
    _token: Arc<()>,
}

/// Why an export found nothing to render: its node has no audio outputs.
const NO_OUTPUTS: &str = "export target node has no audio outputs";

/// Why an export found nothing to render: the graph has no outputs.
const GRAPH_HAS_NO_OUTPUTS: &str = "the graph has no outputs to export";

fn invalid(reason: impl Into<String>) -> tutti_export::Error {
    tutti_export::Error::InvalidConfig(reason.into())
}

impl Session {
    /// Starts an export: gathers its fork of the live graph here (what it
    /// reads of the live graph and its nodes, now; an in-process VST2
    /// plugin's second instance is built here too), mints the export's
    /// token, and (with `plugin`) announces Offline to every hosted plugin
    /// before it returns. [`run`](ExportJob::run) the job on any thread,
    /// then [`finish_export`](Self::finish_export) what it returns.
    ///
    /// The fork renders what the graph is driven to play from the request's
    /// clock, from silence: every node isolated, rebound onto that clock
    /// and reset; controls as they are now; a hosted plugin forked by state
    /// transfer (a fresh instance in a new `plugin-server`, loaded with the
    /// live one's state).
    ///
    /// # Errors
    ///
    /// [`Error::ExportInFlight`] while another export's token lives. An
    /// [`Error::Export`] when the graph has no outputs, the source node has
    /// none (or is not in the graph), or a node the fork needs cannot be
    /// forked (`NotForkable`, a fork source that failed, by node key).
    pub fn export(&mut self, request: ExportRequest) -> crate::Result<ExportJob> {
        if self.export.in_flight() {
            return Err(Error::ExportInFlight);
        }
        if self.graph.outputs() == 0 {
            return Err(invalid(GRAPH_HAS_NO_OUTPUTS).into());
        }
        let ExportRequest {
            source,
            target,
            config,
            clock,
            edit,
            latency_from_graph,
            tail_from_graph,
        } = request;
        let node = match source {
            ExportSource::Mix => AudioNodeTarget::Master,
            ExportSource::Node(node) => AudioNodeTarget::Node(node),
        };
        // The timeline every transport-aware node of the fork is re-seated
        // on: the request's clock, the same object the renderer advances, or
        // a timeline stopped at beat 0 for a frozen one.
        let ctx = clock.offline();
        let pending = match self
            .graph
            .gather_for_export(node, &ctx, config.render.sample_rate)
        {
            Ok(pending) => pending,
            Err(tutti_export::Error::Fork(
                tutti_graph::ForkError::NoOutputs { .. }
                | tutti_graph::ForkError::NoSuchNode { .. },
            )) => return Err(invalid(NO_OUTPUTS).into()),
            Err(e) => return Err(e.into()),
        };
        let drops = self.export.send.clone();
        let job = ExportJob {
            pending: ToMain::new(pending, drops.clone()),
            edit: edit.map(|edit| ToMain::new(edit, drops.clone())),
            ctx,
            clock: clock.render_clock(),
            target,
            config,
            latency_from_graph,
            tail_from_graph,
            drops,
            token: Arc::clone(&self.export.token),
        };
        // Offline is announced here, on the control thread, before the job
        // can render a block.
        #[cfg(feature = "plugin")]
        self.render_mode_phase();
        Ok(job)
    }

    /// Ends an export: drops the rendered graph (and every leftover sent
    /// back so far) here, releases the export's token and (with `plugin`)
    /// announces Realtime when no other export is in flight. Returns what
    /// the render produced.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// The render's [`Error::Export`]: a failed fork build or render (a
    /// fork that failed mid-render names its node's key), an I/O or encoder
    /// error.
    pub fn finish_export(&mut self, rendered: ExportRendered) -> crate::Result<ExportOutput> {
        let result = self.finish_export_unannounced(rendered);
        #[cfg(feature = "plugin")]
        self.render_mode_phase();
        result
    }

    /// [`finish_export`](Self::finish_export) without its announcement:
    /// drops the rendered graph and the leftovers here and releases the
    /// export's token, but leaves telling the plugins Realtime to the next
    /// [`Phase::RenderMode`](crate::Phase::RenderMode) (the next
    /// [`update`](Self::update), or the host's own run of the phase).
    ///
    /// For a host that may start another export before that phase runs (a
    /// queue of them, one finished and the next started in one frame):
    /// with `finish_export` each plugin would be told Realtime and then
    /// Offline again between the two, and three of the four formats take a
    /// mode change only while deactivated, so each announcement is a
    /// deactivate and reactivate (and, for a plugin whose latency follows
    /// the mode, a re-plan). The host must run `Phase::RenderMode` after it,
    /// or the plugins stay Offline once the last export ends.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// As [`finish_export`](Self::finish_export).
    pub fn finish_export_unannounced(
        &mut self,
        rendered: ExportRendered,
    ) -> crate::Result<ExportOutput> {
        crate::main_thread();
        let ExportRendered {
            result,
            graph,
            _token,
        } = rendered;
        if let Some(graph) = graph {
            drop(graph.take());
        }
        self.export.drop_leftovers();
        drop(_token);
        Ok(result?)
    }

    /// [`export`](Self::export), run on this thread, and finished.
    ///
    /// # Errors
    ///
    /// As `export` and `finish_export`.
    pub fn export_blocking(&mut self, request: ExportRequest) -> crate::Result<ExportOutput> {
        let job = self.export(request)?;
        self.finish_export(job.run())
    }

    /// Drops, here, what exports sent back so far: what a dropped job or a
    /// dropped return held (its fork, a hook's edit). `Phase::Collect` (each
    /// [`update`](Self::update)) and [`finish_export`](Self::finish_export)
    /// do this; a host that polls its exports on a frame of its own calls it
    /// there. A main-thread call: a forked in-process VST2 plugin closes
    /// when dropped.
    pub fn drop_export_leftovers(&mut self) {
        crate::main_thread();
        self.export.drop_leftovers();
    }

    /// Whether an export is in flight: its job or what it returned is
    /// still alive.
    pub fn export_in_flight(&self) -> bool {
        self.export.in_flight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Phase, PhaseCx};
    use tutti_types::SampleRate;

    /// A detached session whose outputs hear a dc node: something to export.
    fn session() -> Session {
        let mut session = Session::builder()
            .build_detached(SampleRate(48_000.0))
            .unwrap();
        let (dc, _) = session
            .graph()
            .insert(tutti_nodes::testing::Const::mono(0.0));
        session.graph().set_outputs_from(dc);
        session
    }

    /// **An unannounced finish leaves the mode to the next
    /// `Phase::RenderMode`**; the announcing one (and `export_blocking`)
    /// announces Realtime itself. No plugin is needed: the mode asserted is
    /// the one the session announced.
    ///
    /// Mutation (run, failed): `finish_export_unannounced` running the
    /// render-mode phase → the mode reads Realtime straight after it.
    #[cfg(feature = "plugin")]
    #[test]
    fn an_unannounced_finish_leaves_the_mode_to_the_phase() {
        use tutti_plugin::RenderMode;
        let mut session = session();
        let request = || {
            ExportRequest::new(
                ExportSource::Mix,
                ExportTarget::Buffers,
                ExportConfig::default(),
                ExportClock::frozen(),
            )
        };
        let job = session.export(request()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.finish_export_unannounced(job.run()).unwrap();
        assert!(!session.export_in_flight(), "the token is released");
        assert_eq!(session.render_mode(), RenderMode::Offline, "not announced");
        // The next export starts with no Realtime in between.
        let job = session.export(request()).unwrap();
        assert_eq!(session.render_mode(), RenderMode::Offline);
        session.finish_export_unannounced(job.run()).unwrap();
        session.run_phase(
            Phase::RenderMode,
            PhaseCx::at(Instant::now()),
            &mut Vec::new(),
        );
        assert_eq!(
            session.render_mode(),
            RenderMode::Realtime,
            "the phase announces"
        );

        session.export_blocking(request()).unwrap();
        assert_eq!(
            session.render_mode(),
            RenderMode::Realtime,
            "the headless finish announces"
        );
    }

    /// **A frozen export's timeline is stopped at beat 0**: what its
    /// `PrepareEdit` is handed (`PreparedGraph::ctx`, the timeline a node
    /// it builds binds to) reads not rolling, at beat 0. No forked node
    /// observes it while the render's transport is stopped; a hook does.
    ///
    /// Mutations (each run, each failed): `Stopped::is_rolling` answering
    /// `true`; `Stopped::beat` answering beat 1.
    #[test]
    fn a_frozen_exports_timeline_is_stopped_at_beat_zero() {
        use std::sync::Mutex;
        let mut session = session();
        let seen: Arc<Mutex<Option<(bool, tutti_core::Beat)>>> = Arc::default();
        let into = Arc::clone(&seen);
        session
            .export_blocking(
                ExportRequest::new(
                    ExportSource::Mix,
                    ExportTarget::Buffers,
                    ExportConfig::default(),
                    ExportClock::frozen(),
                )
                .with_edit(Box::new(move |prepared: PreparedGraph<'_>| {
                    *into.lock().unwrap() = Some((prepared.ctx.is_rolling(), prepared.ctx.beat()));
                })),
            )
            .expect("the export renders");
        let (rolling, beat) = seen.lock().unwrap().expect("the edit ran");
        assert!(!rolling, "a frozen export's timeline must not roll");
        assert_eq!(beat, tutti_core::Beat(0.0));
    }

    /// **An export kept in flight past `EXPORT_STALL_WARNING` is warned
    /// about once**, on the collect's `now`; its clock restarts once
    /// nothing is in flight, so the next leaked export is warned about too.
    ///
    /// Mutations (each run, each failed): the `!stall_warned` guard removed
    /// (three warnings for the first export); the bound removed (a warning
    /// at the first collect); the clock not restarted when nothing is in
    /// flight (the second export warned at its first collect).
    #[test]
    fn a_leaked_export_is_warned_about_once() {
        let mut session = session();
        let collect = |session: &mut Session, at: Instant| {
            session.run_phase(Phase::Collect, PhaseCx::at(at), &mut Vec::new());
            session.export.stall_warnings
        };
        let t0 = Instant::now();
        let job = session
            .export(ExportRequest::new(
                ExportSource::Mix,
                ExportTarget::Buffers,
                ExportConfig::default(),
                ExportClock::frozen(),
            ))
            .expect("the export starts");
        assert_eq!(collect(&mut session, t0), 0, "just started");
        let minute = Duration::from_secs(60);
        assert_eq!(
            collect(&mut session, t0 + EXPORT_STALL_WARNING - minute),
            0,
            "inside the bound"
        );
        assert_eq!(collect(&mut session, t0 + EXPORT_STALL_WARNING), 1);
        assert_eq!(
            collect(&mut session, t0 + EXPORT_STALL_WARNING + minute),
            1,
            "once"
        );
        assert_eq!(collect(&mut session, t0 + 3 * EXPORT_STALL_WARNING), 1);

        // The job dropped: nothing in flight, and the next export's clock
        // starts at its own first collect.
        drop(job);
        let t1 = t0 + 4 * EXPORT_STALL_WARNING;
        assert_eq!(collect(&mut session, t1), 1);
        let job = session
            .export(ExportRequest::new(
                ExportSource::Mix,
                ExportTarget::Buffers,
                ExportConfig::default(),
                ExportClock::frozen(),
            ))
            .expect("the next export starts");
        assert_eq!(collect(&mut session, t1 + minute), 1, "a new clock");
        assert_eq!(
            collect(&mut session, t1 + minute + EXPORT_STALL_WARNING),
            2,
            "the second leak is warned about too"
        );
        drop(job);
    }
}
