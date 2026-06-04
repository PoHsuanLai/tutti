//! MIDI output device ports: enumeration + a background thread that owns the
//! open output connection and serializes events to MIDI 1.0 wire bytes.

use crate::MidiDevice;
use crossbeam_channel::{bounded, Receiver, Sender};
use midir::{MidiOutput, MidiOutputConnection};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tutti_midi_types::ump::MidiEvent;

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
    let mut conn: Option<MidiOutputConnection> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            OutputCmd::Connect(idx) => {
                conn.take();
                match connect_device(idx) {
                    Ok((c, name)) => {
                        conn = Some(c);
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
                conn.take();
                connected.store(false, Ordering::Release);
                device_name.store(Arc::new(None));
            }
            OutputCmd::Send(event) => {
                if let Some(ref mut c) = conn {
                    // Serialize to MIDI 1.0 wire bytes. Events without a
                    // 1.0 representation (per-note pitch bend, per-note
                    // controllers, SysEx fragments, utility) are dropped.
                    if let Some((bytes, len)) = event.to_midi1_bytes() {
                        let _ = c.send(&bytes[..len as usize]);
                    }
                }
            }
        }
    }
}

fn connect_device(
    device_index: usize,
) -> Result<(MidiOutputConnection, String), crate::error::Error> {
    let midi_output = MidiOutput::new("tutti-midi-output")?;
    let ports = midi_output.ports();
    let port = ports.get(device_index).ok_or_else(|| {
        crate::error::Error::MidiDevice(format!("MIDI output device {} not found", device_index))
    })?;
    let name = midi_output
        .port_name(port)
        .unwrap_or_else(|_| format!("Device {}", device_index));
    let conn = midi_output.connect(port, "tutti-output")?;
    Ok((conn, name))
}
