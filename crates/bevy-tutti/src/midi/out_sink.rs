//! [`MidiOutSinkRes`] — the collection point a graph node's MIDI-out lands in.
//!
//! The outbound mirror of [`bus`](super::bus): where `MidiBusRes` is what the
//! *inbound* phase queues into, this is what an emitting node pushes into for
//! the *outbound* phase to fan out after the graph renders.

use bevy_ecs::prelude::*;
use tutti_core::Arc;

/// The buffer emitting nodes push their MIDI-out into, drained once per block by
/// the RT [`MidiPostBlock`].
///
/// **Must be the sink the RT post-block was built with.** The engine build takes
/// `MidiPostBlock::sink()` and inserts this resource from that same `Arc`, so a
/// node handed a clone pushes where the audio thread actually reads. Construct a
/// second one and every emitted event lands in a buffer nothing drains: a
/// plugin's MIDI-out silently vanishes, with no error and nothing in the log.
///
/// Hence no `Default` and a private field — exactly the hazard, and exactly the
/// guard, that [`MidiBusRes`](super::MidiBusRes) documents.
///
/// [`MidiPostBlock`]: tutti_midi_runtime::MidiPostBlock
#[derive(Resource, Clone)]
pub struct MidiOutSinkRes(pub(crate) Arc<tutti_midi_runtime::MidiOutSink>);

// Hand-rolled: the sink wraps an audio-thread event buffer with no `Debug`, and
// reading its contents here would mean touching RT state off the audio thread.
// Report what is inspectable without borrowing — whether anything overflowed.
impl std::fmt::Debug for MidiOutSinkRes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiOutSinkRes")
            .field("overflowed", &self.0.overflowed())
            .finish_non_exhaustive()
    }
}

impl MidiOutSinkRes {
    /// Wrap the sink the RT post-block shares. Crate-internal: a host that built
    /// its own would be building the failure above.
    pub(crate) fn new(sink: Arc<tutti_midi_runtime::MidiOutSink>) -> Self {
        Self(sink)
    }

    /// A clone of the shared sink, for handing to an emitting node.
    ///
    /// `Arc::clone`, not a new buffer — the point is that every emitter and the
    /// post-block share one collection point.
    ///
    /// ```ignore
    /// // A host routes a hosted plugin's MIDI-out back into the graph:
    /// plugin.set_midi_out(sink.handle());
    /// ```
    pub fn handle(&self) -> Arc<tutti_midi_runtime::MidiOutSink> {
        Arc::clone(&self.0)
    }
}

// # Why this is handed out on request, and not installed automatically
//
// [`register_midi_senders`](super::registration::register_midi_senders) puts
// *every* MIDI-capable node's sender on the bus unasked, and that is right: an
// inbox is an **address**, it costs one map slot, and being reachable is what
// lets a route arrive later. The outbound direction is not symmetric with it.
//
// Installing a sink says "fan this node's emissions out over the routing table",
// which is a **routing decision**, not an address. Three things go wrong if the
// registration pass makes it for you:
//
// - **The type cannot answer the question.** Resolution is a downcast per
//   registered concrete type, but a plugin's MIDI-out capability is a per-
//   instance negotiated fact (`Features::MIDI_OUT`), so `register::<PluginClient>`
//   cannot tell an emitting instance from a silent one.
// - **A second lookup path drifts from the first.** `MidiTargetRegistry`'s own
//   docs record this happening — a supplied-target mechanism beside port
//   resolution, where the two disagreed and the supplied handle was unreachable
//   in practice. The rule that came out of it: *a target is its node's port, or
//   it is nothing.*
// - **Routing is by channel.** Two plugins emitting on one channel would merge
//   into the same destination silently. Inbound that is intended (one keyboard,
//   many synths); outbound nobody asked for it.
//
// Revisit only when a node type exists whose *type* means "always emits" — a
// MIDI generator — because then `register` genuinely knows the answer.
