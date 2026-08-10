//! What the SysEx assembler allocates on the driver callback thread — and what
//! it still does, which is not zero.
//!
//! [`Sysex7ByteAssembler`] runs on CoreMIDI's delivery thread or ALSA's seq pump.
//! Not the audio thread, but still a context that should not take a `malloc`
//! lock per message.
//!
//! # The honest state of this, measured not assumed
//!
//! **The assembler's own** per-message allocations are gone: the payload buffer
//! and the caller's fragment scratch are both reused across calls, so a run in
//! flight costs nothing once warm.
//!
//! It does **not** reach zero, because
//! [`MidiEvent::sysex7_fragments`](tutti_midi_types::ump::MidiEvent::sysex7_fragments)
//! itself allocates on every call: it builds a `Sysex7::<Vec<u32>>::new()`
//! internally. That is upstream of this crate and cannot be fixed from here — it
//! needs an `Sysex7::<[u32; N]>`-shaped API, or a reusable builder threaded
//! through the call, which is a `tutti-midi-types` change.
//!
//! So the gate below asserts the **reachable** property: reassembling a run
//! across many buffers — the path that dominates a long dump — allocates nothing
//! *until* a run completes. Asserting whole-loop zero would fail, and asserting
//! it with the fragment call hoisted outside the gate would be the vacuous
//! version of this test.
//!
//! # Why this is an integration test
//!
//! `assert_no_alloc` observes nothing unless
//! `#[global_allocator] = AllocDisabler` is installed, and that must be declared
//! at the root of a test *binary*. The same gate written inside `src/` is
//! silently inert — it passes whether or not the code allocates. Mutation-testing
//! caught exactly that, which is why this file exists.

use assert_no_alloc::AllocDisabler;
use tutti_midi_hardware::Sysex7ByteAssembler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Buffering a run in flight allocates nothing once warm.
///
/// This is the assembler's own contract, isolated from the upstream fragment
/// call: every `push` here appends to the in-flight payload and returns 0,
/// because no `0xF7` arrives inside the gate. A long dump spends nearly all of
/// its callbacks on exactly this path, so it is the one that matters for a
/// device streaming a bulk transfer.
///
/// Mutation check: re-introduce a `to_vec()` in `extend`, or drop the buffer
/// reuse, and this fails.
#[test]
fn buffering_a_run_in_flight_is_allocation_free() {
    let mut asm = Sysex7ByteAssembler::new();
    let mut out = Vec::new();

    // Warm the internal buffer to its steady size, then complete the run so the
    // assembler is idle. Deliberately light — warming with the full load is the
    // trap `rt_no_alloc_hardware_poll.rs` documents.
    asm.push(&[0xF0], &mut out);
    for _ in 0..64 {
        asm.push(&[0x7F; 16], &mut out);
    }
    asm.push(&[0xF7], &mut out);
    out.clear();

    assert_no_alloc::assert_no_alloc(|| {
        asm.push(&[0xF0], &mut out);
        for _ in 0..64 {
            asm.push(&[0x7F; 16], &mut out);
        }
    });

    // The gate proves nothing if the loop did no work: 64 * 16 payload bytes
    // must actually be held in flight.
    assert!(
        asm.in_flight(),
        "the gated loop must have left a run buffered"
    );
    assert!(
        out.is_empty(),
        "no run completed inside the gate, so nothing was emitted"
    );
}

/// An overflowing run — the lost-`0xF7` fault path — allocates nothing either.
///
/// Worth its own gate because it is the path a *misbehaving* device drives, and
/// the one where an uncapped buffer used to grow without bound. Discarding must
/// not itself allocate, or a fault becomes a second fault.
#[test]
fn an_overflowing_run_is_allocation_free() {
    let mut asm = Sysex7ByteAssembler::new();
    let mut out = Vec::new();

    // Drive it past the cap once so every buffer is at its steady size, then
    // resync.
    asm.push(&[0xF0], &mut out);
    for _ in 0..(64 * 1024 / 256 + 8) {
        asm.push(&[0x7F; 256], &mut out);
    }
    asm.push(&[0xF7], &mut out);
    out.clear();

    assert_no_alloc::assert_no_alloc(|| {
        asm.push(&[0xF0], &mut out);
        for _ in 0..(64 * 1024 / 256 + 8) {
            asm.push(&[0x7F; 256], &mut out);
        }
    });

    assert!(
        asm.in_flight(),
        "the gated loop must have left the overflowed run in flight"
    );
    assert!(out.is_empty(), "an overflowed run emits nothing");
}
