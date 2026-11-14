use super::*;
use crate::virtual_endpoint::SourceBackend;
use tutti_fixtures::midi;
use tutti_ump::{Message, MidiChannel, Ump, Utility};

/// CoreMIDI's endpoint API takes no Function Blocks, so a spec that declares one
/// is refused by name, not opened without it.
#[test]
fn a_spec_with_function_blocks_is_refused_by_name() {
    use tutti_ump::{Direction, FirstGroup, FunctionBlockId, GroupSpan};
    let block = tutti_midi_device::FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        Direction::Output,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .build()
    .unwrap();
    let spec = VirtualSpec::new(tutti_midi_device::EndpointName::new("tutti-blocks").unwrap())
        .with_function_block(block);
    assert!(matches!(
        UmpVirtualSource::open(&spec),
        Err(Error::VirtualBlocksUnsupported)
    ));
}

#[test]
fn send_empty_is_noop() {
    let src = UmpVirtualSource::new("Test UMP Empty", VirtualProtocol::Midi2).expect("creates");
    assert!(src.send(&[]).is_ok());
}

#[test]
fn send_jr_timestamp_word() {
    // A JR Timestamp is a single UMP Utility word — the whole point of this
    // endpoint (it has no MIDI 1.0 form). It must send without error.
    let src = UmpVirtualSource::new("Test UMP JR", VirtualProtocol::Midi2).expect("creates");
    let jr = Ump::from(Message::Utility(Utility::JrTimestamp(0x1234))).words()[0];
    assert!(src.send(&[jr]).is_ok());
}

#[test]
fn send_multiword_note_on() {
    // A MIDI 2.0 channel-voice note-on is two words.
    let src = UmpVirtualSource::new("Test UMP Note", VirtualProtocol::Midi2).expect("creates");
    let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
    assert!(src.send(note.words()).is_ok());
}
