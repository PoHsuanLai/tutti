//! Unified MIDI hardware I/O.
//!
//! `MidiIo` is the single entry point for hardware MIDI — connecting devices,
//! sending events, and receiving events via ring buffers. Clone is cheap (Arc).

use crate::error::{Error, Result};
use crate::io::{
    connect_midi_input, list_input_devices, list_output_devices, MidiInputRecord, OutputCmd,
    OutputThread,
};
use crate::{InputProducerHandle, MidiPortManager};
use crossbeam_channel::{bounded, Receiver, Sender};
use midir::MidiInputConnection;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::debug;
use tutti_midi_types::ump::MidiEvent;

/// A detected MIDI device (input or output).
#[derive(Debug, Clone)]
pub struct MidiDevice {
    pub index: usize,
    pub name: String,
}

/// Hardware MIDI I/O. Clone is cheap (Arc internally).
///
/// Spawns dedicated input and output threads on creation.
/// Both threads idle when no device is connected.
#[derive(Clone)]
pub struct MidiIo {
    inner: Arc<Inner>,
}

struct Inner {
    port_manager: Arc<MidiPortManager>,
    input_tx: Sender<InputCmd>,
    /// Snapshot of currently-connected input device names. Mutated only by the
    /// input thread; readers see a consistent view via `ArcSwap`.
    connected_inputs: Arc<arc_swap::ArcSwap<Vec<String>>>,
    /// Snapshot of `(device_id, device_name)` for every active input
    /// connection. Mutated only by the input thread on Connect /
    /// Disconnect. Lets UI surfaces (e.g. an "external MIDI clock from
    /// device X" picker) translate the human-readable name they show
    /// back to the integer `device_id` carried on each
    /// [`MidiInputRecord`].
    connected_input_ids: Arc<arc_swap::ArcSwap<Vec<(u32, String)>>>,
    output: OutputThread,
}

enum InputCmd {
    /// Open an input device by index, attach the given producer handle, and
    /// register the connection under `device_name` so subsequent
    /// `Disconnect(name)` can find it.
    Connect {
        device_index: usize,
        device_name: String,
        producer: InputProducerHandle,
    },
    Disconnect(String),
    DisconnectAll,
    SetObserver(Sender<MidiInputRecord>),
}

/// Find a device by case-insensitive substring match.
fn find_device<'a>(devices: &'a [MidiDevice], name: &str) -> Option<&'a MidiDevice> {
    let lower = name.to_lowercase();
    devices
        .iter()
        .find(|d| d.name.to_lowercase().contains(&lower))
}

impl MidiIo {
    /// Create a new MIDI I/O system. Spawns input and output threads immediately.
    pub fn new(port_manager: Arc<MidiPortManager>) -> Self {
        let (input_tx, input_rx) = bounded(16);
        let connected_inputs = Arc::new(arc_swap::ArcSwap::new(Arc::new(Vec::<String>::new())));
        let connected_input_ids =
            Arc::new(arc_swap::ArcSwap::new(Arc::new(Vec::<(u32, String)>::new())));
        let connected_for_thread = Arc::clone(&connected_inputs);
        let connected_ids_for_thread = Arc::clone(&connected_input_ids);

        thread::Builder::new()
            .name("midi-input".to_string())
            .spawn(move || {
                run_input_thread(input_rx, connected_for_thread, connected_ids_for_thread)
            })
            .expect("Failed to spawn MIDI input thread");

        Self {
            inner: Arc::new(Inner {
                port_manager,
                input_tx,
                connected_inputs,
                connected_input_ids,
                output: OutputThread::spawn(),
            }),
        }
    }

    // --- Device enumeration ---

    pub fn list_input_devices(&self) -> Vec<MidiDevice> {
        list_input_devices()
    }

    pub fn list_output_devices(&self) -> Vec<MidiDevice> {
        list_output_devices()
    }

    // --- Input ---

