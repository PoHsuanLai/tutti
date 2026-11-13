//! [`NoteEnvelopeNode`]: an ADSR on a MIDI input, sent to control lanes as
//! exact segments (design doc 014 §6).

use tutti_graph::Cost;
use tutti_modulation::{Adsr, NoteAdsr, SignalKind, SignalSource, SourceId};
use tutti_types::ChannelLayout;

use super::{SourceNode, SourcePorts};

/// A **note envelope**: an ADSR driven by its MIDI input ([`NoteAdsr`]),
/// sent raw in `[0, 1]` as segments addressed `ParamAddr::Id(source.0)`
/// for every control lane that reads `source`.
///
/// - **Exact, no grid**: one segment at each note-on and note-off and at
///   the attack's end. Linear stages are exact through a lane, whose pieces
///   are linear between its points.
/// - **It resets at play start**: on the frame the play clock restarts,
///   every held note is forgotten and the envelope is idle at 0; it sends a
///   0 there unless it was idle already. A note-on on that same frame (a
///   clip's first note) then starts from 0.
/// - **It sends only on events**, so a lane that starts reading it while it
///   holds a sustain (a newly wired route) would read 0 until its next note
///   or stage boundary. An `EventKind::Resync` on its input closes that:
///   it re-sends where it stands, resumed, on the event's frame (doc 014
///   §6). The [`Modulation`](crate::Modulation) reconciler schedules one
///   whenever a lane's program newly reads the envelope.
///
/// Insert it `ForkByClone(node)`: it shares nothing with a clone, and a
/// fork's (the node as inserted, reset) starts idle, as a live play start
/// leaves it.
pub type NoteEnvelopeNode = SourceNode<NoteAdsr>;

impl SourceNode<NoteAdsr> {
    /// A note envelope of `adsr`, sending as `source`.
    #[must_use]
    pub fn new(source: SourceId, adsr: Adsr) -> Self {
        Self::with_source(source, NoteAdsr::new(adsr), ChannelLayout::EMPTY)
    }

    /// The node `decl` describes (what `tutti_modulation::compile` gives each
    /// note envelope): `None` when `decl` is not a note envelope. The rate
    /// comes at `prepare`.
    #[must_use]
    pub fn from_decl(decl: &SignalSource) -> Option<Self> {
        match decl.kind {
            SignalKind::NoteEnvelope { adsr } => Some(Self::new(decl.id, adsr)),
            SignalKind::Follower { .. } => None,
        }
    }
}

impl SourcePorts for NoteAdsr {
    /// It works only on a note, a stage boundary or a play start, a few
    /// comparisons and one segment each.
    const COST: Cost = Cost::Light;
    const MIDI_IN: bool = true;
}
