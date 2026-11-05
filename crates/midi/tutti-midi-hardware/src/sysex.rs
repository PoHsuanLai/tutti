//! MIDI 1.0 SysEx reassembly, and its promotion to UMP SysEx7.
//!
//! A driver delivers SysEx in whatever chunks the transport happened to produce:
//! a long dump spans several callbacks, a middle chunk arrives with no leading
//! `0xF0`, and two short dumps can share one buffer. [`Sysex7ByteAssembler`]
//! absorbs that and yields complete payloads as UMP SysEx7 packets.
//!
//! # Where it applies
//!
//! Neither backend uses it: ALSA's sequencer and CoreMIDI both deliver UMP, with
//! SysEx already fragmented. It is for a byte-stream source (a DIN or serial
//! port, a legacy driver), and for **MIDI-CI (M2-101)**, which is Universal
//! SysEx by design and so reaches a UMP-era engine as `F0 … F7` from such a
//! source.
//!
//! # Byte rules (MIDI 1.0 detailed specification, 4.2)
//!
//! - A System Realtime byte (`0xF8..=0xFF`) may appear anywhere, inside a SysEx
//!   too. It is emitted at once as its own event and does not touch the run.
//! - `0xF7` ends the run in flight. With none in flight it is ignored.
//! - `0xF0` starts a run. Inside a run it means the previous one lost its `0xF7`.
//! - Any other status byte also ends the run, and the run is **discarded**, not
//!   emitted truncated: a SysEx cut short is not a message, which is also how
//!   the overflow and restart cases are treated. The status byte itself is not
//!   a SysEx byte; the caller owns the channel and common messages.
//! - Data bytes with no run in flight are not SysEx and are ignored.
//!
//! # OS-free on purpose
//!
//! Nothing here touches a driver, so every branch is unit-testable without a
//! device. Keep it that way: inside a driver callback the completion, overflow
//! and multi-run paths have no test surface at all, which is how the three bugs
//! the tests below pin survived for as long as they did.
//!
//! The allocation gate lives in `tests/rt_no_alloc_sysex.rs` rather than here:
//! `assert_no_alloc` is inert without a `#[global_allocator]`, which only a test
//! binary can install.

use tracing::warn;
use tutti_midi_types::MidiGroup;
use tutti_midi_types::ump::{MidiEvent, SYSEX7_MAX_PAYLOAD};

/// Ceiling on one in-flight SysEx payload, in bytes: the engine's
/// [`SYSEX7_MAX_PAYLOAD`], which the plugin hosts' reassemblers also hold.
/// A dump's packets are emitted together, so how much of a dump reaches a
/// plugin is set by the event caps after this, not by this cap: a plugin
/// node takes at most 256 MIDI events a chunk (a device callback; a block
/// for the in-process VST2 node), 1536 SysEx payload bytes.
const MAX_SYSEX_BYTES: usize = SYSEX7_MAX_PAYLOAD;

/// Reassembles MIDI 1.0 SysEx runs across transport buffers and emits them as
/// UMP SysEx7 packets.
///
/// One per open input port: the in-flight state is per-connection, and two ports
/// interleaving into one assembler would splice their dumps together.
#[derive(Debug, Default)]
pub struct Sysex7ByteAssembler {
    /// Payload of the run in flight, excluding the `0xF0` and `0xF7`.
    buf: Vec<u8>,
    /// Between an `0xF0` and the byte that ends the run.
    active: bool,
    /// True once the in-flight run has overflowed [`MAX_SYSEX_BYTES`]. The run is
    /// then discarded up to its terminating `0xF7`, so a fault costs one message
    /// rather than desynchronising every message after it.
    overflowed: bool,
}

impl Sysex7ByteAssembler {
    /// A fresh assembler with nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a run is currently in flight.
    ///
    /// The caller uses this to decide whether a buffer with no leading `0xF0` is
    /// a SysEx continuation or an ordinary channel-voice message.
    pub fn in_flight(&self) -> bool {
        self.active
    }

