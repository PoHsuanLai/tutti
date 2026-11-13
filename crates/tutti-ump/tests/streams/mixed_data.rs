//! Mixed Data Sets (M2-104 section 7.9): the header fields, the chunk
//! numbering and ending rules of sections 7.9 and 7.9.1, and the assembler's
//! drop counting, checked against UMPs built from the spec's fields as well as
//! against the writer.

use tutti_ump::{
    Data128, ManufacturerId, MdsChunk, MdsChunkSize, MdsChunkUmps, MdsChunks, MdsIdentity, Message,
    MidiGroup, MixedDataAssembler, MixedDataHeader, MixedDataSet, MixedDataStep, Sysex8Packet,
    SysexStatus, U4, Ump,
};

const G0: MidiGroup = MidiGroup::of(0);
const ID: MdsIdentity = MdsIdentity {
    manufacturer: ManufacturerId::NON_COMMERCIAL,
    device: 0xFFFF,
    sub_id_1: 0x0102,
    sub_id_2: 0x0304,
};

fn assembler(lanes: usize) -> MixedDataAssembler<Vec<u8>> {
    MixedDataAssembler::new(vec![0; 256 * lanes], 256, lanes)
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| 1 + i as u8).collect()
}

/// A header UMP from the spec's fields.
fn header(group: MidiGroup, mds: u8, size: u16, chunks: u16, chunk: u16) -> Ump {
    Ump::from(Message::Data128(
        group,
        Data128::MixedDataHeader(MixedDataHeader {
            mds_id: U4::of(mds),
            chunk_size: MdsChunkSize::of(size),
            chunks: MdsChunks::new(chunks),
            chunk: MdsChunk::new(chunk),
            identity: ID,
        }),
    ))
}

fn payload(group: MidiGroup, mds: u8, bytes: &[u8]) -> Ump {
    let mut padded = [0u8; 14];
    padded[..bytes.len()].copy_from_slice(bytes);
    Ump::from(Message::Data128(
        group,
        Data128::MixedDataPayload {
            mds_id: U4::of(mds),
            bytes: padded,
        },
    ))
}

/// What a stream of UMPs completes: group, MDS id, identity, data.
type Done = (u8, u8, MdsIdentity, Vec<u8>);

fn feed(a: &mut MixedDataAssembler<Vec<u8>>, umps: impl IntoIterator<Item = Ump>) -> Vec<Done> {
    umps.into_iter()
        .filter_map(|u| match a.push(u) {
            MixedDataStep::Complete(s) => {
                Some((s.group.get(), s.mds_id.get(), s.identity, s.data.to_vec()))
            }
            _ => None,
        })
        .collect()
}

fn set(group: MidiGroup, mds: u8, data: &[u8]) -> MixedDataSet<'_> {
    MixedDataSet {
        group,
        mds_id: U4::of(mds),
        identity: ID,
        data,
    }
}

/// Section 7.9, Number of Valid Bytes: the size counts the header and the
/// 16-byte UMPs, so 16 is a header alone, the payload UMPs are as many as the
/// size takes, and the data is what the payload UMPs hold past their prefix.
#[test]
fn chunk_size_counts_the_header_and_whole_umps() {
    assert_eq!(MdsChunkSize::new(0), None);
    assert_eq!(MdsChunkSize::new(15), None);
    let header_only = MdsChunkSize::new(16).unwrap();
    assert_eq!((header_only.payload_umps(), header_only.data_len()), (0, 0));
    let one = MdsChunkSize::new(32).unwrap();
    assert_eq!((one.payload_umps(), one.data_len()), (1, 14));
    // One byte into a payload UMP's prefix carries nothing: not a size.
    assert_eq!(MdsChunkSize::new(33), None);
    assert_eq!(MdsChunkSize::new(16 + 16 * 5 + 1), None);
    assert_eq!(MdsChunkSize::new(u16::MAX).unwrap().payload_umps(), 4095);
    for len in 0..300usize {
        let size = MdsChunkSize::from_data_len(len).unwrap();
        assert_eq!(usize::from(size.data_len()), len);
        assert_eq!(usize::from(size.payload_umps()), len.div_ceil(14));
    }
    assert_eq!(MdsChunkSize::from_data_len(usize::from(u16::MAX)), None);
}

