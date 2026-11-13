//! Chunking (M2-101 §8.3, M2-103 §5.2, §5.3). Each test names the property of
//! the spec it holds.

use super::*;
use crate::ci::pe::{PeRequest, ResourceName};

/// The smallest Receivable Maximum SysEx Message Size.
const SMALLEST: MaxSysexSize = MaxSysexSize::of(128);

fn header() -> PeHeader {
    PeHeader::Request(PeRequest::new(ResourceName::new("State").unwrap()))
}

/// The bytes of [`header`]: `{"resource":"State"}`.
const HEADER_LEN: usize = 20;

fn id() -> PeRequestId {
    PeRequestId::of(5)
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 128) as u8).collect()
}

fn joined(chunks: &[PeChunk]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.data().to_vec()).collect()
}

/// What the first chunk of a 128-byte message holds beside its data: the 24
/// bytes around the fields, and the header.
const FIRST_ROOM: usize = 128 - 24 - HEADER_LEN;
/// What each later chunk holds.
const NEXT_ROOM: usize = 128 - 24;

// M2-103 §5.3: a Request ID is 0 to 127.
#[test]
fn a_request_id_is_0_to_127() {
    assert_eq!(PeRequestId::new(0).map(PeRequestId::get), Some(0));
    assert_eq!(PeRequestId::new(127).map(PeRequestId::get), Some(127));
    assert_eq!(PeRequestId::new(128), None);
    assert!(PeRequestId::try_from(128).is_err());
    assert_eq!(u8::from(PeRequestId::of(9)), 9);
}

// M2-101 §8.3, Table 33: a chunk count and number are 14-bit, and count from 1.
#[test]
fn chunk_numbers_count_from_1_in_14_bits() {
    assert_eq!(ChunkNo::new(0), None);
    assert_eq!(ChunkNo::new(1), Some(ChunkNo::FIRST));
    assert_eq!(ChunkNo::new(0x3FFF).map(ChunkNo::get), Some(16383));
    assert_eq!(ChunkNo::new(0x4000), None);
    assert_eq!(ChunkNo::of(0x3FFF).next(), None);
}

// M2-101 §8.3, Table 29: 0 is the unknown count, and a final chunk numbered 0
// abandons the message.
#[test]
fn zero_is_the_unknown_count_and_the_abandoning_number() {
    assert_eq!(ChunkTotal::from_wire(0), Some(ChunkTotal::Unknown));
    assert_eq!(ChunkTotal::Unknown.wire(), 0);
    assert_eq!(ChunkPosition::from_wire(0), Some(ChunkPosition::Abandoned));
    assert_eq!(
        ChunkTotal::from_wire(6),
        Some(ChunkTotal::Known(ChunkNo::of(6)))
    );
    assert_eq!(ChunkTotal::from_wire(0x4000), None);
}

// M2-101 §8.3: a message whose SysEx fits the peer's maximum is not broken.
#[test]
fn a_body_that_fits_exactly_is_one_chunk() {
    let chunks = split_message(id(), &header(), &body(FIRST_ROOM), SMALLEST).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].sysex_len(), 128);
    assert_eq!(chunks[0].total(), ChunkTotal::Known(ChunkNo::of(1)));
    assert_eq!(chunks[0].position(), ChunkPosition::Number(ChunkNo::FIRST));
}

// M2-101 §8.3: a message past the maximum is broken into chunks, each labelled
// with the count and its number from 1.
#[test]
fn a_body_one_byte_over_is_two_chunks() {
    let body = body(FIRST_ROOM + 1);
    let chunks = split_message(id(), &header(), &body, SMALLEST).unwrap();
    assert_eq!(chunks.len(), 2);
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.total(), ChunkTotal::Known(ChunkNo::of(2)));
        assert_eq!(
            chunk.position(),
            ChunkPosition::Number(ChunkNo::of(i as u16 + 1))
        );
        assert_eq!(chunk.request_id(), id());
    }
    assert_eq!(chunks[0].sysex_len(), 128);
    assert_eq!(chunks[1].data().len(), 1);
    assert_eq!(joined(&chunks), body);
}

