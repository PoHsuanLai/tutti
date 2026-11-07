use super::*;
use tutti_ump::MidiChannel;

#[test]
fn sysex8_single_packet_round_trips() {
    for payload in [
        &[][..],
        &[0x42][..],
        &[0x00, 0xFF, 0x80, 0x7F][..],
        // Exactly 13 bytes — the single-packet limit.
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13][..],
    ] {
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x11, payload, &mut out);
        assert_eq!(out.len(), 1, "{payload:?} should be one packet");
        let (status, stream_id) = out[0].sysex8_status().expect("is a sysex8 packet");
        assert_eq!(status, SYSEX8_STATUS_SINGLE);
        assert_eq!(stream_id, 0x11);
        let (sid, back) = sysex8_message(&out).expect("reassembles");
        assert_eq!(sid, 0x11);
        assert_eq!(back, payload);
    }
}

#[test]
fn sysex8_multi_packet_fragments_and_reassembles() {
    // 30 bytes → 13 + 13 + 4 → Start + Continue + End.
    let data: Vec<u8> = (0u8..30).collect();
    let mut out = Vec::new();
    MidiEvent::sysex8_fragments(MidiGroup::new(2), 0x55, &data, &mut out);
    assert_eq!(out.len(), 3, "30 bytes over 13/packet is three packets");
    assert_eq!(out[0].sysex8_status(), Some((SYSEX8_STATUS_START, 0x55)));
    assert_eq!(out[1].sysex8_status(), Some((SYSEX8_STATUS_CONTINUE, 0x55)));
    assert_eq!(out[2].sysex8_status(), Some((SYSEX8_STATUS_END, 0x55)));

    let (sid, back) = sysex8_message(&out).expect("reassembles");
    assert_eq!(sid, 0x55);
    assert_eq!(back, data, "8-bit payload survives fragmentation");
}

#[test]
fn sysex8_status_rejects_non_sysex8() {
    assert!(
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
            .sysex8_status()
            .is_none()
    );
    assert!(
        sysex8_message(&[MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        )])
        .is_none()
    );
    assert!(sysex8_message(&[]).is_none());
}

#[test]
fn sysex8_carries_full_8bit_bytes() {
    // The whole point of SysEx8 vs SysEx7: bytes with the high bit set.
    let data = [0x80u8, 0xFF, 0xC0, 0x7F, 0x00];
    let mut out = Vec::new();
    MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0, &data, &mut out);
    let (_, back) = sysex8_message(&out).expect("reassembles");
    assert_eq!(back, data);
}