/// Section 7.9: zero chunks is unknown, zero for this chunk is the unverified
/// end of 7.9.1, and every other count is a count.
/// Spec: m2-104/7.9#5
#[test]
fn chunk_counts_and_numbers_keep_their_special_zero() {
    assert_eq!(MdsChunks::new(0), MdsChunks::Unknown);
    assert_eq!(MdsChunk::new(0), MdsChunk::Unverified);
    for raw in [0, 1, 2, 0xFFFF] {
        assert_eq!(MdsChunks::new(raw).get(), raw);
        assert_eq!(MdsChunk::new(raw).get(), raw);
    }
    assert_eq!(MdsChunkUmps::new(0), None);
    assert_eq!(MdsChunkUmps::new(4095), None);
    assert_eq!(MdsChunkUmps::new(4094).unwrap().get(), 4094);
}

/// Section 7.9, Figure 86 and the header fields: each header says its chunk
/// number counting from 1 and the set's chunk count (the same in every
/// chunk), the last chunk's number equals the count, its size matches the
/// payload UMPs after it, and an empty set is one header-only chunk (7.9.1).
/// Spec: m2-104/7.9#7
#[test]
fn headers_carry_numbers_counts_and_sizes() {
    let bytes = data(30);
    let s = set(G0, 6, &bytes);
    let umps: Vec<Ump> = s.packets(MdsChunkUmps::of(1)).unwrap().collect();
    // Three chunks of a header and a payload UMP, the last holding two bytes.
    assert_eq!(umps.len(), 6);
    let headers: Vec<MixedDataHeader> = umps
        .iter()
        .filter_map(|u| match u.decode() {
            Ok(Message::Data128(_, Data128::MixedDataHeader(h))) => Some(h),
            _ => None,
        })
        .collect();
    assert_eq!(
        headers.iter().map(|h| h.chunk.get()).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        headers
            .iter()
            .all(|h| h.chunks.get() == 3 && h.mds_id == U4::of(6))
    );
    assert!(headers.iter().all(|h| h.identity == ID));
    assert_eq!(
        headers
            .iter()
            .map(|h| h.chunk_size.get())
            .collect::<Vec<_>>(),
        [32, 32, 16 + 2 + 2]
    );

    let empty = set(G0, 0, &[]);
    let umps: Vec<Ump> = empty.packets(MdsChunkUmps::of(4)).unwrap().collect();
    assert_eq!(umps, [header(G0, 0, 16, 1, 1)]);
}

/// A written set reads back whole at the boundary sizes (none, one byte,
/// exactly one payload UMP, one more, a chunk's worth and one more), for
/// chunks of several sizes.
#[test]
fn a_written_set_reads_back_whole() {
    let mut a = assembler(1);
    for per_chunk in [1, 2, 8] {
        for len in [0, 1, 13, 14, 15, 27, 28, 29, 56, 57, 100] {
            let bytes = data(len);
            let s = set(MidiGroup::of(9), 15, &bytes);
            let packets = s.packets(MdsChunkUmps::of(per_chunk)).unwrap();
            let expected = packets.len();
            let umps: Vec<Ump> = packets.collect();
            assert_eq!(umps.len(), expected, "{per_chunk} {len}");
            assert_eq!(
                feed(&mut a, umps),
                [(9, 15, ID, bytes)],
                "{per_chunk} {len}"
            );
        }
    }
    assert_eq!(a.dropped(), 0);
}