// M2-101 §8.3.1: Header Data is in the first chunk only; the rest have none.
#[test]
fn only_the_first_chunk_carries_the_header() {
    let chunks = split_message(id(), &header(), &body(1000), SMALLEST).unwrap();
    assert_eq!(chunks[0].header(), br#"{"resource":"State"}"#);
    assert!(chunks[1..].iter().all(|c| c.header().is_empty()));
}

// M2-101 §8.3: many chunks, each a SysEx message within the maximum, the count
// the same in all and the numbers sequential, the body intact.
#[test]
fn many_chunks_each_fit_and_are_numbered_in_order() {
    let body = body(1000);
    let chunks = split_message(id(), &header(), &body, SMALLEST).unwrap();
    // 84 bytes in the first chunk, then 104 in each of 9 more.
    assert_eq!(chunks.len(), 1 + (1000 - FIRST_ROOM).div_ceil(NEXT_ROOM));
    assert_eq!(chunks.len(), 10);
    for (i, chunk) in chunks.iter().enumerate() {
        assert!(chunk.sysex_len() <= 128);
        assert_eq!(chunk.total(), ChunkTotal::Known(ChunkNo::of(10)));
        assert_eq!(
            chunk.position(),
            ChunkPosition::Number(ChunkNo::of(i as u16 + 1))
        );
    }
    assert_eq!(joined(&chunks), body);
}

// M2-101 §8.3.2: a message with no Property Data is one chunk, 1 of 1.
#[test]
fn a_message_without_data_is_not_chunked() {
    let chunks = split_message(id(), &header(), &[], SMALLEST).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].total(), ChunkTotal::Known(ChunkNo::FIRST));
    assert_eq!(chunks[0].position(), ChunkPosition::Number(ChunkNo::FIRST));
    assert!(chunks[0].data().is_empty());
}

/// A request whose header is `len` bytes: the resource and a media type.
fn header_of(len: usize) -> PeHeader {
    let mut request = PeRequest::new(ResourceName::new("State").unwrap());
    // `{"resource":"State","mediaType":""}` is 35 bytes around the media type.
    request.media_type = crate::MediaType::new(&"a".repeat(len - 35));
    PeHeader::Request(request)
}

// M2-101 §5.5.3: 128 is the smallest maximum, and a header that fills the first
// chunk leaves it no data; the body goes in the later chunks, whole.
#[test]
fn the_smallest_maximum_still_carries_a_body() {
    let chunks = split_message(id(), &header_of(104), &body(300), SMALLEST).unwrap();
    assert_eq!(chunks.len(), 1 + 300usize.div_ceil(NEXT_ROOM));
    assert!(chunks[0].data().is_empty());
    assert_eq!(chunks[0].sysex_len(), 128);
    assert!(chunks.iter().all(|c| c.sysex_len() <= 128));
    assert_eq!(joined(&chunks), body(300));
}

// M2-101 Tables 33 to 35: a SysEx message is the preamble, the Request ID, the
// header length, the header, three 2-byte fields, the data and `F7`.
#[test]
fn the_fields_take_the_bytes_the_tables_lay_out() {
    let chunks = split_message(id(), &header(), &body(300), SMALLEST).unwrap();
    for chunk in &chunks {
        let fields = chunk.encode_fields();
        // F0, 13 bytes of preamble up to the destination MUID, then F7.
        assert_eq!(fields.len() + 15, chunk.sysex_len());
        assert_eq!(PeChunk::parse_fields(&fields).as_ref(), Ok(chunk));
    }
    let first = chunks[0].encode_fields();
    assert_eq!(first[0], 5);
    // Header length 20, least significant byte first.
    assert_eq!(first[1..3], [20, 0]);
    // Number of Chunks in Message 4, Number of This Chunk 1.
    assert_eq!(first[3 + 20..3 + 20 + 4], [4, 0, 1, 0]);
}

// M2-101 §8.3: a chunk number is 14-bit, so a message is at most 16383 chunks.
#[test]
fn a_message_may_take_16383_chunks_and_no_more() {
    let most = FIRST_ROOM + 16382 * NEXT_ROOM;
    let chunks = split_message(id(), &header(), &body(most), SMALLEST).unwrap();
    assert_eq!(chunks.len(), 16383);
    assert_eq!(
        chunks.last().unwrap().position(),
        ChunkPosition::Number(ChunkNo::of(16383))
    );
    assert_eq!(
        split_message(id(), &header(), &body(most + 1), SMALLEST),
        Err(PeChunkError::TooManyChunks { needed: 16384 })
    );
}

