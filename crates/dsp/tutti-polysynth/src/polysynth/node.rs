//! The synth as a native graph node (doc 013, rewrite item 5): MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! [`MidiClipNode`](tutti_midi_runtime::MidiClipNode), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! A keyboard reaches it the same way: through a `MidiQueueNode` wired to
//! its input. What [`queue_midi`](PolySynth::queue_midi) was given is for a
//! synth driven by hand, and a graph block drops it.

use tutti_core::{AudioUnit, ChannelLayout};
use tutti_graph::{
    Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, SortedEvents, Status, Ump,
};
use tutti_midi_types::MidiEvent;

use super::PolySynth;
use crate::fork::SynthFork;

/// The synth's MIDI scratch, in events: a block's event input past it is
/// dropped.
pub(super) const MIDI_BUFFER: usize = 512;

impl PolySynth {
    /// The block's MIDI from `events` into the scratch, in their (sorted)
    /// order; returns how many it holds.
    fn gather_events(&mut self, events: SortedEvents<'_>) -> usize {
        self.pending = 0;
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
        AudioUnit::set_sample_rate(self, p.sample_rate());
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
        AudioUnit::reset(self);
    }
}

/// The synth, inserted natively: its fork is a native synth too (see the
/// `fork` module docs for what a fork carries). No controls: the synth's
/// `Param` handles are taken from it before it goes in.
impl IntoNode for PolySynth {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let fork = SynthFork::native(&self);
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
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

    /// **A graph block plays its event input, on its offsets, and drops what
    /// was queued by hand.**
    ///
    /// Mutation (run): not clearing `pending` → the queued note stays for the
    /// next hand-driven block → fails.
    #[test]
    fn a_graph_block_plays_its_event_input() {
        let mut synth = PolySynth::new(SynthConfig::default()).expect("builds");
        assert_eq!(synth.queue_midi(&[note(61)]), 1);
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
        assert_eq!(synth.take_pending_sorted(), 0);
    }
}
