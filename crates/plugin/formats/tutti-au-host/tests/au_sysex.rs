//! Can a SysEx message reach an AU instrument at all?
//!
//! `send_midi` decoded each event to a 3-byte MIDI 1.0 message and fell through
//! to `_ => continue` for everything else, so SysEx was dropped — no patch dump,
//! no vendor-specific message, ever reached a hosted instrument. The doc comment
//! described that as intended, on the grounds that "AUv2's
//! `MusicDeviceMIDIEvent` only speaks legacy channel voice".
//!
//! That premise is true and the conclusion does not follow: AU provides a
//! *second* entry point for exactly this, `MusicDeviceSysEx`
//! (`MusicDevice.h:237-241`, `CA_REALTIME_API`, not deprecated), taking a byte
//! buffer rather than a packed word. Two calls exist because one message family
//! cannot be expressed through the other.
//!
//! # What these can and cannot observe
//!
//! An AU has no "did you receive that" channel: `MusicDeviceSysEx` returns an
//! `OSStatus` and nothing else, and a synth's response to a patch dump is
//! internal. So these assert that a real instrument **accepts** the call and
//! that the surrounding stream is unaffected.
//!
//! **Mutation-checked, and the result is worth stating:** deleting the
//! `send_sysex` call entirely leaves all four tests green. They cannot witness
//! delivery, only that attempting it does not crash and does not disturb the
//! notes around it — which is exactly what a real instrument lets a host
//! observe. Anything stronger needs a fake AU that records what it was handed,
//! which the probe component in `support/probe_au.rs` could grow but does not
//! implement `MusicDeviceSysEx` today.
//!
//! What *is* pinned by assertion is the packet arithmetic
//! ([`the_test_packet_builder_matches_the_ump_layout`]) — the encoding a
//! fragmenting bug would corrupt.
//!
//! # DLSMusicDevice is excluded, and it is not our bug
//!
//! Measured on macOS 15.6 while writing these: **`DLSMusicDevice` traps
//! (SIGTRAP) inside `MusicDeviceSysEx` and never returns**, taking the whole
//! process with it. `AUSampler` on the same machine accepts the identical call
//! and returns `noErr`.
//!
//! Verified as the unit's behaviour, not this host's, and not the
//! instantiation race that the `AU_LOCK` below exists to prevent: the crash
//! reproduces with DLS as the *only* instrument in the list, with the lock
//! held, on the very first call, with both a bare payload and one framed
//! `0xF0 ... 0xF7`. The trap is inside the framework — no Rust frame appears
//! and no status is returned. It is a documented, non-deprecated
//! `CA_REALTIME_API` entry point on a unit that advertises MIDI input.
//!
//! So it is excluded here rather than worked around in `send_midi`. A host
//! cannot pre-screen for this: nothing in the AU's properties says "my SysEx
//! entry point aborts", and suppressing SysEx for every instrument to dodge one
//! broken unit would deny it to the ones that work. A caller who must survive
//! arbitrary AUs needs process isolation, which the plugin-server path already
//! provides — and that is the honest answer, not a special case keyed on a
//! four-char code.

#![cfg(target_os = "macos")]

mod support;

use std::sync::{Mutex, MutexGuard};

use support::corpus::{AuRef, SAMPLER};
use tutti_midi_types::ump::{
    SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE, SYSEX7_STATUS_START,
};
use tutti_midi_types::MidiEvent;

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Serializes AU instantiation across this file's tests.
///
/// Every other AU suite has one. Without it these crash with SIGTRAP inside
/// CoreAudio — not from anything SysEx-specific, but because two tests
/// instantiating corpus units concurrently is what the lock exists to prevent.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison-tolerant: the guard only serializes, so one panicking test must not
/// convert into N spurious failures.
fn lock() -> MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Instruments that survive `MusicDeviceSysEx` on this machine.
///
/// `DLSMusicDevice` is deliberately absent — see the module header. It is the
/// only corpus instrument excluded, so this is one unit rather than a category.
const SYSEX_SAFE_INSTRUMENTS: &[AuRef] = &[SAMPLER];

/// Build one UMP SysEx7 packet: message type 0x3, `status` nibble, `n` bytes.
///
/// Hand-rolled rather than taken from a helper, so the test states the wire
/// layout it depends on instead of agreeing with whatever the encoder does.
fn sysex7_packet(status: u8, bytes: &[u8]) -> MidiEvent {
    assert!(bytes.len() <= 6, "a SysEx7 packet carries at most 6 bytes");
    let n = bytes.len() as u32;
    let b = |i: usize| -> u32 { bytes.get(i).copied().unwrap_or(0) as u32 };

    let w0 = (0x3 << 28) | ((status as u32) << 20) | (n << 16) | (b(0) << 8) | b(1);
    let w1 = (b(2) << 24) | (b(3) << 16) | (b(4) << 8) | b(5);
    MidiEvent::from_ump(0, &[w0, w1])
}

