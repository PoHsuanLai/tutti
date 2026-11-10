use super::*;
use tutti_fixtures::midi::note_on;
use tutti_graph::{Event, Offset};
use tutti_types::Samples;
use tutti_ump::{MidiChannel, MidiGroup};

fn note(n: u8) -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xFFFF)
}

/// The events land in order, on their offsets.
///
/// Mutation (run): writing every event at index 0 (not advancing `n`) -> no
/// event gathered -> fails.
#[test]
fn gathers_events_in_order_on_their_offsets() {
    let at = |k: usize| Offset::new(k, Samples(512)).expect("inside");
    let events = [
        Event::midi(at(5), note(70).0),
        Event::midi(at(200), note(72).0),
    ];
    let sorted = SortedEvents::new(&events, 512).expect("sorted");
    let mut out = [(Offset::ZERO, Ump([0; 4])); 8];
    let n = gather_midi(sorted, &mut out);
    let got: Vec<(u32, u32)> = out[..n]
        .iter()
        .map(|e| (e.0.get(), (e.1.0[0] >> 8) & 0x7f))
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
        Event::midi(at(1), note(60).0),
        Event::midi(at(2), note(61).0),
        Event::midi(at(3), note(62).0),
    ];
    let sorted = SortedEvents::new(&events, 512).expect("sorted");
    let mut out = [(Offset::ZERO, Ump([0; 4])); 2];
    assert_eq!(gather_midi(sorted, &mut out), 2);
    assert_eq!(out[1].0.get(), 2);
}
