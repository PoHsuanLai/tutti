//! Forking a [`PolySynth`] for the native graph's export (`Editor::fork`,
//! design doc 013 PR 12): a fresh synth that shares nothing with the live one
//! and plays the live one's clip on the render's timeline.
//!
//! # Why not a clone of the graph's shadow
//!
//! A host that inserts the synth through `tutti_graph::Legacy::controlled`
//! gets a fork source for free: a clone of the node's **shadow**, isolated
//! when the node was inserted. That copy is wrong for a synth twice over:
//!
//! - **Its MIDI port was severed at insert** (`MidiInPort::isolate`), so it
//!   never saw the clip a host installs on the live port afterwards
//!   (bevy-tutti's `MidiSourceInstall`): the export rendered silence.
//! - **Its `Param` cells were detached at insert**, and the synth's settings
//!   path (`AudioUnit::set`) is a no-op, so no write after insert reaches the
//!   shadow: the master volume and the unison detune and spread are read by
//!   the live synth from cells a host (or a modulation target) writes, and the
//!   export rendered the values the synth was built with.
//!
//! # What this forks from instead
//!
//! A **template**: a clone of the synth taken when the source is made, never
//! processed and deliberately **not** isolated, so it shares the live synth's
//! `Param` cells and its MIDI port (mailbox and source cell). It never polls
//! the port and never renders, so holding it steals nothing. A fork is then,
//! in order:
//!
//! 1. a clone of the template;
//! 2. `AudioUnit::isolate` — detaches the `Param` cells **at their values
//!    now**, so a live move after the fork does not reach the render; mints a
//!    fresh private MIDI port; empties the voices;
//! 3. **offline only:** the live port's source (a `MidiClipSource`) rebound
//!    onto the fork's own port and the render's timeline
//!    ([`MidiInPort::rebind_offline_into`](tutti_midi_runtime::MidiInPort::rebind_offline_into)).
//!    A source that cannot be rebound fails the fork ([`Error::MidiSource`])
//!    rather than render its notes as silence. A live duplicate
//!    ([`ForkMode::Live`]) carries no clip, as a hosted plugin's does not;
//! 4. `AudioUnit::reset`.
//!
//! What the fork does **not** carry: the live mailbox (a keyboard's notes, an
//! all-notes-off), sounding voices, and by-value state the live synth reached
//! through MIDI after the template was taken (a pitch bend, a CC-driven
//! cutoff, an MPE toggle) — the clip replays whatever of that it holds.
//!
//! A `Param` cell read at the fork is its **live value**: the authored base
//! plus whatever a modulation source had added at that instant, the value a
//! `Net` export's `isolate` read too. The render then holds it.

use tutti_core::AudioUnit;
use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked, IntoNode, Legacy};
use tutti_midi_runtime::OfflineRebind;

use crate::{Error, PolySynth};

/// [`PolySynth::fork_source`]'s source: the template in the module docs.
struct SynthFork {
    template: PolySynth,
}

impl SynthFork {
    /// The fork, as a synth: what [`ForkSource::fork`] wraps.
    fn synth(&self, mode: ForkMode<'_>) -> crate::Result<PolySynth> {
        self.template.fork_instance(mode)
    }
}

impl ForkSource for SynthFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let fork = self.synth(mode).map_err(ForkCause::new)?;
        // The fork runs as a host runs the live synth, through `Legacy`;
        // `into_node` so it carries no fork source of its own (a fork is not
        // forked again).
        Ok(Forked::new(Legacy::new(fork).into_node().0))
    }
}

impl PolySynth {
    /// A fresh synth for a fork of the graph this one plays in: the same
    /// config, voices and control values (the `Param` cells read now), a
    /// private MIDI port and no sounding voice, and — for
    /// [`ForkMode::Offline`] — the clip installed on this synth's port,
    /// rebound onto the render's timeline. See the `fork` module docs
    /// (`src/fork.rs`) for the steps and what is not carried.
    ///
    /// Control thread. Reads this synth's port's source cell, never its
    /// mailbox, so this synth keeps every event.
    ///
    /// # Errors
    ///
    /// [`Error::MidiSource`] when a source is installed on this synth's port
    /// that cannot be rebound for an offline render (it is not a function of
    /// a timeline, or `mode`'s context is not one it reads): the render would
    /// drop its notes.
    pub fn fork_instance(&self, mode: ForkMode<'_>) -> crate::Result<PolySynth> {
        let mut fork = self.clone();
        fork.isolate();
        if let ForkMode::Offline(ctx) = mode {
            if self.midi_port().rebind_offline_into(fork.midi_port(), ctx)
                == OfflineRebind::NotRebindable
            {
                return Err(Error::MidiSource);
            }
        }
        fork.reset();
        Ok(fork)
    }

    /// The [`ForkSource`] a host hands the graph's editor when it inserts
    /// this synth, so that a fork of the graph (an export) forks it through
    /// [`fork_instance`](Self::fork_instance) and plays its clip.
    ///
    /// For a host that wraps the synth in its own node builder (bevy-tutti's
    /// `Legacy::controlled`, for a settings ring and a shadow):
    /// `NodeParts { node, controls, fork: Some(synth.fork_source()) }`.
    ///
    /// Take it from the synth that goes into the graph, **before** it goes
    /// in: it keeps a template clone that shares that synth's `Param` cells
    /// and MIDI port (see the `fork` module docs), and costs a second copy of
    /// the synth's voices for as long as the node is in the graph.
    pub fn fork_source(&self) -> Box<dyn ForkSource> {
        Box::new(self.fork_template())
    }

