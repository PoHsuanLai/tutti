use super::*;
use tutti_fixtures::midi::note_on;
use tutti_types::Beat;
use tutti_ump::{MidiChannel, MidiGroup};

/// **A clip a block still held when `set_events` replaced it is freed by
/// the next `reclaim`**, with no further publish.
///
/// Mutation (run): `MidiClipControls::reclaim` returning 0 without
/// reclaiming → the first clip is still alive, fails.
#[test]
fn reclaim_frees_a_clip_a_block_held_across_its_replacement() {
    let on = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    let mut node = MidiClipNode::new([TimedMidiEvent::new(Beat(1.0), on)]);
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
    assert_eq!(controls.reclaim(), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}

/// **A clip node's held-note table is 24 KiB**, allocated with the node.
///
/// Mutation (run): `LEDGER_CAPACITY` of 16,384 → 196 KB, fails.
#[test]
fn a_clip_nodes_table_is_24_kib() {
    let node = MidiClipNode::new([]);
    assert_eq!(node.play.held.heap_bytes(), 2_048 * 12);
}

/// A MIDI 1.0 packet on group 1, channel 1.
fn midi1(voice: tutti_ump::Voice1) -> Ump {
    tutti_ump::Message::Midi1(
        tutti_ump::Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::FIRST,
        },
        voice,
    )
    .into()
}

fn cc1(controller: u8, value: u8) -> Ump {
    midi1(tutti_ump::Voice1::ControlChange {
        controller: tutti_ump::CCNumber::of(controller),
        value: tutti_ump::U7::of(value),
    })
}

/// **A clip is translated as it is loaded, in beat order, and keeps its list
/// order** (design 017 §4). An RPN 0 run listed backwards (its CC 38 first,
/// its selects last) is assembled in beat order into one Registered
/// Controller at the CC 38's beat; a MIDI 1.0 Note On at velocity 0 becomes a
/// Note Off (M2-104 D.3.1); and the events keep the list's order, which the
/// note pairing reads, not the beats'.
///
/// Mutation: translate in list order instead of beat order → the CC 38 comes
/// before any select and is dropped → fails. Mutation: return the events in
/// beat order → the order assertion fails.
#[test]
fn a_clip_is_translated_in_beat_order_and_keeps_its_list_order() {
    let note = tutti_ump::NoteNumber::of(60);
    let on0 = midi1(tutti_ump::Voice1::NoteOn {
        note,
        velocity: tutti_ump::U7::of(0),
    });
    let events = [
        TimedMidiEvent::new(Beat(3.0), on0),
        TimedMidiEvent::new(Beat(2.0), cc1(38, 50)),
        TimedMidiEvent::new(Beat(1.0), cc1(6, 12)),
        TimedMidiEvent::new(Beat(0.0), cc1(101, 0)),
        TimedMidiEvent::new(Beat(0.0), cc1(100, 0)),
    ];
    let out = translated(events);
    let decoded: Vec<(f64, tutti_ump::Message)> = out
        .iter()
        .map(|e| (e.beat.get(), tutti_ump::decode(e.event).unwrap()))
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
                        velocity: 0x8000,
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
                        value: ((12 << 7) | 50) << 18,
                    }
                )
            ),
        ]
    );
}