    /// Open an input device by index. Idempotent if the device's name is
    /// already present in the active connection set.
    pub fn connect_input(&self, device_index: usize) -> Result<()> {
        let devices = list_input_devices();
        let name = devices
            .get(device_index)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("MIDI Device {device_index}"));

        if self.is_input_connected(&name) {
            return Ok(());
        }

        let port_index = self.inner.port_manager.create_input_port(&name);
        let producer = self
            .inner
            .port_manager
            .get_input_producer_handle(port_index)
            .ok_or_else(|| Error::MidiDevice("Failed to get producer handle".to_string()))?;

        self.inner
            .input_tx
            .send(InputCmd::Connect {
                device_index,
                device_name: name,
                producer,
            })
            .map_err(|_| Error::MidiDevice("MIDI input thread not running".to_string()))
    }

    /// Open an input device by case-insensitive substring match. Idempotent.
    pub fn connect_input_by_name(&self, name: &str) -> Result<()> {
        let devices = list_input_devices();
        let device = find_device(&devices, name).ok_or_else(|| {
            Error::MidiDevice(format!("No MIDI input device matching '{name}'"))
        })?;
        if self.is_input_connected(&device.name) {
            return Ok(());
        }
        self.connect_input(device.index)
    }

    /// Disconnect a single input device by name. Silently no-ops if not connected.
    pub fn disconnect_input(&self, name: &str) {
        let _ = self
            .inner
            .input_tx
            .send(InputCmd::Disconnect(name.to_string()));
    }

    /// Disconnect every connected input device.
    pub fn disconnect_all_inputs(&self) {
        let _ = self.inner.input_tx.send(InputCmd::DisconnectAll);
    }

    /// True if at least one input device is connected.
    pub fn is_any_input_connected(&self) -> bool {
        !self.inner.connected_inputs.load().is_empty()
    }

    /// True if a device with the given name is currently connected.
    pub fn is_input_connected(&self, name: &str) -> bool {
        self.inner
            .connected_inputs
            .load()
            .iter()
            .any(|n| n == name)
    }

    /// Snapshot of all currently-connected input device names.
    pub fn connected_input_names(&self) -> Vec<String> {
        self.inner.connected_inputs.load().as_ref().clone()
    }

    /// Snapshot of `(device_id, name)` for every currently-connected input.
    /// The `device_id` matches the one tagged on each [`MidiInputRecord`]
    /// from that device, so consumers can resolve a UI-selected name to
    /// the integer key they filter on.
    pub fn connected_inputs_with_ids(&self) -> Vec<(u32, String)> {
        self.inner.connected_input_ids.load().as_ref().clone()
    }

    /// Look up the `device_id` currently bound to `name`. Returns `None`
    /// when no input device with that name is connected.
    pub fn device_id_for_name(&self, name: &str) -> Option<u32> {
        self.inner
            .connected_input_ids
            .load()
            .iter()
            .find_map(|(id, n)| (n == name).then_some(*id))
    }

    /// Register a channel to receive copies of all incoming MIDI events
    /// tagged with their source device. The observer is shared across
    /// all connected inputs; each event carries the originating
    /// [`MidiInputRecord::device_id`] so consumers can filter (e.g.
    /// "follow MIDI clock from the hardware sequencer only").
    pub fn set_input_observer(&self, sender: Sender<MidiInputRecord>) {
        let _ = self.inner.input_tx.send(InputCmd::SetObserver(sender));
    }

    // --- Output ---

    pub fn connect_output(&self, device_index: usize) -> Result<()> {
        self.inner
            .output
            .tx
            .send(OutputCmd::Connect(device_index))
            .map_err(|_| Error::MidiDevice("MIDI output thread not running".to_string()))
    }

    pub fn connect_output_by_name(&self, name: &str) -> Result<()> {
        let devices = list_output_devices();
        let idx = find_device(&devices, name)
            .map(|d| d.index)
            .ok_or_else(|| {
                Error::MidiDevice(format!("No MIDI output device matching '{name}'"))
            })?;
        self.connect_output(idx)
    }

    pub fn disconnect_output(&self) {
        let _ = self.inner.output.tx.send(OutputCmd::Disconnect);
    }

    pub fn is_output_connected(&self) -> bool {
        self.inner.output.connected.load(Ordering::Acquire)
    }

    pub fn output_device_name(&self) -> Option<String> {
        self.inner.output.device_name.load().as_ref().clone()
    }

    /// Send a MIDI event to the connected output device.
    pub fn send(&self, event: MidiEvent) {
        if let Err(e) = self.inner.output.tx.try_send(OutputCmd::Send(event)) {
            debug!("MIDI output channel full or disconnected: {}", e);
        }
    }

    // --- Port manager access ---

    pub fn port_manager(&self) -> &Arc<MidiPortManager> {
        &self.inner.port_manager
    }
}