    fn fork_template(&self) -> SynthFork {
        SynthFork {
            template: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tutti_core::transport::{
        OfflineTimeline, OfflineTimelineConfig, OfflineTransport, Timeline, Transport,
    };
    use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, SampleRate, Seconds, MAX_BUFFER_SIZE};
    use tutti_graph::ForkMode;
    use tutti_midi_runtime::{MidiClipSource, MidiInPort, OfflineRebind, TimedClipEvent};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup, MidiUnitId, MidiUnitIn};

    use crate::{EnvelopeConfig, Error, OscillatorType, PolySynth, SynthConfig};

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

    /// A timeline at 87.890625 BPM from beat 0: a beat of exactly 32 768
    /// frames at 48 kHz, which the timeline's per-chunk `f64` beat holds
    /// exactly (doc 013, PR 12's "offline timeline's accumulated beat").
    fn offline() -> Arc<OfflineTimeline> {
        Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(87.890625),
            sample_rate: RATE,
            loop_range: None,
        }))
    }

    /// Render `frames` of channel 0 in 64-frame blocks, advancing `timeline`
    /// past each block after it renders, as a renderer does.
    fn render(unit: &mut PolySynth, timeline: &OfflineTimeline, frames: usize) -> Vec<f32> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut out = Vec::with_capacity(frames);
        while out.len() < frames {
            let n = (frames - out.len()).min(MAX_BUFFER_SIZE);
            unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
            out.extend_from_slice(&output.buffer_ref().channel_f32(0)[..n]);
            timeline.advance(n);
        }
        out
    }

    /// **A fork plays the live synth's clip on the render's timeline, and
    /// leaves the live clip alone.** The clip is installed on the live
    /// synth's port after its fork source was made (as bevy-tutti's
    /// `MidiSourceInstall` does after insert); the fork's note sounds from
    /// beat 1 — frame 32 768 — and not a frame before; the live port still
    /// holds its own source.
    ///
    /// Mutation (run): dropping the `rebind_offline_into` call in
    /// `fork_instance` → the fork renders silence. Mutation (run):
    /// `fork_template` isolating its template (as a `Legacy::controlled`
    /// shadow is) → silence.
    #[test]
    fn a_fork_plays_the_live_clip_on_the_render_timeline() {
        let live = saw();
        let source = live.fork_template();
        live.midi_port().install(Arc::new(MidiClipSource::new(
            live.midi_unit_id(),
            vec![TimedClipEvent {
                beat: Beat(1.0),
                event: note_on(),
            }],
            Arc::new(Transport::new(RATE.0)) as Arc<dyn Timeline>,
        )));

        let timeline = offline();
        // The context is the `OfflineTransport` itself, the type a clip
        // downcasts (`ForkMode::Offline`'s docs).
        let ctx: OfflineTransport = timeline.clone();
        let mut fork = source
            .synth(ForkMode::Offline(&ctx))
            .expect("the synth forks");
        let out = render(&mut fork, &timeline, 40_000);
        // Where the synth's first non-zero sample falls after a note-on at
        // frame 0 (a saw starts from 0, so one frame in): the fork's must
        // fall exactly that far after beat 1.
        let lead = {
            let mut reference = saw();
            reference.midi_sender().queue(&[note_on()]);
            render(&mut reference, &offline(), 64)
                .iter()
                .position(|&s| s != 0.0)
                .expect("a note-on at frame 0 sounds in its block")
        };
        assert_eq!(
            out.iter().position(|&s| s != 0.0),
            Some(32_768 + lead),
            "the note sounds from beat 1"
        );
        assert_eq!(
            live.midi_port()
                .rebind_offline_into(&MidiInPort::new(), &ctx),
            OfflineRebind::Rebound,
            "the live synth still holds its clip"
        );
    }

    /// A MIDI source that is not a function of a timeline.
    struct Unrebindable;

    impl MidiUnitIn for Unrebindable {
        fn poll_unit(
            &self,
            _unit: MidiUnitId,
            _block: usize,
            _rate: SampleRate,
            _buffer: &mut [MidiEvent],
        ) -> usize {
            0
        }
        fn rebind_offline(
            &self,
            _unit: MidiUnitId,
            _ctx: &dyn std::any::Any,
        ) -> Option<Arc<dyn MidiUnitIn>> {
            None
        }
    }

    /// **A source that cannot be rebound fails the fork by name**, offline
    /// (and through the `ForkSource`, as a cause a host can downcast); a
    /// live duplicate carries no clip and does not ask.
    ///
    /// Mutation (run): `fork_instance` ignoring `NotRebindable` → the offline
    /// fork succeeds.
    #[test]
    fn an_unrebindable_source_is_a_named_fork_error() {
        let live = saw();
        live.midi_port().install(Arc::new(Unrebindable));
        let ctx: OfflineTransport = offline();
        assert!(matches!(
            live.fork_instance(ForkMode::Offline(&ctx)),
            Err(Error::MidiSource)
        ));
        let cause = live
            .fork_source()
            .fork(ForkMode::Offline(&ctx))
            .err()
            .expect("the source fails too");
        assert!(matches!(
            cause.downcast_ref::<Error>(),
            Some(Error::MidiSource)
        ));
        assert!(live.fork_instance(ForkMode::Live).is_ok());
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
            let mut fork = source.synth(ForkMode::Live).expect("forks");
            live.set_volume(after);
            fork.midi_sender().queue(&[note_on()]);
            let out = render(&mut fork, &offline(), 4_096);
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
