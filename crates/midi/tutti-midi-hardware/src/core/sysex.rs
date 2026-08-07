//! MIDI 1.0 SysEx reassembly, and its promotion to UMP SysEx7.
//!
//! A driver hands us SysEx in whatever chunks the transport happened to produce:
//! a long dump spans several callbacks, a middle chunk arrives with no leading
//! `0xF0`, and two short dumps can share one buffer. [`Sysex7ByteAssembler`] absorbs
//! that and yields complete payloads as UMP SysEx7 packets.
//!
//! # Why this survives the move to native UMP
//!
//! It looks like MIDI-1.0-only code, and it is not. Two live sources still
//! deliver `F0 … F7` runs to a UMP-era engine:
//!
//! - **ALSA's kernel converter.** A legacy seq client bridged to a UMP client
//!   emits MIDI-1.0 byte runs; the kernel converts channel-voice messages but
//!   SysEx still arrives as a byte stream to be fragmented.
//! - **MIDI-CI (M2-101) is Universal SysEx *by design*** — it is how two devices
//!   negotiate *before* either knows the other speaks MIDI 2.0. So it arrives
//!   this way even on an endpoint that will go on to speak MIDI 2.0.
//!
//! # OS-free on purpose
//!
//! Nothing here touches a driver, so every branch is unit-testable without a
//! device. In its previous home — inside the midir input callback — the
//! completion, overflow, and multi-run paths had no test surface at all, which
//! is how the three bugs below survived.
//!
//! The allocation gate lives in `tests/rt_no_alloc_sysex.rs` rather than here:
//! `assert_no_alloc` is inert without a `#[global_allocator]`, which only a test
//! binary can install.

use tracing::warn;
use tutti_midi_types::tutti_types::MidiGroup;
use tutti_midi_types::ump::MidiEvent;

/// Ceiling on one in-flight SysEx payload, in bytes.
///
/// A dump that exceeds this is abandoned rather than buffered forever. The
/// hazard is not a large dump — it is a **lost `0xF7`**: a device unplugged
/// mid-dump, or a dropped buffer, leaves the assembler mid-run with no
/// terminator ever arriving, and an uncapped `Vec` then grows for the lifetime
/// of the connection.
///
/// 64 KiB is chosen to sit above real traffic and below anything alarming: the
/// largest thing MIDI-CI negotiates is a Property Exchange chunk, and CI's own
/// `max_sysex_size` is a 28-bit field that devices populate with values in the
/// hundreds-to-low-thousands of bytes. A dump past this is a fault, not a big
/// message.
const MAX_SYSEX_BYTES: usize = 64 * 1024;

/// Reassembles MIDI 1.0 SysEx runs across transport buffers and emits them as
/// UMP SysEx7 packets.
///
/// One per open input port: the in-flight state is per-connection, and two ports
/// interleaving into one assembler would splice their dumps together.
#[derive(Debug, Default)]
pub struct Sysex7ByteAssembler {
    /// Bytes of the run currently in flight, excluding any leading `0xF0`.
    /// Non-empty ⇒ mid-SysEx.
    buf: Vec<u8>,
    /// True once the in-flight run has overflowed [`MAX_SYSEX_BYTES`]. The run is
    /// then discarded up to its terminating `0xF7`, so a fault costs one message
    /// rather than desynchronising every message after it.
    overflowed: bool,
}

impl Sysex7ByteAssembler {
    /// A fresh assembler with nothing in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a run is currently in flight.
    ///
    /// The caller uses this to decide whether a buffer with no leading `0xF0` is
    /// a SysEx continuation or an ordinary channel-voice message.
    pub fn in_flight(&self) -> bool {
        !self.buf.is_empty() || self.overflowed
    }

