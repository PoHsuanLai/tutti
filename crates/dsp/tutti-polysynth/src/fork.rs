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
mod tests {
    use tutti_core::{SampleRate, Seconds, UnitParam};
    use tutti_graph::contract::{assert_param_fork, Direct};
    use tutti_graph::{Event, Offset, ParamFork};
    use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

    use crate::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig, UnisonConfig};

    const RATE: SampleRate = SampleRate(48_000.0);

    /// A saw with an instant attack, so a note sounds from its own frame.
    fn saw() -> PolySynth {
        PolySynth::new(SynthConfig {
            sample_rate: RATE,
            oscillator: OscillatorType::Saw,
            envelope: EnvelopeConfig {
                attack: Seconds(0.0),
                ..Default::default()
            },
            unison: Some(UnisonConfig::default()),
            ..Default::default()
        })
        .expect("synth builds")
    }

    fn note_on() -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
    }

    /// The peak of channel 0 over `frames` of `synth` with a note on at
    /// frame 0, in 64-frame blocks.
    fn peak(synth: PolySynth, frames: usize) -> f32 {
        let mut hand = Direct::new(synth, RATE, 64);
        let on = Offset::new(0, tutti_core::Samples(64)).expect("inside");
        hand.events(0, &[Event::midi(on, note_on().data)]);
        let mut peak = 0.0f32;
        for _ in 0..frames / 64 {
            hand.block();
            peak = hand.output(0).iter().fold(peak, |a, s| a.max(s.abs()));
        }
        peak
    }

    /// The synth's params by address fork as a `ParamNode`'s must: every one
    /// in its `ParamSet` (volume, and with unison the detune and spread), a
    /// fork starting from the authored value, and no cell shared either way.
    ///
    /// Mutations (run): drop `unison.detach()` from `fork_instance` →
    /// "Detune: a live write reached the fork" → fails; `param_set` leaving
    /// out `StereoSpread` → the list above is short (and
    /// `a_fork_renders_the_volume_and_unison_it_was_taken_with`, which sets
    /// it by address, is refused) → fails.
    #[test]
    fn the_synth_forks_as_a_param_node() {
        let synth = saw();
        let params: Vec<UnitParam> = tutti_graph::ParamNode::param_set(&synth).params().collect();
        assert_eq!(
            params,
            [
                UnitParam::Volume,
                UnitParam::Detune,
                UnitParam::StereoSpread
            ]
        );
        assert_param_fork(synth);
    }

    /// **A fork renders the volume last set through the synth's params**: a
    /// volume set before the fork is the fork's, one set after is not, and
    /// what a modulation driver left in the live cell is not either.
    ///
    /// Mutation (run): `fork_instance` not detaching `master_volume` → the
    /// move after the fork reaches it → fails. Mutation (run): the template
    /// taken detached (`ParamFork::new(&synth.fork_instance())`) → the fork
    /// renders the volume the synth was built with → fails.
    #[test]
    fn a_fork_takes_the_authored_volume_at_the_fork() {
        let at = |before: f32, after: f32| {
            let live = saw();
            let source = ParamFork::new(&live);
            let set = source.params().clone();
            set.set(UnitParam::Volume, before);
            // A modulation driver's composite in the live cell: not authored.
            live.set_volume(before * 3.0);
            let fork = source.fork_node();
            set.set(UnitParam::Volume, after);
            peak(fork, 4_096)
        };
        let half = at(0.5, 0.5);
        assert!(half > 0.0, "the note sounds");
        assert_eq!(at(0.5, 1.0), half, "a move after the fork reached it");
        let full = at(1.0, 1.0);
        assert!(
            (full - 2.0 * half).abs() < 1e-5,
            "the volume set before the fork is the fork's: {full} vs 2 × {half}"
        );
    }
}