/// A universal non-realtime identity request, the shortest realistic SysEx.
///
/// Body only: UMP carries SysEx7 payload *without* the 0xF0/0xF7 framing, which
/// the transport supplies.
const IDENTITY_REQUEST: [u8; 6] = [0xF0, 0x7E, 0x00, 0x06, 0x01, 0xF7];

/// A real instrument accepts a single-packet SysEx without failing the block.
///
/// The weakest useful claim, and the strongest available: an AU cannot report
/// that it *understood* a message. What this rules out is the previous
/// behaviour, where the call was never made at all.
#[test]
fn an_instrument_accepts_a_single_packet_sysex() {
    let _g = lock();
    for unit in SYSEX_SAFE_INSTRUMENTS {
        let au = unit.open(RATE, BLOCK);
        let events = vec![sysex7_packet(SYSEX7_STATUS_SINGLE, &IDENTITY_REQUEST)];

        // No panic and no unwind is the assertion; `send_midi` swallows
        // per-message errors by design so one refusal cannot abort a block.
        au.send_midi(&events);
    }
}

/// A SysEx spanning several packets is accepted, and the notes around it play.
///
/// The interleaving is the point: reassembly keeps state across events, so a
/// bug there could plausibly swallow the channel-voice messages sharing the
/// block. A stuck note is the audible form of that.
#[test]
fn a_multi_packet_sysex_does_not_disturb_the_notes_around_it() {
    let _g = lock();
    // 14 bytes — more than the 6 one packet carries, so this is a genuine
    // START / CONTINUE / END run.
    let body: Vec<u8> = (0..14u8).map(|i| 0x10 + i).collect();

    for unit in SYSEX_SAFE_INSTRUMENTS {
        let au = unit.open(RATE, BLOCK);

        let mut events = vec![MidiEvent::from_midi1_bytes(0, &[0x90, 60, 100])
            .expect("a note-on is a valid 3-byte message")];
        events.push(sysex7_packet(SYSEX7_STATUS_START, &body[0..6]));
        events.push(sysex7_packet(SYSEX7_STATUS_CONTINUE, &body[6..12]));
        events.push(sysex7_packet(SYSEX7_STATUS_END, &body[12..14]));
        events.push(
            MidiEvent::from_midi1_bytes(0, &[0x80, 60, 0])
                .expect("a note-off is a valid 3-byte message"),
        );

        au.send_midi(&events);
    }
}

/// A `CONTINUE` with no preceding `START` is dropped rather than sent.
///
/// This host may join a stream mid-message — a plugin loaded while a dump is in
/// flight, or a block boundary crossed. Sending the tail alone would hand the
/// AU a fragment with no header, which is worse than sending nothing: a synth
/// parsing it as a complete message acts on a truncated payload.
#[test]
fn an_orphaned_continuation_is_not_sent() {
    let _g = lock();
    for unit in SYSEX_SAFE_INSTRUMENTS {
        let au = unit.open(RATE, BLOCK);
        let events = vec![
            sysex7_packet(SYSEX7_STATUS_CONTINUE, &[0x11, 0x22, 0x33]),
            sysex7_packet(SYSEX7_STATUS_END, &[0x44]),
        ];
        au.send_midi(&events);
    }
}

/// The packet builder used above really does encode what UMP specifies.
///
/// Without this the tests are self-consistent and could still be wrong
/// together: a builder that mis-encoded the status nibble would produce packets
/// the host silently ignores, and every test above would pass by doing nothing.
#[test]
fn the_test_packet_builder_matches_the_ump_layout() {
    let ev = sysex7_packet(SYSEX7_STATUS_START, &[0x7E, 0x00, 0x06, 0x01, 0x02, 0x03]);
    let (status, bytes, n) = ev
        .sysex7_payload()
        .expect("the builder must produce a packet the decoder recognises");

    assert_eq!(status, SYSEX7_STATUS_START, "status nibble");
    assert_eq!(n, 6, "byte count");
    assert_eq!(&bytes[..6], &[0x7E, 0x00, 0x06, 0x01, 0x02, 0x03]);

    // And a short packet reports its real length rather than padding to 6.
    let short = sysex7_packet(SYSEX7_STATUS_END, &[0xF7]);
    let (_, _, n) = short.sysex7_payload().expect("short packet decodes");
    assert_eq!(n, 1, "a 1-byte packet must not report 6");
}
