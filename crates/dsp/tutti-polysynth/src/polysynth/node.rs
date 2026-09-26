//! The synth as a native graph node (doc 013, rewrite item 5): MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! [`MidiClipNode`](tutti_midi_runtime::MidiClipNode), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! The synth's own MIDI port ([`PolySynth::midi_port`]) is still read, for
//! what reaches it outside the graph (a keyboard through its
//! [`MidiSender`](tutti_midi_runtime::MidiSender), an all-notes-off, a clip a
//! host installed on it): each block the port's events and the event
//! input's are merged by offset, the port's first at an equal offset. The
//! port goes when every sender is an event source (doc 013 item 5).

use tutti_core::{AudioUnit, ChannelLayout};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, SortedEvents, Status};

use super::PolySynth;
use crate::fork::SynthFork;

/// The synth's MIDI scratch, in events: [`MAILBOX`] for its port, the rest
/// for its event input.
pub(super) const MIDI_BUFFER: usize = 512;

/// The most events a block takes from the synth's own MIDI port; the rest
/// of [`MIDI_BUFFER`] holds the event input's. Past either, a block's events
/// are dropped (the port's stay queued in its mailbox).
const MAILBOX: usize = 256;

impl PolySynth {
    /// Poll the port into the scratch's first [`MAILBOX`] entries, then
    /// merge `events` in by offset (the port's first at an equal offset).
    /// Returns how many the scratch holds, sorted by offset.
    fn gather_events(&mut self, frames: usize, events: SortedEvents<'_>) -> usize {
        let rate = self.bank.sample_rate();
        self.midi
            .gather(frames, rate, &mut self.midi_buffer, MAILBOX, events)
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
    /// event (its release), and its port is fed out of band, so the executor
    /// must never park it.
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
        let count = self.gather_events(frames, events);
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
/// `Param` handles and MIDI sender are taken from it before it goes in.
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

    /// **The port's events and the event input's are merged by offset, the
    /// port's first at an equal offset.** Port notes at 10 and 300, event
    /// input notes at 5, 10 and 200.
    ///
    /// Mutation: `>` → `>=` in the merge (the event input first on a tie) →
    /// notes 61 and 71 swap → fails. Mutation: skip the port's poll → 61 and
    /// 64 missing → fails.
    #[test]
    fn the_port_and_the_event_input_merge_by_offset() {
        let mut synth = PolySynth::new(SynthConfig::default()).expect("builds");
        let sender = synth.midi_sender();
        assert_eq!(
            sender.queue(&[
                note(61).with_frame_offset(10),
                note(64).with_frame_offset(300)
            ]),
            2
        );
        let at = |k: usize| Offset::new(k, tutti_core::Samples(512)).expect("inside");
        let events = [
            Event::midi(at(5), note(70).data),
            Event::midi(at(10), note(71).data),
            Event::midi(at(200), note(72).data),
        ];
        let sorted = SortedEvents::new(&events, 512).expect("sorted");
        let n = synth.gather_events(512, sorted);
        let got: Vec<(u32, u32)> = synth.midi_buffer[..n]
            .iter()
            .map(|e| (e.frame_offset, (e.data[0] >> 8) & 0x7f))
            .collect();
        assert_eq!(got, [(5, 70), (10, 61), (10, 71), (200, 72), (300, 64)]);
    }
}