/// Section 7.9, MDS ID: sets with different ids on a Group, and the same id on
/// different Groups, are separate and may interleave.
#[test]
fn mds_ids_and_groups_are_independent() {
    let mut a = assembler(4);
    let (d1, d2, d3) = (data(20), data(21), data(22));
    let g1 = MidiGroup::of(1);
    let streams = [
        set(G0, 1, &d1).packets(MdsChunkUmps::of(1)).unwrap(),
        set(G0, 2, &d2).packets(MdsChunkUmps::of(1)).unwrap(),
        set(g1, 1, &d3).packets(MdsChunkUmps::of(1)).unwrap(),
    ];
    // Round-robin one UMP from each.
    let mut streams: Vec<_> = streams.into_iter().collect();
    let mut umps = Vec::new();
    while streams.iter().any(|s| s.len() > 0) {
        for s in &mut streams {
            umps.extend(s.next());
        }
    }
    let mut done = feed(&mut a, umps);
    done.sort_by_key(|d| (d.0, d.1));
    assert_eq!(done, [(0, 1, ID, d1), (0, 2, ID, d2), (1, 1, ID, d3)]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.9.1, note: other UMPs, a SysEx8 packet and a voice message of the
/// same Group among them, end nothing.
#[test]
fn other_messages_inside_a_set_end_nothing() {
    let mut a = assembler(1);
    let bytes = data(20);
    let sx8 = Ump::from(Message::Data128(
        G0,
        Data128::Sysex8(Sysex8Packet::new(SysexStatus::Complete, 0, &[1]).unwrap()),
    ));
    let mut umps: Vec<Ump> = set(G0, 0, &bytes)
        .packets(MdsChunkUmps::of(1))
        .unwrap()
        .collect();
    umps.insert(1, sx8);
    umps.insert(3, Ump([0x2090_3C40, 0, 0, 0]));
    assert_eq!(feed(&mut a, umps), [(0, 0, ID, bytes)]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.9, Number of This Chunk: incrementing from 1. A header that is not
/// the next number drops the set, counted once, and its payload is skipped.
/// Spec: m2-104/7.9#7
#[test]
fn a_chunk_out_of_order_drops_the_set() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 3, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 32, 3, 3),
            payload(G0, 0, &[2; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    // A repeated number is out of order too.
    let done = feed(
        &mut a,
        [
            header(G0, 1, 32, 3, 1),
            payload(G0, 1, &[1; 14]),
            header(G0, 1, 32, 3, 2),
            payload(G0, 1, &[1; 14]),
            header(G0, 1, 32, 3, 2),
            payload(G0, 1, &[1; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 2);
}

/// A set begins with chunk 1: a first header with another number, and the
/// payload UMPs that follow, are one dropped set.
#[test]
fn a_set_that_does_not_begin_with_chunk_one_is_dropped_once() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 48, 2, 2),
            payload(G0, 0, &[1; 14]),
            payload(G0, 0, &[2; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    // A payload UMP with no header before it is one dropped set as well.
    feed(&mut a, [payload(G0, 3, &[1]), payload(G0, 3, &[2])]);
    assert_eq!(a.dropped(), 2);
}

/// Chunk 1 of a set whose id is open begins a new set: the open one is
/// dropped, counted.
#[test]
fn chunk_one_again_restarts_the_set() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 2, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 32, 1, 1),
            payload(G0, 0, &[2; 14]),
        ],
    );
    assert_eq!(done, [(0, 0, ID, vec![2; 14])]);
    assert_eq!(a.dropped(), 1);
}

/// Section 7.9, Number of Valid Bytes: the payload UMPs of a chunk are as many
/// as its size takes. A header before they are all in, or a payload UMP past
/// them, drops the set.
#[test]
fn a_chunk_with_too_few_or_too_many_payload_umps_drops_the_set() {
    let mut a = assembler(4);
    // Two payload UMPs promised, one sent.
    let done = feed(
        &mut a,
        [
            header(G0, 0, 48, 2, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 48, 2, 2),
            payload(G0, 0, &[1; 14]),
            payload(G0, 0, &[1; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    // One promised, two sent.
    let done = feed(
        &mut a,
        [
            header(G0, 1, 32, 2, 1),
            payload(G0, 1, &[1; 14]),
            payload(G0, 1, &[1; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 2);
    // A header-only chunk promises none.
    let done = feed(&mut a, [header(G0, 2, 16, 1, 1)]);
    assert_eq!(done, [(0, 2, ID, vec![])]);
}

/// Section 7.9, Number of Chunks (zero is unknown): chunks go on until one
/// declares a count equal to its own number, which ends the set.
/// Spec: m2-104/7.9#6
#[test]
fn an_unknown_count_ends_at_the_chunk_that_declares_it() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 0, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 32, 0, 2),
            payload(G0, 0, &[2; 14]),
            header(G0, 0, 32, 3, 3),
            payload(G0, 0, &[3; 14]),
        ],
    );
    let mut want = vec![1; 14];
    want.extend([2; 14]);
    want.extend([3; 14]);
    assert_eq!(done, [(0, 0, ID, want)]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.9.1: a sender out of data sends one more header-only chunk
/// (size 16) that declares the count equal to its number, which closes the
/// set with the data sent valid.
/// Spec: m2-104/7.9.1#2, m2-104/7.9.1#4
#[test]
fn a_header_only_chunk_that_declares_its_number_closes_the_set() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 0, 1),
            payload(G0, 0, &[5; 14]),
            header(G0, 0, 16, 2, 2),
        ],
    );
    assert_eq!(done, [(0, 0, ID, vec![5; 14])]);
    assert_eq!(a.dropped(), 0);
}

/// A known count is also the end: chunk number equal to it, and a number
/// above it is not a chunk of the set.
#[test]
fn a_chunk_numbered_past_the_count_drops_the_set() {
    let mut a = assembler(1);
    let done = feed(&mut a, [header(G0, 0, 32, 1, 1), payload(G0, 0, &[1; 14])]);
    assert_eq!(done.len(), 1);
    // Count 1, chunk 2 follows a chunk 1 that declared 2 and is not the last.
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 2, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 32, 1, 2),
            payload(G0, 0, &[1; 14]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
}

/// Section 7.9.1: a final chunk numbered zero ends the set with its data not
/// vouched for: the set is dropped, counted. One that closes nothing counts
/// nothing, and its payload is skipped.
/// Spec: m2-104/7.9.1#3
#[test]
fn an_unverified_end_drops_the_set() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 0, 1),
            payload(G0, 0, &[1; 14]),
            header(G0, 0, 16, 0, 0),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    // The id and the lane are free again: another id's set is taken.
    let done = feed(&mut a, [header(G0, 7, 16, 1, 1)]);
    assert_eq!(done.len(), 1);
    // With nothing open: no drop, and the payload of the chunk is skipped.
    feed(&mut a, [header(G0, 5, 32, 0, 0), payload(G0, 5, &[1; 14])]);
    assert_eq!(a.dropped(), 1);
    let done = feed(&mut a, [header(G0, 5, 16, 1, 1)]);
    assert_eq!(done.len(), 1);
}

/// A set past the buffer is dropped whole, never cut, counted once; one that
/// fills it exactly is delivered.
#[test]
fn an_over_long_set_is_dropped_whole_and_counted_once() {
    let mut a = MixedDataAssembler::new(vec![0; 40], 40, 1);
    let bytes = data(41);
    let s = set(G0, 0, &bytes);
    assert!(feed(&mut a, s.packets(MdsChunkUmps::of(1)).unwrap()).is_empty());
    assert_eq!(a.dropped(), 1);
    let fits = data(40);
    let s = set(G0, 0, &fits);
    assert_eq!(
        feed(&mut a, s.packets(MdsChunkUmps::of(2)).unwrap()),
        [(0, 0, ID, fits)]
    );
    assert_eq!(a.dropped(), 1);
}

/// With one lane, another id's set is dropped, counted; the open one is intact.
#[test]
fn a_set_with_no_free_lane_is_dropped_and_counted() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            header(G0, 0, 32, 1, 1),
            header(G0, 1, 16, 1, 1),
            payload(G0, 0, &[4; 14]),
        ],
    );
    assert_eq!(done, [(0, 0, ID, vec![4; 14])]);
    assert_eq!(a.dropped(), 1);
}

/// A set open at `lost_tail` or `reset` is dropped, counted.
#[test]
fn lost_tail_and_reset_drop_open_sets() {
    let mut a = assembler(2);
    feed(&mut a, [header(G0, 0, 32, 2, 1), header(G0, 1, 32, 2, 1)]);
    a.lost_tail();
    assert_eq!(a.dropped(), 2);
    feed(&mut a, [header(G0, 0, 32, 2, 1)]);
    a.reset();
    assert_eq!(a.dropped(), 3);
}

/// UMPs that are not Mixed Data Set UMPs are for the caller.
#[test]
fn only_mixed_data_umps_are_taken() {
    let mut a = assembler(1);
    assert_eq!(a.push(Ump([0; 4])), MixedDataStep::NotMixedData);
    assert_eq!(
        a.push(Ump([0x5003_0100, 0, 0, 0])),
        MixedDataStep::NotMixedData
    );
    assert_eq!(a.push(header(G0, 0, 32, 1, 1)), MixedDataStep::Held);
}
