use tutti_ump::MidiChannel;

use super::super::testing::{DST, channel, decoded, header_on, preamble, to_dst, unread};
use super::super::*;

/// Every flag of a set against the bit the table gives it.
fn bits_of<F: bitflags::Flags<Bits = u8> + Copy + std::fmt::Debug>(table: &[(F, u8)]) {
    for &(flag, bit) in table {
        assert_eq!(flag.bits(), 1 << bit, "{flag:?}");
    }
    let all = table.iter().fold(0, |all, (flag, _)| all | flag.bits());
    assert_eq!(F::all().bits(), all);
}

#[test]
fn the_process_features_bitmap_is_d0_report_and_reserved_above_table_42() {
    bits_of(&[(ProcessFeatures::MIDI_MESSAGE_REPORT, 0)]);
    assert_eq!(
        ProcessFeatures::from_bits(0x01),
        Some(ProcessFeatures::MIDI_MESSAGE_REPORT)
    );
    for bit in 1..8 {
        assert_eq!(ProcessFeatures::from_bits(1 << bit), None, "D{bit}");
    }
}

#[test]
fn the_system_bitmap_is_d0_to_d2_of_table_43() {
    bits_of(&[
        (SystemMessages::MTC_QUARTER_FRAME, 0),
        (SystemMessages::SONG_POSITION, 1),
        (SystemMessages::SONG_SELECT, 2),
    ]);
    for bit in 3..8 {
        assert_eq!(SystemMessages::from_bits(1 << bit), None, "D{bit}");
    }
}

#[test]
fn the_channel_controller_bitmap_is_d0_to_d5_of_table_43() {
    bits_of(&[
        (ChannelControllers::PITCH_BEND, 0),
        (ChannelControllers::CONTROL_CHANGE, 1),
        (ChannelControllers::REGISTERED_CONTROLLER, 2),
        (ChannelControllers::ASSIGNABLE_CONTROLLER, 3),
        (ChannelControllers::PROGRAM_CHANGE, 4),
        (ChannelControllers::CHANNEL_PRESSURE, 5),
    ]);
    for bit in 6..8 {
        assert_eq!(ChannelControllers::from_bits(1 << bit), None, "D{bit}");
    }
}

#[test]
fn the_note_data_bitmap_is_d0_to_d4_of_table_43() {
    bits_of(&[
        (NoteData::NOTES, 0),
        (NoteData::POLY_PRESSURE, 1),
        (NoteData::PER_NOTE_PITCH_BEND, 2),
        (NoteData::REGISTERED_PER_NOTE_CONTROLLER, 3),
        (NoteData::ASSIGNABLE_PER_NOTE_CONTROLLER, 4),
    ]);
    for bit in 5..8 {
        assert_eq!(NoteData::from_bits(1 << bit), None, "D{bit}");
    }
}

#[test]
fn the_message_data_control_is_closed_over_table_44() {
    assert_eq!(DataControl::new(0x00), Some(DataControl::Capabilities));
    assert_eq!(DataControl::new(0x01), Some(DataControl::NonDefault));
    assert_eq!(DataControl::new(0x7F), Some(DataControl::Full));
    for raw in (0x02..=0x7E).chain(0x80..=0xFF) {
        assert_eq!(DataControl::new(raw), None, "{raw:#x}");
        assert_eq!(DataControl::try_from(raw), Err(NotADataControl(raw)));
    }
    assert_eq!(u8::from(DataControl::Full), 0x7F);
}

#[cfg(feature = "serde")]
#[test]
fn the_data_control_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: DataControl::new,
        get: DataControl::get,
        of: None,
        valid: &[0x00, 0x01, 0x7F],
        invalid: &[0x02, 0x40, 0x7E, 0x80, 0xFF],
    });
}

#[test]
fn a_system_bitmap_is_unwritable_off_the_function_block() {
    let system = SystemMessages::SONG_SELECT.bits();
    assert_eq!(ReportScope::from_wire(channel(2), system, 0, 0), None);
    assert_eq!(
        ReportScope::from_wire(CiDeviceId::Group, system, 0, 0),
        None
    );
    let scope = ReportScope::from_wire(CiDeviceId::FunctionBlock, system, 0, 0).unwrap();
    assert_eq!(scope.system(), SystemMessages::SONG_SELECT);
    // The channelized bitmaps are at every address, and a Group or Channel
    // scope has an empty system bitmap.
    let group = ReportScope::from_wire(CiDeviceId::Group, 0, 0x01, 0x02).unwrap();
    assert_eq!(group.system(), SystemMessages::empty());
    assert_eq!(group.controllers(), ChannelControllers::PITCH_BEND);
    assert_eq!(group.notes(), NoteData::POLY_PRESSURE);
}

