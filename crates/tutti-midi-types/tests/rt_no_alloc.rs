//! Regression gate: the readers on a plugin's or a transport's bytes must not allocate.

use assert_no_alloc::AllocDisabler;
use tutti_midi_types::from_midi1_bytes;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `from_midi1_bytes` runs on a plugin's output and a packet transport, where
/// the bytes are not ours: a stray status, a truncated message, an undefined
/// status and SysEx allocate nothing and read as `None`, and a good message
/// still reads.
///
/// Mutation (run): the old `midly` path restored in `from_midi1_bytes` ->
/// the process aborts on `[0xF7, 1, 2]` under `assert_no_alloc`.
#[test]
fn from_midi1_bytes_on_hostile_bytes_is_allocation_free() {
    let hostile: [&[u8]; 8] = [
        &[0xF7, 1, 2],
        &[0x90, 60],
        &[0x90],
        &[0xF0, 1, 2],
        &[0xF4, 1, 2],
        &[0xFD],
        &[0x90, 0x80, 1],
        &[],
    ];
    assert_no_alloc::assert_no_alloc(|| {
        for bytes in hostile {
            assert!(from_midi1_bytes(bytes).is_none(), "{bytes:02X?}");
        }
        assert!(from_midi1_bytes(&[0x90, 60, 100]).is_some());
    });
}

/// Writing SysEx7 packets (`tutti_ump::sysex7_packets`) allocates nothing.
///
/// Mutation (run): `Sysex7Packets::next` built through `Sysex7::<Vec<u32>>` again
/// -> the process aborts.
#[test]
fn sysex7_packets_are_allocation_free() {
    use tutti_ump::{MidiGroup, U7, sysex7_packets};

    let long = [U7::of(7); 20];
    assert_no_alloc::assert_no_alloc(|| {
        for payload in [&[][..], &[U7::of(1), U7::of(2), U7::of(3)][..], &long[..]] {
            assert!(sysex7_packets(MidiGroup::FIRST, payload).count() >= 1);
        }
    });
}
