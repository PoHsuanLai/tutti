use super::*;
use tutti_core::Samples;
use tutti_graph::{Event, Offset};
use tutti_midi_types::{MidiChannel, MidiGroup};

fn note(n: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xFFFF)
}

/// The events land in order, on their offsets.
///
/// Mutation (run): writing every event at index 0 (not advancing `n`) -> no
/// event gathered -> fails.
#[test]
fn gathers_events_in_order_on_their_offsets() {
    let at = |k: usize| Offset::new(k, Samples(512)).expect("inside");
    let events = [
        Event::midi(at(5), note(70).data),
        Event::midi(at(200), note(72).data),
    ];
    let sorted = SortedEvents::new(&events, 512).expect("sorted");
    let mut out = [MidiEvent::noop(); 8];
    let n = gather_midi(sorted, &mut out);
    let got: Vec<(u32, u32)> = out[..n]
        .iter()
        .map(|e| (e.frame_offset, (e.data[0] >> 8) & 0x7f))
        .collect();
    assert_eq!(got, [(5, 70), (200, 72)]);
}

/// A full buffer takes the first events and drops the rest.
///
/// Mutation (run): removing the `n == out.len()` check -> index out of
/// bounds -> fails.
#[test]
fn a_full_buffer_keeps_the_first_events() {
    let at = |k: usize| Offset::new(k, Samples(512)).expect("inside");
    let events = [
        Event::midi(at(1), note(60).data),
        Event::midi(at(2), note(61).data),
        Event::midi(at(3), note(62).data),
    ];
    let sorted = SortedEvents::new(&events, 512).expect("sorted");
    let mut out = [MidiEvent::noop(); 2];
    assert_eq!(gather_midi(sorted, &mut out), 2);
    assert_eq!(out[1].frame_offset, 2);
}