    /// Feeds one transport buffer and appends to `out` every completed run's UMP
    /// SysEx7 packets and every System Realtime message met on the way (see the
    /// module docs for the byte rules).
    ///
    /// Returns the number of *runs* completed by this call — normally 0 (still
    /// buffering) or 1, but genuinely more when a buffer carries several short
    /// dumps. Realtime events are not counted.
    ///
    /// `out` is caller-owned and reused across calls, so buffering a run in
    /// flight allocates nothing once warm — which matters because this runs on
    /// the driver callback thread.
    ///
    /// **Completing** a run still allocates, and not here:
    /// [`MidiEvent::sysex7_fragments`] builds a `Sysex7::<Vec<u32>>` internally
    /// on every call. Fixing that needs an array-backed or reusable-builder API
    /// in `tutti-midi-types`; it cannot be done from this crate.
    /// `tests/rt_no_alloc_sysex.rs` gates the part that is reachable and
    /// documents the part that is not.
    pub fn push(&mut self, message: &[u8], out: &mut Vec<MidiEvent>) -> usize {
        let mut completed = 0;
        let mut i = 0;
        while i < message.len() {
            let b = message[i];
            if b < 0x80 {
                // A stretch of data bytes: payload when a run is in flight,
                // otherwise somebody else's message.
                let end = message[i..]
                    .iter()
                    .position(|&b| b >= 0x80)
                    .map_or(message.len(), |n| i + n);
                if self.active {
                    self.extend(&message[i..end]);
                }
                i = end;
                continue;
            }
            i += 1;
            match b {
                0xF8..=0xFF => {
                    if let Some(ev) = MidiEvent::from_midi1_bytes(0, &[b]) {
                        out.push(ev);
                    }
                }
                0xF0 => {
                    if self.active && !self.overflowed && !self.buf.is_empty() {
                        warn!("SysEx restart (0xF0) mid-run — the previous run lost its 0xF7");
                    }
                    self.buf.clear();
                    self.overflowed = false;
                    self.active = true;
                }
                0xF7 => {
                    if self.active && !self.overflowed {
                        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &self.buf, out);
                        completed += 1;
                    }
                    self.end_run();
                }
                _ => {
                    if self.active {
                        warn!(
                            status = b,
                            "status byte inside a SysEx run — discarding the run"
                        );
                    }
                    self.end_run();
                }
            }
        }
        completed
    }

    fn end_run(&mut self) {
        self.buf.clear();
        self.active = false;
        self.overflowed = false;
    }

    /// Append payload bytes to the in-flight run, enforcing [`MAX_SYSEX_BYTES`].
    fn extend(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.buf.len() + bytes.len() > MAX_SYSEX_BYTES {
            warn!(
                limit = MAX_SYSEX_BYTES,
                "SysEx run exceeded the size ceiling — discarding it. A lost 0xF7 \
                 (device unplugged mid-dump, or a dropped buffer) looks like this."
            );
            self.buf.clear();
            self.overflowed = true;
            return;
        }
        self.buf.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten SysEx7 packets back into the payload bytes they carry.
    fn payload_of(fragments: &[MidiEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in fragments {
            let (_status, bytes, n) = f.sysex7_payload().expect("sysex7 packet");
            out.extend_from_slice(&bytes[..n]);
        }
        out
    }

    /// Drive one buffer through a fresh assembler and return its packets.
    fn run(message: &[u8]) -> (Vec<MidiEvent>, usize) {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        let n = asm.push(message, &mut out);
        (out, n)
    }

    #[test]
    fn single_buffer_sysex_fragments_payload() {
        let (frags, n) = run(&[0xF0, 0x01, 0x02, 0x03, 0xF7]);
        assert_eq!(n, 1);
        assert_eq!(payload_of(&frags), vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn split_sysex_reassembles_across_buffers() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        // A dump split across three transport callbacks; only the last completes.
        assert_eq!(asm.push(&[0xF0, 0x10, 0x11], &mut out), 0);
        assert_eq!(asm.push(&[0x12, 0x13], &mut out), 0);
        assert_eq!(asm.push(&[0x14, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x10, 0x11, 0x12, 0x13, 0x14]);
        assert!(!asm.in_flight(), "assembler resets after a complete run");
    }

    /// Bytes after a terminating `0xF7` start the next run.
    ///
    /// A device that packs two short dumps into one transport write must get
    /// both; clearing the buffer at the first `0xF7` would lose the second with
    /// no error anywhere.
    #[test]
    fn two_runs_in_one_buffer_both_survive() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        let n = asm.push(&[0xF0, 0x01, 0x02, 0xF7, 0xF0, 0x03, 0x04, 0xF7], &mut out);
        assert_eq!(n, 2, "both runs must complete");
        assert_eq!(
            payload_of(&out),
            vec![0x01, 0x02, 0x03, 0x04],
            "the second run's payload must not be dropped"
        );
    }

    /// The asymmetric case: a trailing partial run must be retained.
    #[test]
    fn run_after_a_terminator_keeps_buffering() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01, 0xF7, 0xF0, 0x02], &mut out), 1);
        assert!(
            asm.in_flight(),
            "the trailing partial run is still in flight"
        );
        assert_eq!(asm.push(&[0x03, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x01, 0x02, 0x03]);
    }

    /// The buffer has a ceiling.
    ///
    /// A lost `0xF7` (device unplugged mid-dump) must not grow a buffer for the
    /// lifetime of the connection. The run is abandoned, and the *next*
    /// terminator resyncs rather than splicing the garbage onto a good message.
    #[test]
    fn an_unterminated_run_is_capped_and_resyncs() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();

        let mut flood = vec![0xF0];
        flood.extend(std::iter::repeat_n(0x7F, MAX_SYSEX_BYTES + 1));
        assert_eq!(asm.push(&flood, &mut out), 0, "no run completes");
        assert!(out.is_empty(), "the overflowed run emits nothing");

        // More of the same dump keeps being discarded, not buffered.
        assert_eq!(asm.push(&[0x7F; 64], &mut out), 0);

        // Its terminator resyncs the assembler without emitting the garbage...
        assert_eq!(asm.push(&[0xF7], &mut out), 0, "the bad run is not emitted");
        assert!(out.is_empty());
        assert!(!asm.in_flight(), "and the assembler is clean again");

        // ...so the next good message comes through intact.
        assert_eq!(asm.push(&[0xF0, 0x0A, 0x0B, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x0A, 0x0B]);
    }

    // That a completed SysEx allocates nothing on the driver callback thread is
    // covered by `tests/rt_no_alloc_sysex.rs`, not here: `assert_no_alloc` only
    // observes anything when a `#[global_allocator] = AllocDisabler` is
    // installed at the test-binary root, so a unit-test version would be
    // silently inert.

    /// A `0xF0` arriving mid-run restarts it — it cannot be payload.
    ///
    /// SysEx7 payload is 7-bit by definition (`sysex7_fragments` masks
    /// `b & 0x7F`), so keeping the `0xF0` would silently emit `0x70` and corrupt
    /// the message. Mid-run it means the previous dump lost its `0xF7`, and
    /// dropping that truncated run beats splicing it onto a good one.
    #[test]
    fn f0_mid_run_restarts_rather_than_corrupting() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01], &mut out), 0);
        assert_eq!(asm.push(&[0xF0, 0x02, 0xF7], &mut out), 1);
        assert_eq!(
            payload_of(&out),
            vec![0x02],
            "the truncated run is dropped; only the restarted one is emitted"
        );
    }

    fn rt_status(ev: &MidiEvent) -> u8 {
        ((ev.data_words()[0] >> 16) & 0xFF) as u8
    }

    // Mutations: treat `0xF8..=0xFF` as data (drop the arm) and the clock is
    // folded into the payload as 0x78, or lost; emit before the run ends vs
    // after is pinned by the order assert.
    #[test]
    fn realtime_inside_a_run_is_passed_through_not_data() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01, 0xF8, 0x02, 0xF7], &mut out), 1);
        assert_eq!(rt_status(&out[0]), 0xF8, "the clock comes out at once");
        assert_eq!(
            payload_of(&out[1..]),
            vec![0x01, 0x02],
            "payload is unbroken"
        );
        // Across a buffer boundary, and for a byte that is its own buffer.
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01], &mut out), 0);
        assert_eq!(asm.push(&[0xFA], &mut out), 0);
        assert!(asm.in_flight(), "a realtime byte does not end the run");
        assert_eq!(asm.push(&[0x02, 0xF7], &mut out), 1);
        assert_eq!(rt_status(&out[0]), 0xFA);
        assert_eq!(payload_of(&out[1..]), vec![0x01, 0x02]);
        // An undefined realtime byte is dropped, not data.
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01, 0xF9, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x01]);
    }

    // Mutation: make the catch-all arm a no-op (as before): the Note On is
    // swallowed into the payload and the F7 completes `[01, 02, 10, 3C, 40]`.
    #[test]
    fn a_status_byte_discards_the_run() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF0, 0x01, 0x02, 0x90, 0x3C, 0x40], &mut out), 0);
        assert!(!asm.in_flight());
        assert_eq!(asm.push(&[0xF7], &mut out), 0, "the late F7 is a lone one");
        assert!(out.is_empty(), "nothing truncated is emitted");
        // The next run is clean.
        assert_eq!(asm.push(&[0xF0, 0x05, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x05]);
        // Common messages end a run too.
        assert_eq!(asm.push(&[0xF0, 0x01, 0xF1, 0x20, 0xF7], &mut out), 0);
        assert_eq!(payload_of(&out), vec![0x05]);
    }

    // Mutation: emit on every F7 regardless of `active`: a lone F7 emits an
    // empty SINGLE, and `90 3C 40 F7` emits `[10, 3C, 40]` (data bytes taken
    // as payload when nothing is in flight).
    #[test]
    fn a_lone_f7_and_stray_data_are_ignored() {
        let mut asm = Sysex7ByteAssembler::new();
        let mut out = Vec::new();
        assert_eq!(asm.push(&[0xF7], &mut out), 0);
        assert_eq!(asm.push(&[0x90, 0x3C, 0x40, 0xF7], &mut out), 0);
        assert_eq!(asm.push(&[0x12, 0x34], &mut out), 0);
        assert!(out.is_empty());
        assert!(!asm.in_flight());
    }

    /// The MIDI-1 wire → MIDI-CI seam, end to end.
    ///
    /// MIDI-CI (M2-101) is Universal SysEx precisely so it works over a MIDI-1.0
    /// transport — that's how two devices negotiate *before* either knows the
    /// other speaks MIDI 2.0. So a real CI probe arrives here as raw
    /// `F0 7E … F7` bytes even on a UMP-era stack.
    ///
    /// This asserts the whole promotion chain: raw bytes → `Sysex7ByteAssembler` →
    /// UMP SysEx7 fragments → `Sysex7PacketReassembler` → a typed `CiMessage`. The
    /// codec's own round-trip tests start from UMP and so can't catch a break at
    /// the wire edge (a dropped `0xF0`, a mis-sized payload split).
    #[test]
    fn midi1_wire_sysex_promotes_to_a_typed_ci_message() {
        use tutti_midi_runtime::{CiInitiator, Sysex7PacketReassembler};
        use tutti_midi_types::ci::{DiscoveryData, Muid, ci_to_sysex7, sysex7_to_ci};

        // A Discovery probe exactly as a peer device would send it.
        let peer = CiInitiator::new(
            Muid::from_seed(0x1234),
            DiscoveryData {
                manufacturer: [0x00, 0x21, 0x09],
                family: 0x0042,
                family_model: 0x0007,
                software_revision: [1, 2, 3, 4],
                categories: Default::default(),
                max_sysex_size: 512,
                // A Discovery, not a reply: §5.5.4 makes the path id the
                // initiator's own, and `function_block` is reply-only, so
                // `NO_FUNCTION_BLOCK` is the value §5.6.2 names for a device
                // that represents none.
                output_path_id: 0,
                function_block: tutti_midi_types::ci::NO_FUNCTION_BLOCK,
            },
        );
        let probe = peer.discovery();

        // Encode it, then flatten back to the MIDI-1.0 byte stream a hardware
        // port actually delivers: F0 <payload> F7.
        let mut ump = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &probe, &mut ump);
        let mut wire = vec![0xF0];
        wire.extend_from_slice(&payload_of(&ump));
        wire.push(0xF7);

        // Drive the driver's reassembly, then the UMP-side reassembler.
        let mut asm = Sysex7ByteAssembler::new();
        let mut fragments = Vec::new();
        assert_eq!(asm.push(&wire, &mut fragments), 1, "complete on the F7");

        let mut reassembler = Sysex7PacketReassembler::new();
        let decoded = fragments
            .iter()
            .find_map(|ev| reassembler.push(ev).and_then(|run| sysex7_to_ci(&run)));

        assert_eq!(
            decoded,
            Some(probe),
            "a CI probe off the MIDI-1 wire must reach the negotiators intact"
        );
    }
}