    /// Feed one transport buffer; append every completed run's UMP SysEx7
    /// packets to `out`.
    ///
    /// Returns the number of *runs* completed by this call — normally 0 (still
    /// buffering) or 1, but genuinely more when a buffer carries several short
    /// dumps.
    ///
    /// `out` is caller-owned and reused across calls, so buffering a run in
    /// flight allocates nothing once warm — which matters because this runs on
    /// the driver callback thread.
    ///
    /// **Completing** a run still allocates, and not here:
    /// [`MidiEvent::sysex7_fragments`] builds a `Sysex7::<Vec<u32>>` internally
    /// on every call (`tutti-midi-types/src/ump/sysex.rs:33`). Fixing that needs
    /// an array-backed or reusable-builder API in `tutti-midi-types`; it cannot
    /// be done from this crate. `tests/rt_no_alloc_sysex.rs` gates the part that
    /// is reachable and documents the part that is not.
    pub fn push(&mut self, message: &[u8], out: &mut Vec<MidiEvent>) -> usize {
        let mut completed = 0;
        let mut rest = message;

        while !rest.is_empty() {
            // `0xF7` terminates the run in flight. Anything before it belongs to
            // that run; anything after it is the next message and must be
            // re-examined rather than dropped.
            let Some(end) = rest.iter().position(|&b| b == 0xF7) else {
                // No terminator in this buffer: the whole remainder is payload.
                self.extend(rest);
                return completed;
            };

            self.extend(&rest[..end]);
            rest = &rest[end + 1..];

            if self.overflowed {
                // The run was abandoned; its terminator just arrived, so resync
                // and emit nothing for it.
                self.buf.clear();
                self.overflowed = false;
            } else {
                MidiEvent::sysex7_fragments(MidiGroup::FIRST, &self.buf, out);
                self.buf.clear();
                completed += 1;
            }
        }

        completed
    }

    /// Append payload bytes to the in-flight run, handling a `0xF0` start and
    /// enforcing [`MAX_SYSEX_BYTES`].
    fn extend(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        // A `0xF0` **restarts** the run rather than joining it as payload.
        //
        // SysEx7 payload bytes are 7-bit by definition, and `sysex7_fragments`
        // enforces that with `b & 0x7F` — so a retained `0xF0` would silently
        // become `0x70` and corrupt the message. Arriving mid-run it means the
        // previous dump lost its `0xF7` (device unplugged, dropped buffer), and
        // the honest reading is "that run is gone, this is a new one" rather
        // than splicing a truncated dump onto a good one.
        let bytes = match bytes.iter().rposition(|&b| b == 0xF0) {
            Some(start) => {
                if !self.buf.is_empty() {
                    warn!("SysEx restart (0xF0) mid-run — the previous run lost its 0xF7");
                    self.buf.clear();
                }
                &bytes[start + 1..]
            }
            None => bytes,
        };
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

    /// BUG 1 (fixed): bytes after the terminating `0xF7` were discarded.
    ///
    /// The old code found the first `0xF7`, took the payload before it, then
    /// `buf.clear()`ed — silently dropping everything after it in the same
    /// buffer. A device that packs two short dumps into one transport write lost
    /// the second one entirely, with no error anywhere.
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

    /// BUG 1, the asymmetric case: a trailing partial run must be retained.
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

    /// BUG 2 (fixed): the buffer had no ceiling.
    ///
    /// A lost `0xF7` — device unplugged mid-dump — left the old `Vec` growing for
    /// the lifetime of the connection. Now the run is abandoned, and the *next*
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

        // Its terminator resyncs us without emitting the garbage...
        assert_eq!(asm.push(&[0xF7], &mut out), 0, "the bad run is not emitted");
        assert!(out.is_empty());
        assert!(!asm.in_flight(), "and the assembler is clean again");

        // ...so the next good message comes through intact.
        assert_eq!(asm.push(&[0xF0, 0x0A, 0x0B, 0xF7], &mut out), 1);
        assert_eq!(payload_of(&out), vec![0x0A, 0x0B]);
    }

    // BUG 3 (two `Vec` allocations per completed SysEx on the driver callback
    // thread) is covered by `tests/rt_no_alloc_sysex.rs`, not here.
    //
    // It cannot be tested from inside `src/`: `assert_no_alloc` only observes
    // anything when a `#[global_allocator] = AllocDisabler` is installed, and
    // that must be declared at the test-binary root. A unit-test version of this
    // gate is silently inert — it passes whether or not the bug is present,
    // which mutation-testing confirmed.

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
        use tutti_midi_types::ci::{ci_to_sysex7, sysex7_to_ci, DiscoveryData, Muid};

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
