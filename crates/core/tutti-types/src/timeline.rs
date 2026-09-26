//! [`Timeline`], what a transport-aware node reads, and
//! [`OfflineTransport`], the one shape an offline render hands every node.
//!
//! Both lived in tutti-core until the native graph needed to *name* the
//! offline context. `tutti_graph::ForkMode::Offline` hands it to every forked
//! unit, and tutti-graph cannot depend on tutti-core (tutti-core depends on
//! it), so the context crossed as `&dyn Any` and every unit downcast it. A
//! context of any other type was not an error: every rebind silently did
//! nothing, and transport-aware units rendered against a playhead nothing
//! advanced. The trait names only `Beat` and `Bpm`, so it moved down to the
//! layer every crate already depends on, and the context is typed end to
//! end: `ForkMode::Offline(&OfflineTransport)`,
//! `AudioUnit::rebind_offline(&OfflineTransport)`. A wrong type is now a
//! compile error:
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
/// # When to use this instead of a beat edge
///
/// Pure DSP nodes should **not** implement against this trait. A node that is
/// a function of musical time takes the beat as a signal on its input ports
/// (tutti-core's `BEAT_PORTS`), which is per-sample accurate, works unchanged
/// offline, and makes the timeline→node relationship a visible graph edge.
///
/// What legitimately remains here is what a beat signal cannot express:
///
/// - **Boolean gating**: `is_rolling` drives early returns with state-reset
///   side effects. "Emit nothing" is not the same as "emit a level", and a
///   paused timeline still has a valid beat, so rolling-ness is not
///   recoverable from the beat.
/// - **Nodes with no ports**: the MIDI sources implement `poll_into` and have
///   no `BufferRef` to read a beat from.
/// - **Discontinuities** ([`segment_generation`](Self::segment_generation)): a
///   seek to where the playhead already stands reads the same beat, and only
///   the generation says the timeline jumped.
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
    /// The playhead, as a [`Beat`] position. May be negative during a
    /// count-in.
    fn beat(&self) -> Beat;
    /// The tempo in force, in [`Bpm`].
    fn tempo(&self) -> Bpm;
    /// Whether time is advancing. An offline render is always rolling.
    fn is_rolling(&self) -> bool;
    /// Which segment of the timeline the playhead is on: a count that moves
    /// on at every discontinuity (a seek, a tempo or rate change, a loop
    /// wrap, a play start) and at nothing else. Two reads with the same
    /// generation are on one straight line; a new generation means the
    /// playhead jumped, **even when [`beat`](Self::beat) reads the same**
    /// (a seek to where it stands, or a transport loop exactly one block
    /// long that lands on the beat it left).
    ///
    /// Required, not defaulted: a constant default is exactly the beat-only
    /// key this exists to replace, and a timeline that seeks and forgot to
    /// say so would re-seat nothing on a seek to the same beat. A timeline
    /// that never jumps (a test clock, a stopped export) returns a constant.
    ///
    /// Read **after** [`beat`](Self::beat) when both are wanted: a live
    /// timeline publishes the generation before the beat, so that order can
    /// pair an older beat with a newer generation (a reader re-seats, then
    /// re-seats again at the next beat) but never a newer beat with an older
    /// generation.
    fn segment_generation(&self) -> u64;
}

/// The timeline an offline render advances, one block at a time: what
/// `tutti_graph::ForkMode::Offline` carries and what
/// `AudioUnit::rebind_offline` receives, typed on both sides, so a context
/// of another type is a compile error rather than a rebind that silently
/// does nothing.
///
/// # What a node does on rebind
///
/// Nodes holding a transport re-point at this. Nodes carrying their own
/// internal clock re-seat it from [`Timeline::beat`] and [`Timeline::tempo`]:
/// `isolate()` severs the live links but leaves the clock at whatever beat
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
pub type OfflineTransport = Arc<dyn Timeline>;
