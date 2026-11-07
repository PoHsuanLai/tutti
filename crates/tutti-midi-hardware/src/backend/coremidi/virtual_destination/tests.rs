use super::*;
use std::sync::mpsc;
use tutti_ump::{MidiChannel, MidiGroup};

#[test]
fn create_ump_destination() {
    let dst = UmpVirtualDestination::new("Test UMP Dest", |_| {}).expect("creates");
    assert_eq!(dst.name(), "Test UMP Dest");
}

/// The decode half of the receive path, driven directly.
///
/// A CoreMIDI packet is a packed word stream; this is the exact transform the
/// receive callback applies to `packet.data()`. Tested here rather than over a
/// live endpoint because two *virtual* CoreMIDI endpoints in one process are
/// not connected to each other — a virtual source publishes to clients that
/// explicitly connect to it, so a source→destination round trip in-process
/// delivers nothing and would make the test vacuous. Real delivery needs a
/// second process or hardware, which belongs in manual QA, not `cargo test`.
///
/// UMP Stream (type 0xF) is the message family that motivates this endpoint:
/// it has no MIDI-1.0 encoding, so it can only arrive this way.
#[test]
fn packet_words_decode_to_ump_stream_and_channel_voice() {
    use tutti_midi_types::EndpointDiscoveryRequest;
    use tutti_midi_types::ump::UmpMessageType;

    let discovery = MidiEvent::endpoint_discovery(1, 1, EndpointDiscoveryRequest::all());
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(3), 60, 0x8000);

    // One packet carrying both, concatenated exactly as CoreMIDI delivers.
    let mut packet_words = Vec::new();
    packet_words.extend_from_slice(discovery.data_words());
    packet_words.extend_from_slice(note.data_words());

    let decoded: Vec<_> = split_ump_stream(&packet_words).collect();
    assert_eq!(decoded, vec![discovery, note]);
    assert_eq!(decoded[0].message_type(), UmpMessageType::UmpStream);
    assert_eq!(decoded[1].message_type(), UmpMessageType::ChannelVoice2);
}

/// The callback wiring: whatever the decode yields must reach the caller's
/// closure. Exercises the `FnMut(MidiEvent)` seam `new` installs.
#[test]
fn callback_receives_each_decoded_event() {
    let (tx, rx) = mpsc::channel();
    let sink = move |event: MidiEvent| {
        let _ = tx.send(event);
    };

    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let clock = MidiEvent::timing_clock(MidiGroup::FIRST);
    let mut words = Vec::new();
    words.extend_from_slice(note.data_words());
    words.extend_from_slice(clock.data_words());

    // The body of the receive callback, verbatim.
    for event in split_ump_stream(&words) {
        sink(event);
    }

    assert_eq!(rx.recv().expect("note"), note);
    assert_eq!(rx.recv().expect("clock"), clock);
}
