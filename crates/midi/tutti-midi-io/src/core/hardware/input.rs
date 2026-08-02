//! MIDI input device ports: enumeration + opening a hardware input connection.

use super::MidiDevice;
use crate::core::InputProducerHandle;
use crossbeam_channel::Sender;
use midir::{MidiInput, MidiInputConnection};
use std::time::Instant;
use tracing::debug;
use tutti_midi_types::ump::MidiEvent;

/// One MIDI event tagged with the source device. The observer channel
/// carries these so consumers can filter by originating device (e.g.
/// "follow MIDI clock from this hardware sequencer, ignore the
/// keyboard").
///
/// `device_id` is a stable per-connection identifier minted by the
/// input thread; it's the same id for every event from a given
/// connection until that device disconnects. `device_name` is
/// repeated on every event for cheap UI rendering; if that turns out
/// to be too costly we can move to an `Arc<str>` later.
///
/// `timestamp_us` is the midir-provided timestamp in microseconds
/// since the connection opened — monotonic per device, useful for
/// clock-tempo derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiInputRecord {
    pub device_id: u32,
    pub device_name: String,
    pub event: MidiEvent,
    pub timestamp_us: u64,
}

/// Enumerate available MIDI input devices.
pub fn list_input_devices() -> Vec<MidiDevice> {
    let Ok(midi_input) = MidiInput::new("tutti-device-list") else {
        return Vec::new();
    };
    midi_input
        .ports()
        .iter()
        .enumerate()
        .map(|(index, port)| MidiDevice {
            index,
            name: midi_input
                .port_name(port)
                .unwrap_or_else(|_| format!("Unknown Device {}", index)),
        })
        .collect()
}

/// Open a MIDI input device and register a callback that parses raw bytes into
/// [`MidiEvent`] (UMP), pushes them to the ring buffer, and optionally
/// notifies an observer.
///
/// SysEx (0xF0 … 0xF7) is reassembled across callbacks (a long dump can arrive
/// in several buffers) and fragmented into UMP SysEx7 packets via
/// [`MidiEvent::sysex7_fragments`]; each fragment flows to the ring + observer
/// like any other event, so downstream consumers (MIDI-CI, bulk dumps) can
/// reassemble the UMP run.
///
/// Returns the midir connection handle (drop it to disconnect) and the device
/// name.
pub(crate) fn connect_midi_input(
    device_index: usize,
    device_id: u32,
    producer_handle: InputProducerHandle,
    ui_observer: Option<Sender<MidiInputRecord>>,
) -> Result<(MidiInputConnection<()>, String), crate::core::error::Error> {
    let midi_input = MidiInput::new("tutti-midi-input")?;

    let ports = midi_input.ports();
    let port = ports.get(device_index).ok_or_else(|| {
        crate::core::error::Error::MidiDevice(format!("MIDI device {} not found", device_index))
    })?;

    let port_name = midi_input
        .port_name(port)
        .unwrap_or_else(|_| format!("Device {}", device_index));
    let observer_name = port_name.clone();

    // Accumulates raw SysEx bytes across callbacks: a long dump can span
    // several midir buffers, and a middle/tail chunk may arrive without a
    // leading `0xF0`. Non-empty ⇒ we're mid-SysEx.
    let mut sysex_buf: Vec<u8> = Vec::new();

    let connection = midi_input.connect(
        port,
        "tutti-input",
        move |timestamp, message, _| {
            if message.is_empty() {
                return;
            }

            let now = Instant::now();

            // SysEx path: a message that starts a dump (`0xF0`) or continues one
            // already in flight. Accumulate until `0xF7`, then fragment the
            // payload into UMP SysEx7 packets.
            if message[0] == 0xF0 || !sysex_buf.is_empty() {
                if let Some(fragments) = accumulate_sysex(&mut sysex_buf, message) {
                    for event in fragments {
                        emit_event(
                            event,
                            now,
                            timestamp,
                            device_id,
                            &observer_name,
                            &producer_handle,
                            ui_observer.as_ref(),
                        );
                    }
                }
                // Still mid-SysEx (no 0xF7 yet) — keep buffering, emit nothing.
                return;
            }

            match MidiEvent::from_midi1_bytes(0, message) {
                Some(event) => emit_event(
                    event,
                    now,
                    timestamp,
                    device_id,
                    &observer_name,
                    &producer_handle,
                    ui_observer.as_ref(),
                ),
                None => {
                    debug!("Failed to parse MIDI event: {:02x?}", message);
                }
            }
        },
        (),
    )?;

    Ok((connection, port_name))
}

