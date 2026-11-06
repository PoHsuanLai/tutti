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
mod tests;
