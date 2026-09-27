//! The synth as a graph node: MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! [`MidiClipNode`](tutti_midi_runtime::MidiClipNode), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! A keyboard reaches it the same way: through a `MidiQueueNode` wired to
//! its input. Driven by hand (a test, a bench), its events are handed to it
//! the same way, through `tutti_graph::contract::{drive_in, Direct}`.
//!
//! Its controls are its [`ParamSet`]: the master volume and, with a unison
//! engine, the unison detune and stereo spread, by `UnitParam`. It is a
//! [`ParamNode`], inserted through [`tutti_graph::param_parts`], so a fork
//! starts from the values last set through the set (see `src/fork.rs`).

use tutti_core::{ChannelLayout, UnitParam};
use tutti_graph::{
    Cx, EventKind, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape,
    SortedEvents, Status, Ump,
};
use tutti_midi_types::MidiEvent;

use super::PolySynth;

/// The synth's MIDI scratch, in events: a block's event input past it is
/// dropped.
pub(super) const MIDI_BUFFER: usize = 512;

impl PolySynth {
    /// The block's MIDI from `events` into the scratch, in their (sorted)
    /// order; returns how many it holds.
    fn gather_events(&mut self, events: SortedEvents<'_>) -> usize {
        let mut n = 0;
        for e in events {
            if n == self.midi_buffer.len() {
                break;
            }
            if let EventKind::Midi(Ump(data)) = e.kind {
                self.midi_buffer[n] = MidiEvent {
                    frame_offset: e.offset.get(),
                    data,
                };
                n += 1;
            }
        }
        n
    }
}

impl Node for PolySynth {
    /// No audio in, stereo out, one MIDI event input; the release as its
    /// tail. Events land on their frame ([`Resolution::Sample`](tutti_graph::Resolution::Sample)).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_tail(self.release_tail())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
    }

    /// Always [`Status::Modified`]: the synth sounds after its input's last
    /// event (its release), so the executor must never park it.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        if frames == 0 {
            return Status::Modified;
        }
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics();
        }
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let count = self.gather_events(events);
        let volume = self.master_volume.load().get();
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), right) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        match right {
            Some(right) => self.render_events(frames, count, volume, &mut |i, l, r| {
                left[i] = l;
                right[i] = r;
            }),
            None => self.render_events(frames, count, volume, &mut |i, l, _| left[i] = l),
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.reset_voices();
    }
}

impl ParamNode for PolySynth {
    /// `Volume` (the master gain, as a linear amplitude), and — only when
    /// this synth has a unison engine — `Detune` (cents) and `StereoSpread`
    /// (0..1): the cells the synth reads once per block, and the ones a
    /// control-rate modulation route writes.
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(UnitParam::Volume, self.volume_atomic());
        match (self.detune_atomic(), self.spread_atomic()) {
            (Some(detune), Some(spread)) => set
                .param(UnitParam::Detune, detune)
                .param(UnitParam::StereoSpread, spread),
            _ => set,
        }
        .build()
    }

    /// See the `fork` module docs (`src/fork.rs`).
    fn fork_fresh(&self) -> Self {
        self.fork_instance()
    }
}

/// The synth, inserted with its [`ParamSet`] as its controls and a
/// fork that starts from the values last set through it
/// ([`tutti_graph::param_parts`]; see the `fork` module docs for what a fork
/// carries).
impl IntoNode for PolySynth {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use tutti_graph::{Event, Offset, SortedEvents};
    use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

    use crate::{PolySynth, SynthConfig};

    fn note(n: u8) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xFFFF)
    }

    /// **A graph block plays its event input, on its offsets.**
    ///
    /// Mutation (run): `gather_events` writing every event at index 0 (not
    /// advancing `n`) → no event gathered → fails.
    #[test]
    fn a_graph_block_plays_its_event_input() {
        let mut synth = PolySynth::new(SynthConfig::default()).expect("builds");
        let at = |k: usize| Offset::new(k, tutti_core::Samples(512)).expect("inside");
        let events = [
            Event::midi(at(5), note(70).data),
            Event::midi(at(200), note(72).data),
        ];
        let sorted = SortedEvents::new(&events, 512).expect("sorted");
        let n = synth.gather_events(sorted);
        let got: Vec<(u32, u32)> = synth.midi_buffer[..n]
            .iter()
            .map(|e| (e.frame_offset, (e.data[0] >> 8) & 0x7f))
            .collect();
        assert_eq!(got, [(5, 70), (200, 72)]);
    }
}
