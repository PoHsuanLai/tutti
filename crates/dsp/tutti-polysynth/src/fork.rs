//! Forking a [`PolySynth`] for the graph's export (`Editor::fork`): a fresh
//! synth that shares nothing with the live one. Its MIDI comes from its event
//! input, as the live synth's does: the fork of the graph forks the clip node
//! feeding it too.
//!
//! # A `ParamNode` fork
//!
//! The synth is a `tutti_graph::ParamNode`, inserted through
//! `tutti_graph::param_parts`: the editor keeps a **template** (a clone taken
//! at insert, never processed, sharing the live synth's `Param` cells) and
//! the synth's `ParamSet`. A fork is the template's
//! [`fork_fresh`](tutti_graph::ParamNode::fork_fresh) —
//! [`fork_instance`](PolySynth::fork_instance): a clone with its `Param`
//! cells **detached** (so a live move after the fork does not reach the
//! render, nor a move on the fork the live synth) and every voice silenced —
//! and then each param set to its **authored** value: what the host last
//! set through the `ParamSet`, not whatever a modulation source had added to
//! the cell at the instant of the fork (the export runs its own modulation).
//!
//! What the fork does **not** carry: sounding voices, and by-value state the
//! live synth reached through MIDI after the template was taken (a pitch
//! bend, a CC-driven cutoff, an MPE toggle) — the clip replays whatever of
//! that it holds.

use crate::PolySynth;

impl PolySynth {
    /// Returns a fresh synth for a fork of the graph this one plays in (an
    /// offline export, for example).
    ///
    /// The copy has the same config and unison settings, control cells of its
    /// own (at the values this synth's hold now, so later moves on either side
    /// do not reach the other), and no sounding voice. State the live synth
    /// reached through MIDI (a pitch bend, a CC-driven cutoff) is carried as
    /// it is now. Allocates a copy of every voice, so call it off the audio
    /// thread.
    ///
    /// A synth inserted into a graph registers this as its fork, so a graph
    /// fork needs no call to it; it is for hosts building a copy by hand.
    #[must_use]
    pub fn fork_instance(&self) -> PolySynth {
        let mut fork = self.clone();
        // Control cells: detached at their current values, so the fork
        // renders the controls it was taken with rather than following live
        // moves (and a move on the fork never reaches the live synth).
        fork.detach_controls();
        // A clean, inactive voice set: a clone carries the live synth's
        // sounding notes, which a fork must not replay.
        fork.reset_voices();
        fork
    }
}

#[cfg(test)]
mod tests;
