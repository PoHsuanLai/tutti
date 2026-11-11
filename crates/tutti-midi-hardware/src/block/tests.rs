use super::*;
use tutti_ump::Midi1Port;

fn raw(direction: u32, first_group: u32, groups: u32) -> RawBlock<'static> {
    RawBlock {
        id: 0,
        active: true,
        direction,
        first_group,
        groups,
        midi1: 0,
        ui_hint: 0,
        midi_ci_version: 0,
        sysex8_streams: 0,
        name: None,
    }
}

// M2-104 §7.1.8: the direction field is 0b01 input, 0b10 output, 0b11
// bidirectional; 0b00 is reserved and has no block.
#[test]
fn the_direction_field_is_the_specs() {
    let of = |d| raw(d, 0, 1).function_block().map(|b| b.direction());
    assert_eq!(of(1), Some(BlockDirection::Input));
    assert_eq!(of(2), Some(BlockDirection::Output));
    assert_eq!(of(3), Some(BlockDirection::Bidirectional));
    assert_eq!(of(0), None, "reserved");
    assert_eq!(of(4), None, "outside the two bits");
}

// A Function Block number is 0 to 31 (§7.1.8, five bits) and its Groups end by
// Group 16: what the spec cannot hold is refused, not clamped.
#[test]
fn a_block_past_the_numbers_the_spec_has_is_refused() {
    let id = |id| RawBlock { id, ..raw(1, 0, 1) }.function_block();
    assert!(id(31).is_some());
    assert!(id(32).is_none(), "block 32");
    assert!(raw(1, 15, 1).function_block().is_some(), "ends at Group 16");
    assert!(raw(1, 15, 2).function_block().is_none(), "past Group 16");
    assert!(raw(1, 16, 1).function_block().is_none(), "first Group 16");
    assert!(raw(1, 0, 0).function_block().is_none(), "no Groups");
    assert!(raw(1, 300, 1).function_block().is_none(), "not a byte");
}

// §7.1.8: a MIDI 1.0 port is one Group wide; 1 is unrestricted bandwidth and
// 2 is restricted to 31.25 kbps. A span of several Groups is kept as a plain
// block, and 3 (reserved) is not a port.
#[test]
fn the_midi1_field_marks_a_single_group_port() {
    let port = |midi1, groups| {
        RawBlock {
            midi1,
            ..raw(3, 0, groups)
        }
        .function_block()
        .unwrap()
        .midi1()
    };
    assert_eq!(port(0, 1), Midi1Port::No);
    assert_eq!(port(1, 1), Midi1Port::Unrestricted);
    assert_eq!(port(2, 1), Midi1Port::Restricted);
    assert_eq!(port(3, 1), Midi1Port::No, "reserved");
    assert_eq!(port(1, 2), Midi1Port::No, "a span of two");
}

// The UI hint field: 1 receiver, 2 sender, 3 both, anything else unknown; the
// enabled flag, the SysEx 8 stream count and the name come through.
#[test]
fn the_other_fields_come_through() {
    let block = RawBlock {
        id: 7,
        active: false,
        ui_hint: 2,
        sysex8_streams: 3,
        name: Some("Keys"),
        ..raw(2, 4, 2)
    }
    .function_block()
    .unwrap();
    assert_eq!(block.id().get(), 7);
    assert!(!block.active());
    assert_eq!(block.ui_hint(), UiHint::Sender);
    assert_eq!(block.max_sysex8_streams(), 3);
    assert_eq!(block.first_group().get(), 4);
    assert_eq!(block.groups().get(), 2);
    assert_eq!(block.name().map(BlockName::as_str), Some("Keys"));
    let hint = |h| {
        RawBlock {
            ui_hint: h,
            ..raw(1, 0, 1)
        }
        .function_block()
        .unwrap()
        .ui_hint()
    };
    assert_eq!(hint(1), UiHint::Receiver);
    assert_eq!(hint(3), UiHint::Both);
    assert_eq!(hint(9), UiHint::Unknown);
}