// --- Input thread ---

fn run_input_thread(
    rx: Receiver<InputCmd>,
    connected_inputs: Arc<arc_swap::ArcSwap<Vec<String>>>,
    connected_input_ids: Arc<arc_swap::ArcSwap<Vec<(u32, String)>>>,
) {
    use std::collections::HashMap;
    let mut connections: HashMap<String, MidiInputConnection<()>> = HashMap::new();
    let mut device_ids: HashMap<String, u32> = HashMap::new();
    let mut ui_observer: Option<Sender<MidiInputRecord>> = None;
    let next_device_id = AtomicU32::new(1);

    let publish = |connections: &HashMap<String, MidiInputConnection<()>>,
                   device_ids: &HashMap<String, u32>| {
        let names: Vec<String> = connections.keys().cloned().collect();
        connected_inputs.store(Arc::new(names));
        let pairs: Vec<(u32, String)> = device_ids
            .iter()
            .filter(|(name, _)| connections.contains_key(*name))
            .map(|(name, id)| (*id, name.clone()))
            .collect();
        connected_input_ids.store(Arc::new(pairs));
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            InputCmd::Connect {
                device_index,
                device_name,
                producer,
            } => {
                if connections.contains_key(&device_name) {
                    continue;
                }
                let device_id = next_device_id.fetch_add(1, Ordering::Relaxed);
                match connect_midi_input(device_index, device_id, producer, ui_observer.clone()) {
                    Ok((conn, name)) => {
                        device_ids.insert(name.clone(), device_id);
                        connections.insert(name, conn);
                        publish(&connections, &device_ids);
                    }
                    Err(e) => {
                        debug!("Failed to connect MIDI input '{}': {:?}", device_name, e);
                    }
                }
            }
            InputCmd::Disconnect(name) => {
                if connections.remove(&name).is_some() {
                    device_ids.remove(&name);
                    publish(&connections, &device_ids);
                }
            }
            InputCmd::DisconnectAll => {
                if !connections.is_empty() {
                    connections.clear();
                    device_ids.clear();
                    publish(&connections, &device_ids);
                }
            }
            InputCmd::SetObserver(sender) => {
                ui_observer = Some(sender);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_clone() {
        let pm = Arc::new(MidiPortManager::new(256));
        let io = MidiIo::new(pm);
        let io2 = io.clone();

        assert!(!io.is_any_input_connected());
        assert!(!io2.is_output_connected());
        assert!(io.connected_input_names().is_empty());
        assert!(io2.output_device_name().is_none());
    }

    #[test]
    fn test_list_devices() {
        let pm = Arc::new(MidiPortManager::new(256));
        let io = MidiIo::new(pm);

        // Just verify these don't panic
        let _inputs = io.list_input_devices();
        let _outputs = io.list_output_devices();
    }

    #[test]
    fn test_disconnect_unknown_is_noop() {
        let pm = Arc::new(MidiPortManager::new(256));
        let io = MidiIo::new(pm);
        io.disconnect_input("nonexistent");
        // Give the thread a moment to drain.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!io.is_any_input_connected());
    }
}
