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
#[derive(Debug, Clone)]
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
/// SysEx (0xF0 … 0xF7) is not yet emitted into the UMP stream; it is dropped
/// silently. Support requires running `MidiEvent::sysex7_fragments` and
/// pushing each fragment.
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

    let connection = midi_input.connect(
        port,
        "tutti-input",
        move |timestamp, message, _| {
            if message.is_empty() {
                return;
            }

            // SysEx 0xF0 … 0xF7 not yet fragmented here; drop.
            if message[0] == 0xF0 {
                return;
            }

            let now = Instant::now();
            match MidiEvent::from_midi1_bytes(0, message) {
                Some(event) => {
                    if !producer_handle.push(event, now) {
                        debug!("MIDI input ring buffer full, dropping event");
                    }

                    if let Some(ref observer) = ui_observer {
                        let _ = observer.try_send(MidiInputRecord {
                            device_id,
                            device_name: observer_name.clone(),
                            event,
                            timestamp_us: timestamp,
                        });
                    }
                }
                None => {
                    debug!("Failed to parse MIDI event: {:02x?}", message);
                }
            }
        },
        (),
    )?;

    Ok((connection, port_name))
}
