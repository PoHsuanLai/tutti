use super::*;

// NOTE: the single-packet case of `sysex7_fragments` is covered twice over —
// `sysex7_multi_packet` below pins the same two-words-per-packet shape (and
// the packet count), and `sysex7_single_payload_round_trips` recovers the
// identical `[0x7E, 0x7F, 0x06, 0x01]` payload byte for byte.

#[test]
fn sysex7_multi_packet() {
    let mut out = Vec::new();
    let data: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    MidiEvent::sysex7_fragments(MidiGroup::FIRST, &data, &mut out);
    // 15 bytes / 6 per packet = 3 packets
    assert_eq!(out.len(), 3);
    for ev in &out {
        assert_eq!(ev.data_words().len(), 2);
    }
}

#[test]
fn sysex7_single_payload_round_trips() {
    for payload in [
        &[][..],
        &[0x42][..],
        &[0x7E, 0x7F, 0x06, 0x01][..],
        &[1, 2, 3, 4, 5, 6][..],
    ] {
        let ev = MidiEvent::sysex7_single(MidiGroup::FIRST, payload).expect("fits one packet");
        let (status, bytes, n) = ev.sysex7_payload().expect("decodes");
        assert_eq!(status, SYSEX7_STATUS_SINGLE);
        assert_eq!(n, payload.len());
        assert_eq!(&bytes[..n], payload);
    }
    // 7 bytes is more than one packet.
    assert!(MidiEvent::sysex7_single(MidiGroup::FIRST, &[0; 7]).is_none());
}

/// The allocation-free packetizer emits exactly the packets midi2 does,
/// for every length around the 6-byte packet boundary and a long one.
///
/// Mutation: end the last packet as `CONTINUE` instead of `END` (or
/// split at 5 bytes) in `sysex7_packets` and this fails.
#[test]
fn sysex7_packets_match_the_midi2_fragmenter() {
    let data: Vec<u8> = (0..200u32).map(|i| (i * 7 % 128) as u8).collect();
    for len in (0..=40).chain([199, 200]) {
        for group in [MidiGroup::FIRST, MidiGroup::of(5)] {
            let mut expected = Vec::new();
            MidiEvent::sysex7_fragments(group, &data[..len], &mut expected);
            let got: Vec<MidiEvent> = MidiEvent::sysex7_packets(group, &data[..len]).collect();
            assert_eq!(got, expected, "{len} bytes, group {}", group.get());
        }
    }
}

/// The packet count is what `sysex7_packets` yields, at every length
/// around a packet boundary, and an empty payload is one packet.
///
/// Mutation: drop the empty-payload case (`0` for `0`) or round down
/// (`payload_len / 6`) in `sysex7_packet_count` and this fails.
#[test]
fn sysex7_packet_count_is_what_sysex7_packets_yields() {
    let data = [0u8; 40];
    for len in 0..=40 {
        assert_eq!(
            MidiEvent::sysex7_packet_count(len),
            MidiEvent::sysex7_packets(MidiGroup::FIRST, &data[..len]).count(),
            "{len} bytes"
        );
    }
}

/// The words of a single packet from M2-104 Table 18: type 3, group, status 0,
/// byte count, then the bytes; a byte's high bit is dropped.
///
/// Mutation (run): the count nibble shifted by 20 instead of 16, and the mask
/// removed → each case fails.
#[test]
fn sysex7_single_words_are_the_spec_words() {
    let words = |group, payload: &[u8]| {
        MidiEvent::sysex7_single(MidiGroup::of(group), payload)
            .expect("fits one packet")
            .data_words()
            .to_vec()
    };
    assert_eq!(
        words(3, &[0x7E, 0x7F, 0x06, 0x01]),
        [0x3304_7E7F, 0x0601_0000]
    );
    assert_eq!(words(0, &[]), [0x3000_0000, 0]);
    assert_eq!(words(15, &[1, 2, 3, 4, 5, 6]), [0x3F06_0102, 0x0304_0506]);
    assert_eq!(words(0, &[0x81, 0xFF]), [0x3002_017F, 0]);
    assert!(MidiEvent::sysex7_single(MidiGroup::FIRST, &[0; 7]).is_none());
}