#[test]
fn a_scope_read_from_the_wire_drops_a_reserved_bit() {
    let scope = ReportScope::from_wire(CiDeviceId::FunctionBlock, 0x7F, 0x7F, 0x7F).unwrap();
    assert_eq!(scope.system(), SystemMessages::all());
    assert_eq!(scope.controllers(), ChannelControllers::all());
    assert_eq!(scope.notes(), NoteData::all());
}

#[test]
fn a_reply_scope_within_a_request_keeps_only_what_was_asked() {
    let asked = ReportScope::FunctionBlock {
        system: SystemMessages::SONG_POSITION,
        controllers: ChannelControllers::PITCH_BEND | ChannelControllers::CONTROL_CHANGE,
        notes: NoteData::empty(),
    };
    let reply = ReportScope::FunctionBlock {
        system: SystemMessages::all(),
        controllers: ChannelControllers::CONTROL_CHANGE | ChannelControllers::PROGRAM_CHANGE,
        notes: NoteData::NOTES,
    };
    assert_eq!(
        reply.within(&asked),
        ReportScope::FunctionBlock {
            system: SystemMessages::SONG_POSITION,
            controllers: ChannelControllers::CONTROL_CHANGE,
            notes: NoteData::empty(),
        }
    );
}

#[test]
fn the_capabilities_inquiry_is_table_40_on_the_function_block() {
    let message = CiMessage::ProcessCapabilitiesInquiry {
        header: header_on(CiDeviceId::FunctionBlock),
        to: Muid::of(DST),
    };
    let mut want = preamble(0x7F, 0x40, to_dst());
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    // Per-channel requests are not allowed: the Device ID is 0x7F whatever the
    // header says, and a message on another Device ID is not read.
    let channelized = CiMessage::ProcessCapabilitiesInquiry {
        header: header_on(channel(4)),
        to: Muid::of(DST),
    };
    assert_eq!(channelized.encode(), want);
    want[1] = 0x04;
    assert_eq!(unread(&want).reason, UnreadReason::Malformed);
    want[1] = 0x7E;
    assert_eq!(unread(&want).reason, UnreadReason::Malformed);
}

#[test]
fn the_capabilities_reply_is_table_41_with_the_features_byte() {
    let message = CiMessage::ProcessCapabilitiesReply {
        header: header_on(CiDeviceId::FunctionBlock),
        to: Muid::of(DST),
        features: ProcessFeatures::MIDI_MESSAGE_REPORT,
    };
    let mut want = preamble(0x7F, 0x41, to_dst());
    want.push(0x01);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    // A reserved feature bit of a later revision is dropped; none is not
    // supported.
    *want.last_mut().unwrap() = 0x7F;
    assert_eq!(decoded(&want), message);
    *want.last_mut().unwrap() = 0x00;
    let CiMessage::ProcessCapabilitiesReply { features, .. } = decoded(&want) else {
        panic!()
    };
    assert!(features.is_empty());
    // A missing byte, a byte with the top bit set and another Device ID are
    // malformed.
    assert_eq!(unread(&want[..13]).reason, UnreadReason::Malformed);
    *want.last_mut().unwrap() = 0x80;
    assert_eq!(unread(&want).reason, UnreadReason::Malformed);
    want[1] = 0x00;
    *want.last_mut().unwrap() = 0x01;
    assert_eq!(unread(&want).reason, UnreadReason::Malformed);
}

fn inquiry(control: DataControl, scope: ReportScope) -> CiMessage {
    CiMessage::MessageReportInquiry {
        header: header_on(scope.address()),
        to: Muid::of(DST),
        control,
        scope,
    }
}

