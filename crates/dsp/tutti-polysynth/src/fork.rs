//! Forking a [`PolySynth`] for the graph's export (`Editor::fork`, design
//! doc 013 PR 12): a fresh synth that shares nothing with the live one. Its
//! MIDI comes from its event input, as the live synth's does: the fork of
//! the graph forks the clip node feeding it too (doc 013, rewrite item 5).
//!
//! # Why not a clone of the graph's shadow
//!
//! A host that inserts the synth through `tutti_graph::Legacy::controlled`
//! gets a fork source for free: a clone of the node's **shadow**, isolated
//! when the node was inserted. Its `Param` cells were detached then, and the
//! synth's settings path (`AudioUnit::set`) is a no-op, so no write after
//! insert reaches the shadow: the master volume and the unison detune and
//! spread are read by the live synth from cells a host (or a modulation
//! target) writes, and the export rendered the values the synth was built
//! with.
//!
//! # What this forks from instead
//!
//! A **template**: a clone of the synth taken when the source is made, never
//! processed and deliberately **not** isolated, so it shares the live synth's
//! `Param` cells. A fork is then a clone of the template,
//! `AudioUnit::isolate` (detaching the `Param` cells **at their values now**,
//! so a live move after the fork does not reach the render, and emptying the
//! voices), then `AudioUnit::reset`.
//!
//! What the fork does **not** carry: sounding voices, and by-value state the
//! live synth reached through MIDI after the template was taken (a pitch
//! bend, a CC-driven cutoff, an MPE toggle) — the clip replays whatever of
//! that it holds.
//!
//! A `Param` cell read at the fork is its **live value**: the authored base
//! plus whatever a modulation source had added at that instant. The render
//! then holds it.

use tutti_core::AudioUnit;
use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked, IntoNode, Legacy};

use crate::PolySynth;

/// [`PolySynth::fork_source`]'s source: the template in the module docs.
pub(crate) struct SynthFork {
    template: PolySynth,
    /// Whether the fork is a native node (the synth was inserted as one,
    /// `IntoNode for PolySynth`) rather than a `Legacy` one.
    native: bool,
}

impl SynthFork {
    /// The source of a synth inserted as a native node.
    pub(crate) fn native(synth: &PolySynth) -> Self {
        Self {
            template: synth.clone(),
            native: true,
        }
    }

    /// The fork, as a synth: what [`ForkSource::fork`] wraps.
    fn synth(&self) -> PolySynth {
        self.template.fork_instance()
    }
}

impl ForkSource for SynthFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let fork = self.synth();
        // The fork runs as the host runs the live synth: natively, or
        // through `Legacy` (`into_node`, so it carries no fork source of its
        // own: a fork is not forked again).
        Ok(Forked::new(if self.native {
            Box::new(fork)
        } else {
            Legacy::new(fork).into_node().0
        }))
    }
}

impl PolySynth {
    /// A fresh synth for a fork of the graph this one plays in: the same
    /// config, voices and control values (the `Param` cells read now), and no
    /// sounding voice. See the `fork` module docs (`src/fork.rs`).
    pub fn fork_instance(&self) -> PolySynth {
        let mut fork = self.clone();
        fork.isolate();
        fork.reset();
        fork
    }

    /// The [`ForkSource`] a host hands the graph's editor when it inserts
    /// this synth, so that a fork of the graph (an export) forks it through
    /// [`fork_instance`](Self::fork_instance).
    ///
    /// For a host that wraps the synth in its own node builder (bevy-tutti's
    /// `Legacy::controlled`, for a settings ring and a shadow):
    /// `NodeParts { node, controls, fork: Some(synth.fork_source()) }`.
    ///
    /// Take it from the synth that goes into the graph, **before** it goes
    /// in: it keeps a template clone that shares that synth's `Param` cells
    /// (see the `fork` module docs), and costs a second copy of
    /// the synth's voices for as long as the node is in the graph.
    pub fn fork_source(&self) -> Box<dyn ForkSource> {
        Box::new(self.fork_template())
    }

    fn fork_template(&self) -> SynthFork {
        SynthFork {
            template: self.clone(),
            native: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use tutti_core::{AudioUnit, BufferVec, SampleRate, Seconds, MAX_BUFFER_SIZE};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    use crate::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig};

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
            ..Default::default()
        })
        .expect("synth builds")
    }

    fn note_on() -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
    }

    /// Render `frames` of channel 0 in 64-frame blocks.
    fn render(unit: &mut PolySynth, frames: usize) -> Vec<f32> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut out = Vec::with_capacity(frames);
        while out.len() < frames {
            let n = (frames - out.len()).min(MAX_BUFFER_SIZE);
            unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
            out.extend_from_slice(&output.buffer_ref().channel_f32(0)[..n]);
        }
        out
    }

    /// **A fork renders the controls as they stood at the fork**: a volume
    /// set on the live synth after its source was made reaches the fork (the
    /// cell is read at fork time), and one set after the fork does not
    /// (`isolate` detached the cell).
    ///
    /// Mutation (run): dropping `self.master_volume.detach()` from
    /// `PolySynth::isolate` → the move after the fork reaches it. Mutation
    /// (run): `fork_template` isolating its template → the fork renders the
    /// volume the synth was built with.
    #[test]
    fn a_fork_takes_the_controls_at_the_fork() {
        let peak = |before: f32, after: f32| {
            let live = saw();
            let source = live.fork_template();
            live.set_volume(before);
            let mut fork = source.synth();
            live.set_volume(after);
            fork.queue_midi(&[note_on()]);
            let out = render(&mut fork, 4_096);
            out.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
        };
        let half = peak(0.5, 0.5);
        assert!(half > 0.0, "the note sounds");
        assert_eq!(peak(0.5, 1.0), half, "a move after the fork reached it");
        let full = peak(1.0, 1.0);
        assert!(
            (full - 2.0 * half).abs() < 1e-5,
            "the volume set before the fork is the fork's: {full} vs 2 × {half}"
        );
    }
}