/// Push one parsed [`MidiEvent`] to the audible ring and (best-effort) the UI
/// observer channel. Shared by the channel-voice and reassembled-SysEx paths.
#[allow(clippy::too_many_arguments)]
fn emit_event(
    event: MidiEvent,
    now: Instant,
    timestamp: u64,
    device_id: u32,
    device_name: &str,
    producer_handle: &InputProducerHandle,
    ui_observer: Option<&Sender<MidiInputRecord>>,
) {
    if !producer_handle.push(event, now) {
        debug!("MIDI input ring buffer full, dropping event");
    }
    if let Some(observer) = ui_observer {
        let _ = observer.try_send(MidiInputRecord {
            device_id,
            device_name: device_name.to_string(),
            event,
            timestamp_us: timestamp,
        });
    }
}

/// Accumulate raw MIDI 1.0 SysEx bytes across (possibly split) transport buffers.
///
/// Appends `message` to `buf`; once a terminating `0xF7` is present, extracts the
/// payload (the bytes *between* the leading `0xF0` and the `0xF7`), clears `buf`,
/// and returns that payload fragmented into UMP SysEx7 packets on group 0. While
/// the dump is still in flight (no `0xF7` yet) it returns `None` and keeps
/// buffering. Factored out of the midir callback so the reassembly is testable
/// without a live device.
fn accumulate_sysex(buf: &mut Vec<u8>, message: &[u8]) -> Option<Vec<MidiEvent>> {
    buf.extend_from_slice(message);
    let end = buf.iter().position(|&b| b == 0xF7)?;
    // Payload excludes the 0xF0 start (if present) and the 0xF7 end byte.
    let payload_start = usize::from(buf.first() == Some(&0xF0));
    let payload: Vec<u8> = buf[payload_start..end].to_vec();
    buf.clear();

    let mut fragments = Vec::new();
    MidiEvent::sysex7_fragments(0, &payload, &mut fragments);
    Some(fragments)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble the reassembled fragments back into the payload bytes.
    fn payload_of(fragments: &[MidiEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in fragments {
            let (_status, bytes, n) = f.sysex7_payload().expect("sysex7 packet");
            out.extend_from_slice(&bytes[..n]);
        }
        out
    }

    /// The MIDI-1 wire → MIDI-CI seam, end to end.
    ///
    /// MIDI-CI (M2-101) is Universal SysEx precisely so it works over a MIDI-1.0
    /// transport — that's how two devices negotiate *before* either knows the
    /// other speaks MIDI 2.0. midir is such a transport, so a real CI probe
    /// arrives here as raw `F0 7E … F7` bytes.
    ///
    /// This asserts the whole promotion chain: raw bytes → `accumulate_sysex` →
    /// UMP SysEx7 fragments → `Sysex7Reassembler` → a typed `CiMessage`. The
    /// codec's own round-trip tests start from UMP and so can't catch a break at
    /// the wire edge (a dropped `0xF0`, a mis-sized payload split).
    #[test]
    fn midi1_wire_sysex_promotes_to_a_typed_ci_message() {
        use tutti_midi_runtime::{CiInitiator, Sysex7Reassembler};
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
        ci_to_sysex7(0, &probe, &mut ump);
        let mut wire = vec![0xF0];
        wire.extend_from_slice(&payload_of(&ump));
        wire.push(0xF7);

        // Drive the driver's reassembly, then the UMP-side reassembler.
        let mut buf = Vec::new();
        let fragments = accumulate_sysex(&mut buf, &wire).expect("complete on the F7");
        let mut reassembler = Sysex7Reassembler::new();
        let decoded = fragments
            .iter()
            .find_map(|ev| reassembler.push(ev).and_then(|run| sysex7_to_ci(&run)));

        assert_eq!(
            decoded,
            Some(probe),
            "a CI probe off the MIDI-1 wire must reach the negotiators intact"
        );
    }

    #[test]
    fn single_buffer_sysex_fragments_payload() {
        let mut buf = Vec::new();
        // F0 <payload> F7 delivered in one buffer.
        let frags = accumulate_sysex(&mut buf, &[0xF0, 0x01, 0x02, 0x03, 0xF7])
            .expect("complete on the F7");
        assert_eq!(payload_of(&frags), vec![0x01, 0x02, 0x03]);
        assert!(buf.is_empty(), "buffer resets after a complete message");
    }

    #[test]
    fn split_sysex_reassembles_across_buffers() {
        let mut buf = Vec::new();
        // A dump split across three transport callbacks; only the last completes.
        assert!(accumulate_sysex(&mut buf, &[0xF0, 0x10, 0x11]).is_none());
        assert!(accumulate_sysex(&mut buf, &[0x12, 0x13]).is_none());
        let frags =
            accumulate_sysex(&mut buf, &[0x14, 0xF7]).expect("completes on the final chunk");
        assert_eq!(payload_of(&frags), vec![0x10, 0x11, 0x12, 0x13, 0x14]);
        assert!(buf.is_empty());
    }
}