// M2-101 Table 33: the Property Data length of a chunk is 14-bit, whatever the
// peer's maximum.
#[test]
fn a_chunk_holds_at_most_16383_data_bytes() {
    let huge = MaxSysexSize::of(MaxSysexSize::MAX);
    let chunks = split_message(id(), &header(), &body(16383), huge).unwrap();
    assert_eq!(chunks.len(), 1);
    let chunks = split_message(id(), &header(), &body(16384), huge).unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1].data().len(), 1);
    let wire = PeChunk::new(
        id(),
        ChunkTotal::Unknown,
        ChunkPosition::Abandoned,
        vec![],
        vec![0; 16384],
    );
    assert_eq!(wire, Err(PeChunkError::DataTooLong { len: 16384 }));
}

// M2-101 §8.3.2: a header is not chunked, so it has to fit the maximum on its own.
#[test]
fn a_header_past_the_maximum_is_refused() {
    assert!(split_message(id(), &header_of(104), &[], SMALLEST).is_ok());
    assert_eq!(
        split_message(id(), &header_of(105), &[], SMALLEST),
        Err(PeChunkError::Header(PeHeaderTooLong {
            len: 105,
            limit: 104
        }))
    );
}

// M2-101 §8.3.1: chunks 2 and higher have a header length of 0.
#[test]
fn a_later_chunk_with_a_header_cannot_be_made_or_read() {
    let made = PeChunk::new(
        id(),
        ChunkTotal::Unknown,
        ChunkPosition::Number(ChunkNo::of(2)),
        b"{}".to_vec(),
        vec![],
    );
    assert_eq!(made, Err(PeChunkError::HeaderNotInFirst));
    // Request ID 5, header length 2 then `{}`, counts 0 and 2, data length 0.
    let wire = [5, 2, 0, b'{', b'}', 0, 0, 2, 0, 0, 0];
    assert_eq!(
        PeChunk::parse_fields(&wire),
        Err(PeChunkError::HeaderNotInFirst)
    );
}

// M2-101 Table 33: lengths count the bytes that follow, and the message ends
// after the data.
#[test]
fn a_chunk_whose_lengths_do_not_fill_its_bytes_is_refused() {
    let good = PeChunk::new(
        id(),
        ChunkTotal::Known(ChunkNo::FIRST),
        ChunkPosition::Number(ChunkNo::FIRST),
        b"{}".to_vec(),
        b"ab".to_vec(),
    )
    .unwrap()
    .encode_fields();
    assert!(PeChunk::parse_fields(&good).is_ok());
    let mut long = good.clone();
    long.push(0);
    assert_eq!(PeChunk::parse_fields(&long), Err(PeChunkError::Length));
    assert_eq!(
        PeChunk::parse_fields(&good[..good.len() - 1]),
        Err(PeChunkError::Length)
    );
    let mut high = good;
    high[3] = 0x80;
    assert!(matches!(
        PeChunk::parse_fields(&high),
        Err(PeChunkError::NotSevenBit { .. })
    ));
}

// M2-103 §6.1.6: the data of a SysEx message is 7-bit.
#[test]
fn a_body_with_a_top_bit_is_refused() {
    assert_eq!(
        split_message(id(), &header(), &[1, 2, 0x80], SMALLEST),
        Err(PeChunkError::NotSevenBit { at: 2 })
    );
}

#[cfg(feature = "serde")]
#[test]
fn the_numbers_are_range_checked_values() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: PeRequestId::new,
        get: PeRequestId::get,
        of: Some(PeRequestId::of),
        valid: &[0, 1, 127],
        invalid: &[128, 255],
    });
    check_range_contract(&RangeContract {
        new: ChunkNo::new,
        get: ChunkNo::get,
        of: Some(ChunkNo::of),
        valid: &[1, 2, 0x3FFF],
        invalid: &[0, 0x4000, u16::MAX],
    });
}
