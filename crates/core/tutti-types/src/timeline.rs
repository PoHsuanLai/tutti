//! [`Timeline`], what a transport-aware reader reads, and
//! [`OfflineTransport`], the one shape an offline render hands every node.
//!
//! Both live here, below `tutti-core` and `tutti-graph`, so the graph's
//! `ForkMode::Offline(&OfflineTransport)` can name the offline context by
//! type: a context of the wrong type is a compile error rather than a rebind
//! that silently does nothing.
//!
//! ```compile_fail,E0308
//! fn rebind(ctx: &tutti_types::OfflineTransport) {}
//! rebind(&42u32); // not a timeline: refused, not silently ignored
//! ```

use std::sync::Arc;

use crate::value::{Beat, Bpm};

/// A musical timeline: the position, the tempo, whether it is moving, and
/// which stretch of straight-line time the position is on.
///
/// Implemented by tutti-core's live `Transport` and by its `OfflineTimeline`,
/// so a beat-driven source can be handed either one and not care which.
///
/// # When to use this instead of a block's `Env`
///
/// Graph nodes should **not** implement against this trait. A node that is a
/// function of musical time reads the transport of each frame from its
/// block's `Env` (`tutti_graph::Env::transport_at`, `Env::for_each_beat`),
/// which is per-sample accurate, carries the play state and every change
/// inside the block, and works unchanged offline (a fork's `Env` is the
/// render's).
///
/// What legitimately remains here is a reader outside the graph, which has
/// no `Env`: a host's UI, the control-rate modulation driver, a renderer
/// deciding what to hand its graph next — including
/// [`segment_generation`](Self::segment_generation), the one way to tell a
/// seek to where the playhead already stands from standing still.
///
/// # What is deliberately NOT here
///
/// Recording, looping, and preroll are live-session facts, not timeline
/// facts: an offline render either answers `false`/`None` forever or handles
/// them by direct field access, never through this trait. They live on
/// tutti-core's `TransportState` supertrait (record + loop) and its
/// `TransportSettings` (preroll), read only where a genuinely live transport
/// is required.
pub trait Timeline: Send + Sync {
    /// Returns the playhead, as a [`Beat`] position. May be negative during a
    /// count-in.
    fn beat(&self) -> Beat;
    /// Returns the tempo in force, in [`Bpm`].
    fn tempo(&self) -> Bpm;
    /// Returns whether time is advancing. An offline render is always rolling.
    fn is_rolling(&self) -> bool;
    /// Returns which segment of the timeline the playhead is on: a count that moves
    /// on at every discontinuity (a seek, a tempo or rate change, a loop
    /// wrap, a play start) and at nothing else. Two reads with the same
    /// generation are on one straight line; a new generation means the
    /// playhead jumped, **even when [`beat`](Self::beat) reads the same**
    /// (a seek to where it stands, or a transport loop exactly one block
    /// long that lands on the beat it left).
    ///
    /// Required, not defaulted: a timeline that seeks and forgot to say so
    /// would re-seat nothing on a seek to the same beat. A timeline that never
    /// jumps (a test clock, a stopped export) returns a constant.
    ///
    /// Read **after** [`beat`](Self::beat) when both are wanted: a live
    /// timeline publishes the generation before the beat, so that order can
    /// pair an older beat with a newer generation (a reader re-seats, then
    /// re-seats again at the next beat) but never a newer beat with an older
    /// generation.
    fn segment_generation(&self) -> u64;
}

/// A [`Timeline`] that only an offline render advances: the promise
/// [`OfflineTransport::new`] asks for.
///
/// A marker, so the offline context cannot be the live transport. The live
/// transport is tutti-core's `Transport`, and by the orphan rule only
/// tutti-core (or this crate) could implement this for it, and neither
/// does: handing the live playhead to a fork as its render timeline, which
/// would export whatever the live transport happened to be doing, does not
/// compile. tutti-core's `OfflineTimeline` implements it; so may a test's
/// own clock, or a render's stopped timeline.
pub trait OfflineClock: Timeline {}

/// The timeline an offline render advances, one block at a time.
///
/// What `tutti_graph::ForkMode::Offline` carries to every `ForkSource::fork`,
/// typed on both sides, so a context of another type is a compile error rather
/// than a rebind that silently does nothing.
///
/// A newtype over `Arc<dyn Timeline>`, built only from an [`OfflineClock`]
/// ([`new`](Self::new)): a timeline of the right *type* but the wrong
/// *kind* — the live transport — is refused too. It derefs to the
/// timeline; [`timeline`](Self::timeline) hands out the shared handle for a
/// node that keeps one.
///
/// # What a node does on rebind
///
/// Nodes holding a transport re-point at this. Nodes carrying their own
/// internal clock re-seat it from [`Timeline::beat`] and [`Timeline::tempo`]:
/// severing the live links (`Param::detach`) leaves the clock at whatever beat
/// the *live* playhead held, so without this every beat-driven node (LFO,
/// automation) renders from an arbitrary position and the output depends on
/// *when* the render started. Read at rebind time, before the renderer has
/// advanced anything, so these are the seeded start values rather than a
/// moving position.
///
/// # No scalars beside the timeline
///
/// It carries **no** `start_beat` or `tempo` of its own. Both are things a
/// timeline already answers, and a copy beside it can disagree: one rebind
/// path reading the scalar while another follows the timeline renders half
/// the graph at one tempo and half at another, silently.
#[derive(Clone)]
pub struct OfflineTransport(Arc<dyn Timeline>);

impl OfflineTransport {
    /// Creates the offline context for a render advancing `timeline`.
    pub fn new<T: OfflineClock + 'static>(timeline: Arc<T>) -> Self {
        Self(timeline)
    }

    /// Returns the timeline, as the shared handle a node keeps (a clip
    /// reader's cursor, a voice's transport).
    pub fn timeline(&self) -> Arc<dyn Timeline> {
        Arc::clone(&self.0)
    }
}

impl std::ops::Deref for OfflineTransport {
    type Target = dyn Timeline;

    fn deref(&self) -> &(dyn Timeline + 'static) {
        &*self.0
    }
}

impl std::fmt::Debug for OfflineTransport {
    /// A timeline is not `Debug`; its position is what tells two renders
    /// apart.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineTransport")
            .field("beat", &self.0.beat())
            .field("tempo", &self.0.tempo())
            .finish()
    }
}
