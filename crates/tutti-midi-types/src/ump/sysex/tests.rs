use super::*;
use tutti_ump::{MidiGroup, Ump};

// NOTE: the single-packet case of `sysex7_fragments` is covered twice over —
// `sysex7_multi_packet` below pins the same two-words-per-packet shape (and
// the packet count), and `sysex7_single_payload_round_trips` recovers the
// identical `[0x7E, 0x7F, 0x06, 0x01]` payload byte for byte.

#[test]
fn sysex7_multi_packet() {
    let mut out = Vec::new();
    let data: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    sysex7_fragments(MidiGroup::FIRST, &data, &mut out);
    // 15 bytes / 6 per packet = 3 packets
    assert_eq!(out.len(), 3);
    for ev in &out {
        assert_eq!(ev.words().len(), 2);
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
        let ev = sysex7_single(MidiGroup::FIRST, payload).expect("fits one packet");
        let (status, bytes, n) = sysex7_payload(&ev).expect("decodes");
        assert_eq!(status, SYSEX7_STATUS_SINGLE);
        assert_eq!(n, payload.len());
        assert_eq!(&bytes[..n], payload);
    }
    // 7 bytes is more than one packet.
    assert!(sysex7_single(MidiGroup::FIRST, &[0; 7]).is_none());
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
            // The original `sysex7_fragments` body, kept here so the
            // comparison stays against midi2 itself.
            use midi2::prelude::*;
            let mut m = midi2::sysex7::Sysex7::<Vec<u32>>::new();
            m.set_group(u4::new(group.get()));
            m.set_payload(data[..len].iter().map(|&b| u7::new(b & 0x7F)));
            let expected: Vec<Ump> = m
                .data()
                .chunks(2)
                .map(|p| {
                    let mut words = [0u32; 4];
                    words[..p.len()].copy_from_slice(p);
                    Ump(words)
                })
                .collect();
            let got: Vec<Ump> = sysex7_packets(group, &data[..len]).collect();
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
            sysex7_packet_count(len),
            sysex7_packets(MidiGroup::FIRST, &data[..len]).count(),
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
        sysex7_single(MidiGroup::of(group), payload)
            .expect("fits one packet")
            .words()
            .to_vec()
    };
    assert_eq!(
        words(3, &[0x7E, 0x7F, 0x06, 0x01]),
        [0x3304_7E7F, 0x0601_0000]
    );
    assert_eq!(words(0, &[]), [0x3000_0000, 0]);
    assert_eq!(words(15, &[1, 2, 3, 4, 5, 6]), [0x3F06_0102, 0x0304_0506]);
    assert_eq!(words(0, &[0x81, 0xFF]), [0x3002_017F, 0]);
    assert!(sysex7_single(MidiGroup::FIRST, &[0; 7]).is_none());
}

/// The old helpers and `tutti_ump::sysex7_packets` produce the same packets,
/// for every length 0..=40 and both groups; a byte's high bit is dropped by
/// the old ones only, so the new writer is fed the masked bytes.
///
/// The two sides share one implementation now, so a change in the writer
/// moves both and is caught by `sysex7_packets_match_the_midi2_fragmenter`
/// and `tutti-ump`'s own cases; this one pins the wrappers.
///
/// Mutation: `sysex7_packets` ignoring its group, `sysex7_fragments` pushing
/// nothing, or `sysex7_single` taking `>= 6` -> fails here.
#[test]
fn the_old_helpers_are_the_ump_writer() {
    use tutti_ump::U7;
    let data: Vec<u8> = (0..40u32).map(|i| (i * 7 % 256) as u8).collect();
    for len in 0..=40 {
        let masked: Vec<U7> = data[..len].iter().map(|&b| U7::masked(b)).collect();
        for group in [MidiGroup::FIRST, MidiGroup::of(9)] {
            let new: Vec<Ump> = tutti_ump::sysex7_packets(group, &masked).collect();
            let old: Vec<Ump> = sysex7_packets(group, &data[..len]).collect();
            assert_eq!(old, new, "sysex7_packets, {len} bytes");
            let mut fragments = Vec::new();
            sysex7_fragments(group, &data[..len], &mut fragments);
            assert_eq!(fragments, new, "sysex7_fragments, {len} bytes");
            assert_eq!(sysex7_packet_count(len), new.len(), "count, {len} bytes");
            assert_eq!(
                sysex7_single(group, &data[..len]),
                (len <= 6).then(|| new[0]),
                "sysex7_single, {len} bytes"
            );
        }
    }
}
