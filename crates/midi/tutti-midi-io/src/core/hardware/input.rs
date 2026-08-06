//! MIDI input device ports: enumeration + opening a hardware input connection.

use super::MidiDevice;
use crate::core::sysex::Sysex7Assembler;
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

    // Reassembles SysEx across callbacks: a long dump can span several driver
    // buffers, and a middle/tail chunk may arrive without a leading `0xF0`.
    // One per connection — two ports sharing an assembler would splice their
    // dumps together.
    let mut sysex = Sysex7Assembler::new();
    // Scratch for the packets a completed run fragments into, reused across
    // callbacks so a steady SysEx stream stops allocating on this thread.
    let mut sysex_fragments: Vec<MidiEvent> = Vec::new();

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
            if message[0] == 0xF0 || sysex.in_flight() {
                sysex_fragments.clear();
                sysex.push(message, &mut sysex_fragments);
                for event in sysex_fragments.iter().copied() {
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
                // A run still in flight (no 0xF7 yet) emits nothing this call.
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

// SysEx reassembly lives in `crate::core::sysex` — it is OS-free, shared by every
// driver edge, and unit-testable without a device. Its tests moved with it.
