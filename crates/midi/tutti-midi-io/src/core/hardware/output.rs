//! MIDI output device ports: enumeration + a background thread that owns the
//! open output connection. The connection is a [`Midi1Port`] — a `midir` port is
//! a MIDI 1.0 endpoint — so it speaks [`MidiEvent`] and translates to wire bytes
//! at its own edge.

use super::MidiDevice;
use crossbeam_channel::{bounded, Receiver, Sender};
use midir::{MidiOutput, MidiOutputConnection};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::debug;
use tutti_midi_types::MidiEvent;

/// A MIDI output connection over `midir`. Because every OS MIDI API `midir`
/// targets presents a MIDI 1.0 byte stream, this is a MIDI 1.0 endpoint: it takes
/// engine-native [`MidiEvent`]s and translates them to 1.0 wire bytes at the edge
/// via [`MidiEvent::to_midi1_bytes`]. A MIDI-2-only message has no 1.0 form and
/// is dropped (the translation returns `None`).
pub(crate) struct Midi1Port {
    conn: MidiOutputConnection,
}

impl Midi1Port {
    /// Translate an engine-native [`MidiEvent`] to MIDI 1.0 wire bytes and send
    /// it. A message with no 1.0 form (MIDI-2-only: per-note pitch bend, per-note
    /// controllers, …) has no wire representation and is silently dropped
    /// (`to_midi1_bytes` returns `None` → `Ok(())`).
    fn send(&mut self, event: &MidiEvent) -> Result<(), crate::core::error::Error> {
        match event.to_midi1_bytes() {
            Some((bytes, len)) => self
                .conn
                .send(&bytes[..len as usize])
                .map_err(|e| crate::core::error::Error::MidiPort(e.to_string())),
            None => Ok(()),
        }
    }
}

/// Enumerate available MIDI output devices.
pub fn list_output_devices() -> Vec<MidiDevice> {
    let Ok(midi_output) = MidiOutput::new("tutti-device-list") else {
        return Vec::new();
    };
    midi_output
        .ports()
        .iter()
        .enumerate()
        .map(|(index, port)| MidiDevice {
            index,
            name: midi_output
                .port_name(port)
                .unwrap_or_else(|_| format!("Unknown Device {}", index)),
        })
        .collect()
}

pub(crate) enum OutputCmd {
    Connect(usize),
    Disconnect,
    Send(MidiEvent),
}

/// Background output thread state. Created by `MidiIo`.
pub(crate) struct OutputThread {
    pub tx: Sender<OutputCmd>,
    pub device_name: Arc<arc_swap::ArcSwap<Option<String>>>,
    pub connected: Arc<AtomicBool>,
}

impl OutputThread {
    pub fn spawn() -> Self {
        let (tx, rx) = bounded(1024);
        let device_name = Arc::new(arc_swap::ArcSwap::new(Arc::new(None)));
        let connected = Arc::new(AtomicBool::new(false));

        let device_name2 = Arc::clone(&device_name);
        let connected2 = Arc::clone(&connected);

        thread::Builder::new()
            .name("midi-output".to_string())
            .spawn(move || run_output_thread(rx, device_name2, connected2))
            .expect("Failed to spawn MIDI output thread");

        Self {
            tx,
            device_name,
            connected,
        }
    }
}

fn run_output_thread(
    rx: Receiver<OutputCmd>,
    device_name: Arc<arc_swap::ArcSwap<Option<String>>>,
    connected: Arc<AtomicBool>,
) {
    let mut port: Option<Midi1Port> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            OutputCmd::Connect(idx) => {
                port.take();
                match connect_device(idx) {
                    Ok((p, name)) => {
                        port = Some(p);
                        connected.store(true, Ordering::Release);
                        device_name.store(Arc::new(Some(name)));
                    }
                    Err(_) => {
                        connected.store(false, Ordering::Release);
                        device_name.store(Arc::new(None));
                    }
                }
            }
            OutputCmd::Disconnect => {
                port.take();
                connected.store(false, Ordering::Release);
                device_name.store(Arc::new(None));
            }
            OutputCmd::Send(event) => {
                if let Some(ref mut p) = port {
                    // The port translates to its wire form at the edge; a
                    // MIDI-2-only message with no MIDI 1.0 form is dropped there.
                    // A genuine driver write error is logged.
                    if let Err(e) = p.send(&event) {
                        debug!("MIDI output: {e}");
                    }
                }
            }
        }
    }
}

fn connect_device(device_index: usize) -> Result<(Midi1Port, String), crate::core::error::Error> {
    let midi_output = MidiOutput::new("tutti-midi-output")?;
    let ports = midi_output.ports();
    let port = ports.get(device_index).ok_or_else(|| {
        crate::core::error::Error::MidiDevice(format!(
            "MIDI output device {} not found",
            device_index
        ))
    })?;
    let name = midi_output
        .port_name(port)
        .unwrap_or_else(|_| format!("Device {}", device_index));
    let conn = midi_output.connect(port, "tutti-output")?;
    Ok((Midi1Port { conn }, name))
}

#[cfg(test)]
mod tests {
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::MidiEvent;

    /// The `Midi1Port::send` contract hinges on `to_midi1_bytes`: a message with
    /// a 1.0 form translates and is sent; a MIDI-2-only message returns `None`
    /// and `send` drops it (returns `Ok(())`). (We can't open a real
    /// `MidiOutputConnection` without hardware, so this pins the classification
    /// the port relies on.)
    #[test]
    fn midi1_representable_events_translate_others_are_dropped() {
        // A plain note-on has a MIDI 1.0 status → translatable.
        let note_on = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);
        assert!(
            note_on.to_midi1_bytes().is_some(),
            "note-on must have a 1.0 wire form"
        );

        // A per-note pitch bend is MIDI-2-only → no 1.0 form → dropped by `send`.
        let per_note_bend =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000_0000);
        assert!(
            per_note_bend.to_midi1_bytes().is_none(),
            "per-note pitch bend has no 1.0 wire form and is dropped on send"
        );
    }
}