#[test]
fn the_message_report_inquiry_is_table_43_byte_for_byte() {
    let scope = ReportScope::FunctionBlock {
        system: SystemMessages::MTC_QUARTER_FRAME | SystemMessages::SONG_SELECT,
        controllers: ChannelControllers::PITCH_BEND | ChannelControllers::CHANNEL_PRESSURE,
        notes: NoteData::NOTES | NoteData::ASSIGNABLE_PER_NOTE_CONTROLLER,
    };
    let message = inquiry(DataControl::Full, scope);
    let mut want = preamble(0x7F, 0x42, to_dst());
    // Control, system bitmap, the reserved byte, controllers, notes.
    want.extend([0x7F, 0b101, 0x00, 0b10_0001, 0b1_0001]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

#[test]
fn the_message_report_inquiry_is_on_the_device_id_of_its_scope() {
    for (scope, id) in [
        (ReportScope::none(channel(0)), 0x00),
        (ReportScope::none(channel(15)), 0x0F),
        (ReportScope::none(CiDeviceId::Group), 0x7E),
        (ReportScope::none(CiDeviceId::FunctionBlock), 0x7F),
    ] {
        let mut message = inquiry(DataControl::Capabilities, scope);
        // The header's own Device ID is not the one sent.
        if let CiMessage::MessageReportInquiry { header, .. } = &mut message {
            header.device_id = channel(9);
        }
        let mut want = preamble(id, 0x42, to_dst());
        want.extend([0x00, 0, 0, 0, 0]);
        assert_eq!(message.encode(), want, "{id:#x}");
        assert_eq!(decoded(&want).header().device_id, scope.address());
    }
}

#[test]
fn a_message_report_inquiry_of_a_channel_carries_its_channelized_bitmaps() {
    let scope = ReportScope::Channel {
        channel: MidiChannel::of(5),
        controllers: ChannelControllers::PROGRAM_CHANGE,
        notes: NoteData::POLY_PRESSURE,
    };
    let message = inquiry(DataControl::NonDefault, scope);
    let mut want = preamble(0x05, 0x42, to_dst());
    want.extend([0x01, 0x00, 0x00, 0b1_0000, 0b10]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

#[test]
fn a_message_report_inquiry_that_table_43_forbids_is_malformed() {
    let mut want = preamble(0x7F, 0x42, to_dst());
    want.extend([0x00, 0, 0, 0, 0]);
    // A reserved Message Data Control.
    for control in [0x02, 0x40, 0x7E] {
        want[13] = control;
        let got = unread(&want);
        assert_eq!(got.reason, UnreadReason::Malformed, "{control:#x}");
        let Some(CiMessage::Nak { nak, .. }) = got.nak(Muid::of(9)) else {
            panic!("a NAK");
        };
        assert_eq!(nak.status_code, Nak::STATUS_MALFORMED);
    }
    want[13] = 0x00;
    // System Messages requested of a Channel or a Group.
    for id in [0x03, 0x7E] {
        let mut asked = want.clone();
        asked[1] = id;
        asked[14] = 0x01;
        assert_eq!(unread(&asked).reason, UnreadReason::Malformed, "{id:#x}");
    }
    // A byte with the top bit set, and a body too short.
    let mut asked = want.clone();
    asked[17] = 0x80;
    assert_eq!(unread(&asked).reason, UnreadReason::Malformed);
    assert_eq!(unread(&want[..17]).reason, UnreadReason::Malformed);
    // The reserved byte is not read, and a reserved bit is dropped.
    let mut asked = want.clone();
    asked[15] = 0x7F;
    asked[16] = 0x7F;
    let CiMessage::MessageReportInquiry { scope, .. } = decoded(&asked) else {
        panic!()
    };
    assert_eq!(scope.controllers(), ChannelControllers::all());
}

#[test]
fn the_message_report_reply_is_table_45_on_the_inquirys_device_id() {
    let scope = ReportScope::Group {
        controllers: ChannelControllers::CONTROL_CHANGE | ChannelControllers::REGISTERED_CONTROLLER,
        notes: NoteData::empty(),
    };
    let message = CiMessage::MessageReportReply {
        header: header_on(CiDeviceId::Group),
        to: Muid::of(DST),
        scope,
    };
    let mut want = preamble(0x7E, 0x43, to_dst());
    want.extend([0x00, 0x00, 0b110, 0x00]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    // System bits on a Group.
    want[13] = 0x01;
    assert_eq!(unread(&want).reason, UnreadReason::Malformed);
}

#[test]
fn the_end_of_message_report_is_table_46_with_no_body() {
    for (address, id) in [
        (channel(7), 0x07),
        (CiDeviceId::Group, 0x7E),
        (CiDeviceId::FunctionBlock, 0x7F),
    ] {
        let message = CiMessage::MessageReportEnd {
            header: header_on(address),
            to: Muid::of(DST),
        };
        let want = preamble(id, 0x44, to_dst());
        assert_eq!(message.encode(), want);
        assert_eq!(decoded(&want), message);
    }
}

#[test]
fn a_process_inquiry_message_to_the_broadcast_muid_is_not_read() {
    for sub in 0x40..=0x44 {
        let bytes = preamble(0x7F, sub, [0x7F; 4]);
        assert_eq!(
            unread(&bytes).reason,
            UnreadReason::WrongDestination,
            "{sub:#x}"
        );
    }
}

#[test]
fn the_sub_id2s_are_those_of_appendix_d_category_4() {
    assert_eq!(SUB_ID2_PROCESS_CAPABILITIES_INQUIRY, 0x40);
    assert_eq!(SUB_ID2_PROCESS_CAPABILITIES_REPLY, 0x41);
    assert_eq!(SUB_ID2_MESSAGE_REPORT_INQUIRY, 0x42);
    assert_eq!(SUB_ID2_MESSAGE_REPORT_REPLY, 0x43);
    assert_eq!(SUB_ID2_MESSAGE_REPORT_END, 0x44);
    // Sub-ID#2 0x45 is reserved.
    assert_eq!(
        unread(&preamble(0x7F, 0x45, to_dst())).reason,
        UnreadReason::UnsupportedMessage
    );
}
