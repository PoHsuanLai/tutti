//! `decode` and `From<Message> for Ump` allocate nothing, on every message
//! family and on a packet that is `Unknown`.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_ump::{Message, Ump};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// One packet of every family, the defined messages and some refused ones.
fn corpus() -> Vec<Ump> {
    let words: [[u32; 4]; 24] = [
        [0x0010_1234, 0, 0, 0],
        [0x15F2_4546, 0, 0, 0],
        [0x2395_3C7F, 0, 0, 0],
        [0x23E5_4546, 0, 0, 0],
        [0x3203_7E7F, 0x0900_0000, 0, 0],
        [0x4192_3C03, 0xABCD_1234, 0, 0],
        [0x41C2_0001, 0x0900_3424, 0, 0],
        [0x41F2_3C03, 0, 0, 0],
        [0x5403_07AA, 0xBB00_0000, 0, 0],
        [0x543F_0700, 0, 0, 0],
        [0x5485_0020, 0x0002_0001, 0x8213_FFFF, 0x0102_0304],
        [0xD610_0000, 0x02FA_F080, 0, 0],
        [0xD610_0006, 0xF509_0000, 0x3547_0000, 0xF201_2300],
        [0xD610_0101, 0x4869_0000, 0, 0],
        [0xF000_0101, 0x0000_001F, 0, 0],
        [0xF001_0101, 0x8300_0303, 0, 0],
        [0xF002_0000, 0x0000_206B, 0x0102_0304, 0x0506_0708],
        [0xF003_5475, 0x7474_6900, 0, 0],
        [0xF011_833B, 0x0402_0110, 0, 0],
        [0xF012_0241, 0x4200_0000, 0, 0],
        [0x6123_4567, 1, 2, 3],
        [0x0030_0000, 0, 0, 0],
        [0x5400_0700, 0, 0, 0],
        [0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF],
    ];
    words.map(Ump).to_vec()
}

#[test]
fn decoding_allocates_nothing() {
    let packets = corpus();
    let mut decoded = 0;
    assert_no_alloc(|| {
        for &p in &packets {
            if p.decode().is_ok() {
                decoded += 1;
            }
        }
    });
    assert_eq!(
        decoded, 21,
        "the corpus has 21 defined messages and 3 refused packets"
    );
}

#[test]
fn encoding_allocates_nothing() {
    let messages: Vec<Message> = corpus()
        .into_iter()
        .filter_map(|p| p.decode().ok())
        .collect();
    assert_eq!(messages.len(), 21);
    let mut words = 0u32;
    assert_no_alloc(|| {
        for &m in &messages {
            words ^= Ump::from(m).0[0];
        }
    });
    assert_ne!(words, 0);
}
