//! What an offline render hands its nodes so they can re-seat themselves.
//!
//! Paired with [`AudioUnit::rebind_offline`](crate::AudioUnit::rebind_offline)
//! and [`PendingClone::isolate_for_offline`](crate::dsp::PendingClone::isolate_for_offline):
//! the clone is severed from live state, then every node is handed one of these
//! to re-point at.
//!
//! The context is passed to nodes as `&dyn Any` (see `rebind_offline`'s docs —
//! `fundsp-tutti` cannot name [`Timeline`]), so this alias is the agreed shape on
//! both sides of that cast. Downcast it with
//! `ctx.downcast_ref::<OfflineTransport>()`.

use std::sync::Arc;

use crate::transport::Timeline;

/// The timeline an offline render advances, one block at a time.
///
/// Nodes holding a transport re-point at this. Nodes carrying their own internal
/// clock (rather than an `Arc<dyn Timeline>`) re-seat it from
/// [`Timeline::beat`] and [`Timeline::tempo`] — the clock is severed by
/// `isolate()` but keeps whatever beat the *live* playhead happened to be at, so
/// without this every beat-driven node (LFO, automation) would render from the
/// wrong position. Read at rebind time, before the renderer has advanced
/// anything, so those are the seeded start values rather than a moving position.
///
/// # Why this is an alias and not a struct
///
/// It was a struct, carrying `start_beat` and `tempo` beside the transport. Both
/// are things a `Timeline` already answers, so the copies could disagree with it
/// — and two rebind paths read different ones:
/// `TransportClock::rebind_offline` re-seated itself from the scalars while
/// `MemorySource::rebind_offline` followed the timeline. A caller who built the
/// context with mismatched values rendered half the graph at one tempo and half
/// at another, silently, with nothing to compare.
///
/// Deleting the scalars left a one-field wrapper around the type every call site
/// actually wanted, so the wrapper went too. `Arc<dyn Timeline>` is `'static` and
/// sized, so it downcasts out of `&dyn Any` exactly as the struct did.
pub type OfflineTransport = Arc<dyn Timeline>;
