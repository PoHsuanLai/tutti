use super::*;

// What `pack_id` makes unpacks to what went in, at the ends of both bytes.
// Mutation: `>> 32` to `>> 31` in `unpack_id`: client 1 reads as 2.
#[test]
fn a_packed_id_unpacks_to_its_address() {
    for (client, port) in [(0, 0), (14, 0), (128, 3), (255, 255)] {
        assert_eq!(unpack_id(pack_id(client, port)), Some((client, port)));
    }
}

// An id no `pack_id` made is refused, not truncated: a client above a byte, a
// port above a byte, a negative address that wrapped on packing.
// Mutation: drop the `<= byte` checks: `from_raw(1 << 32 | 256)` reads as
// (1, 256), and the cast of a wider client names a real one.
#[test]
fn an_id_the_backend_did_not_mint_is_refused() {
    for raw in [
        (256u64 << 32) | 1,
        (1u64 << 32) | 256,
        1u64 << 40,
        u64::MAX,
        // The low half of a CoreMIDI-style 32-bit unique id.
        0xDEAD_BEEF,
    ] {
        assert_eq!(unpack_id(EndpointId::from_raw(raw)), None, "{raw:#x}");
    }
    assert_eq!(unpack_id(pack_id(-1, 0)), None);
}
