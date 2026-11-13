//! The Property Data codecs (M2-103 §6.1.6, Table 12). Each test names the
//! property it holds.

use super::*;

fn limit(bytes: usize) -> BodyLimit {
    BodyLimit::new(bytes).unwrap()
}

fn big() -> BodyLimit {
    limit(1 << 24)
}

fn sample(len: usize) -> Vec<u8> {
    // Bytes of every value, in an order that does not compress to nothing.
    (0..len)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect()
}

// Table 12: ASCII is the data as it is, 7-bit.
#[test]
fn ascii_is_the_body_unchanged_and_seven_bit() {
    let text = br#"{"a":1}"#;
    assert_eq!(encode_body(PeEncoding::Ascii, text).unwrap(), text);
    assert_eq!(decode_body(PeEncoding::Ascii, text, big()).unwrap(), text);
    assert_eq!(
        encode_body(PeEncoding::Ascii, &[b'a', 0x80]),
        Err(PeBodyError::NotAscii { at: 1 })
    );
    assert_eq!(
        decode_body(PeEncoding::Ascii, &[0xFF], big()),
        Err(PeBodyError::NotAscii { at: 0 })
    );
}

// Table 12: Mcoded7 carries 8-bit data in 7-bit bytes.
#[test]
fn mcoded7_round_trips_every_byte_value() {
    let data: Vec<u8> = (0..=255).collect();
    let wire = encode_body(PeEncoding::Mcoded7, &data).unwrap();
    assert!(wire.iter().all(|b| b & 0x80 == 0));
    assert_eq!(
        decode_body(PeEncoding::Mcoded7, &wire, big()).unwrap(),
        data
    );
}

// Table 12: zlib+Mcoded7 is zlib compression, then Mcoded7.
#[test]
fn zlib_mcoded7_round_trips() {
    for len in [0, 1, 7, 100, 5000] {
        let data = sample(len);
        let wire = encode_body(PeEncoding::ZlibMcoded7, &data).unwrap();
        assert!(wire.iter().all(|b| b & 0x80 == 0));
        assert_eq!(
            decode_body(PeEncoding::ZlibMcoded7, &wire, big()).unwrap(),
            data
        );
    }
}

/// The 14 bytes of a zlib stream of `abc`, made by hand (RFC 1950, RFC 1951):
///
/// - `78 01`: the header. CMF `78` is deflate with a 32 KiB window, FLG `01` is
///   no preset dictionary and the lowest level; (0x78 * 256 + 0x01) is 30721, a
///   multiple of 31, as the header check requires.
/// - `01`: one deflate block, BFINAL 1 and BTYPE 00, a stored block, padded to
///   the byte.
/// - `03 00 FC FF`: LEN 3 and its complement NLEN, least significant byte first.
/// - `61 62 63`: the data.
/// - `02 4D 01 27`: the Adler-32, most significant byte first. Over `abc`, A is
///   1 + 97 + 98 + 99 = 295 = 0x0127, and B is the sum of A after each byte:
///   98 + 196 + 295 = 589 = 0x024D; the checksum is B * 65536 + A.
const STORED_ABC: [u8; 14] = [
    0x78, 0x01, 0x01, 0x03, 0x00, 0xFC, 0xFF, 0x61, 0x62, 0x63, 0x02, 0x4D, 0x01, 0x27,
];

// RFC 1950 §2.2: a stream a hand wrote decodes to the body it holds.
#[test]
fn a_known_zlib_stream_decodes_to_its_body() {
    let wire = mcoded7::encode_vec(&STORED_ABC);
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, big()).unwrap(),
        b"abc"
    );
}

// RFC 1950 §2.2: the Adler-32 at the end is checked, whichever bit is wrong.
#[test]
fn a_wrong_adler_checksum_is_refused() {
    for at in 10..14 {
        let mut stream = STORED_ABC;
        stream[at] ^= 1;
        let wire = mcoded7::encode_vec(&stream);
        assert_eq!(
            decode_body(PeEncoding::ZlibMcoded7, &wire, big()),
            Err(PeBodyError::BadChecksum),
            "byte {at}"
        );
    }
}

// RFC 1950 §2.2: a stream that stops before its checksum is truncated.
#[test]
fn a_truncated_zlib_stream_is_refused() {
    let wire = mcoded7::encode_vec(&STORED_ABC[..11]);
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, big()),
        Err(PeBodyError::Truncated)
    );
}

// RFC 1950 §2.2: a header whose check is not a multiple of 31 is not zlib.
#[test]
fn a_bad_zlib_header_is_refused() {
    let mut stream = STORED_ABC;
    stream[1] = 0x02;
    let wire = mcoded7::encode_vec(&stream);
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, big()),
        Err(PeBodyError::BadStream)
    );
}

// The Mcoded7 layer comes first: a byte with its top bit set is refused there.
#[test]
fn a_zlib_body_that_is_not_seven_bit_is_refused() {
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &[0x7F, 0x80], big()),
        Err(PeBodyError::Mcoded7(Mcoded7Error::NotSevenBit { at: 1 }))
    );
}

// A few bytes of zlib inflate to a great many: the inflation stops at the limit,
// and a body of exactly the limit is fine.
#[test]
fn a_decompression_bomb_is_refused_at_the_limit() {
    let zeros = vec![0u8; 1 << 20];
    let wire = encode_body(PeEncoding::ZlibMcoded7, &zeros).unwrap();
    assert!(wire.len() < 2000, "the bomb is small: {}", wire.len());
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, limit(1000)),
        Err(PeBodyError::TooLarge(limit(1000)))
    );
    let exact = limit(1 << 20);
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, exact)
            .unwrap()
            .len(),
        1 << 20
    );
    let under = limit((1 << 20) - 1);
    assert_eq!(
        decode_body(PeEncoding::ZlibMcoded7, &wire, under),
        Err(PeBodyError::TooLarge(under))
    );
}

// The limit holds for every encoding, at its boundary.
#[test]
fn the_limit_holds_for_the_plain_encodings() {
    let data = sample(100);
    for encoding in [PeEncoding::Mcoded7, PeEncoding::Ascii] {
        let data: Vec<u8> = data.iter().map(|b| b & 0x7F).collect();
        let wire = encode_body(encoding, &data).unwrap();
        assert!(decode_body(encoding, &wire, limit(100)).is_ok());
        assert_eq!(
            decode_body(encoding, &wire, limit(99)),
            Err(PeBodyError::TooLarge(limit(99)))
        );
    }
}

#[test]
fn a_limit_is_at_least_one_byte() {
    assert_eq!(BodyLimit::new(0), None);
}

#[cfg(feature = "serde")]
#[test]
fn the_body_limit_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: BodyLimit::new,
        get: BodyLimit::get,
        of: None,
        valid: &[1, usize::MAX],
        invalid: &[0],
    });
}
