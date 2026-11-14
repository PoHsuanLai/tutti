use super::*;

use tutti_fixtures::midi;
use tutti_types::Beat;
use tutti_ump::{CCNumber, ControlValue, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7};

/// **A clip a block still held when `set` replaced it is freed by
/// the next `reclaim`**, with no further publish.
#[test]
fn reclaim_frees_a_clip_a_block_held_across_its_replacement() {
    let on = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let mut node = MidiClipNode::new([TimedMidiEvent::new(Beat(1.0), on).unwrap()]);
    let controls = MidiClipControls {
        shared: Arc::clone(&node.shared),
    };
    let gone = Arc::downgrade(&node.shared.current());
    {
        // A block in progress across the replacement.
        let _held = node.clip.lend();
        controls.clear();
    }
    assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
    assert_eq!(tutti_rt::Reclaim::reclaim(&controls), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}

/// **A clip node's held-note table is 24 KiB**, allocated with the node.
#[test]
fn a_clip_nodes_table_is_24_kib() {
    let node = MidiClipNode::new([]);
    assert_eq!(node.play.held.heap_bytes(), 2_048 * 12);
}

/// **A clip is translated as it is loaded, in beat order, and keeps its list
/// order**. An RPN 0 run listed backwards (its CC 38 first,
/// its selects last) is assembled in beat order into one Registered
/// Controller at the CC 38's beat; a MIDI 1.0 Note On at velocity 0 becomes a
/// Note Off (M2-104 D.3.1); and the events keep the list's order, which the
/// note pairing reads, not the beats'.
#[test]
fn a_clip_is_translated_in_beat_order_and_keeps_its_list_order() {
    let note = NoteNumber::of(60);
    let on0 = Ump::from(Message::midi1(
        MidiChannel::FIRST,
        Voice1::note_on(note, U7::of(0)),
    ));
    let events = [
        TimedMidiEvent::new(Beat(3.0), on0).unwrap(),
        TimedMidiEvent::new(
            Beat(2.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::DATA_ENTRY_LSB, U7::of(50)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat(1.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::DATA_ENTRY, U7::of(12)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat(0.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::RPN_MSB, U7::of(0)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            Beat(0.0),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::control_change(CCNumber::RPN_LSB, U7::of(0)),
            ),
        )
        .unwrap(),
    ];
    let out = translated(events);
    let decoded: Vec<(f64, tutti_ump::Message)> = out
        .iter()
        .map(|e| (e.beat().get(), e.event().decode().unwrap()))
        .collect();
    let address = tutti_ump::Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::FIRST,
    };
    assert_eq!(
        decoded,
        vec![
            (
                3.0,
                tutti_ump::Message::Midi2(
                    address,
                    tutti_ump::Voice2::NoteOff {
                        note,
                        velocity: NoteVelocity::CENTER,
                        attribute: tutti_ump::Attribute::None,
                    }
                )
            ),
            (
                2.0,
                tutti_ump::Message::Midi2(
                    address,
                    tutti_ump::Voice2::Registered {
                        bank: tutti_ump::U7::of(0),
                        index: tutti_ump::U7::of(0),
                        // Pitch bend sensitivity 12 semitones 50 cents,
                        // zero-extended (M2-115 section 4.1).
                        value: ControlValue::new(((12 << 7) | 50) << 18),
                    }
                )
            ),
        ]
    );
}
