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

use tutti_core::NodeId;
use tutti_export::{ExportConfig, Normalize, RenderClock, Rendered, Written};

use tutti_core::transport::OfflineContext;

/// Which audio to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSource {
    /// The whole live graph, as the master hears it. One clone of the net, no
    /// per-node isolation.
    Master,
    /// One node's output, isolated from everything downstream of it — "what
    /// does this point in the graph actually sound like".
    ///
    /// # Cost
    ///
    /// Each of these does one **main-thread deep clone** of the live net
    /// (`clone_isolated` → `DynClone` of every DSP node) in the frame it is
    /// admitted. Spawning many in a single frame runs those clones back to back
    /// on the main thread and can stall the frame long enough to underrun the
    /// audio callback — an audible glitch.
    ///
    /// This crate deliberately does not throttle that for you: a cap here would
    /// make unrelated consumers queue behind each other, and only the caller
    /// knows which of its own requests matters most. Gate your spawn system
    /// instead —
    ///
    /// ```ignore
    /// use bevy_ecs::prelude::*;
    /// app.add_systems(Update, spawn_my_taps
    ///     .run_if(not(any_with_component::<ExportInFlight>)));
    /// ```
    ///
    /// — and pick the single best candidate when several are pending.
    Node(NodeId),
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
        path: PathBuf,
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
    pub source: ExportSource,
    pub target: ExportTarget,
    /// Rate, duration, format, bit depth, channel width, resample, dither.
    pub config: ExportConfig,
    /// The clock the render advances, one block at a time. Use
    /// `tutti_export::FrozenClock` for a graph with no time-dependent nodes, or
    /// an `OfflineTimeline` seeded at the beat you want to render from.
    pub clock: Arc<dyn RenderClock>,
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
/// Public so callers can gate their own spawning on it — this component *is*
/// the throttling API:
///
/// ```ignore
/// .run_if(not(any_with_component::<ExportInFlight>))
/// ```
///
/// There is deliberately no cap, priority field or slot counter in this crate;
/// see [`ExportSource::Node`] for why.
#[derive(Component)]
pub struct ExportInFlight {
    pub(crate) task: Task<tutti_export::Result<ExportOutput>>,
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
/// ```ignore
/// commands
///     .spawn(ExportRequest { .. })
///     .observe(move |done: On<ExportDone>, mut commands: Commands| {
///         // `view_entity` and friends are captured here.
///     });
/// ```
#[derive(EntityEvent, Debug)]
pub struct ExportDone {
    pub entity: Entity,
    pub result: tutti_export::Result<ExportOutput>,
}

/// Fill a cloned, isolated net's voices before it is rendered.
///
/// # Why this is a trait and not a system set
///
/// An isolated clone is born **empty**: `isolate()` drops the voice pools'
/// command channels and clears their voices, so nothing downstream of a sampler
/// makes a sound until something re-inserts them. The data needed to do that —
/// which clips exist, and their decoded audio — lives in the *app's* ECS, which
/// this crate cannot see.
///
/// The predecessor solved that by leaving an empty `Populate` system set in the
/// middle of a three-step pipeline for a stranger crate to fill, plus a public
/// `&mut dyn AudioUnit` escape hatch into a half-built net. Correctness then
/// depended on an unenforced scheduling contract spanning three crates.
///
/// Passing the filler *in* collapses that to one system: register an
/// implementation as a resource with
/// [`App::insert_resource`], and it is called on the clone before it reaches
/// the worker.
///
/// Registering none is fine — a graph with no voices needs no filling.
pub trait PopulateNet: Send + Sync + 'static {
    /// Insert whatever voices this net should render with.
    ///
    /// `world` is read-only on purpose: filling a render is a *read* of the
    /// app's state, and a `&mut World` here would let it mutate the app from
    /// inside the export pipeline — the sort of back-channel the projection
    /// arrow is not supposed to have.
    ///
    /// `ctx` carries the render's offline transport: voices built here must be
    /// bound to it, not to the live one, or they read a playhead nothing
    /// advances.
    fn populate(&self, net: &mut tutti_core::dsp::Net, ctx: &OfflineContext, world: &World);
}

/// Holds the app's [`PopulateNet`] implementation, if it registered one.
#[derive(Resource)]
pub struct NetPopulator(pub Box<dyn PopulateNet>);

impl NetPopulator {
    pub fn new(populator: impl PopulateNet) -> Self {
        Self(Box::new(populator))
    }
}
