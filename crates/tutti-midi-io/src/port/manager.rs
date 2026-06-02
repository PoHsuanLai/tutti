use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use parking_lot::RwLock;
use tutti_core::AudioThreadCell;

use super::async_port::{AsyncMidiPort, OutputProducerHandle};
use tutti_midi_types::ump::MidiEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Input,
    Output,
}

#[derive(Debug, Clone)]
pub struct PortInfo {
    pub index: usize,
    pub name: String,
    pub port_type: PortType,
    pub active: Arc<AtomicBool>,
}

pub struct MidiPortManager {
    input_ports: Arc<ArcSwap<Vec<Arc<AsyncMidiPort>>>>,
    output_ports: Arc<ArcSwap<Vec<Arc<AsyncMidiPort>>>>,
    output_handles: Arc<ArcSwap<Vec<OutputProducerHandle>>>,
    port_info: Arc<RwLock<Vec<PortInfo>>>,
    fifo_size: usize,
    // Scalar and the internal scratch buffer use AudioThreadCell: they never
    // hand a reference back out, so the borrow-guard lifetimes are contained
    // within a single method, and debug builds get the concurrent-borrow check.
    sample_rate: AudioThreadCell<f64>,
    timestamped_buffer: AudioThreadCell<Vec<(Instant, usize, MidiEvent)>>,
    // These two stay as raw UnsafeCell because their `.as_slice()` is returned
    // out of the cycle_*_all_* methods with `&self` lifetime (see
    // `MidiInputSource::cycle_read`), which AudioThreadCell's scoped guards
    // intentionally do not allow. The single-audio-thread invariant is what
    // keeps the raw access sound.
    event_buffer: UnsafeCell<Vec<(usize, MidiEvent)>>,
    output_event_buffer: UnsafeCell<Vec<(usize, MidiEvent)>>,
}

// SAFETY: MidiPortManager is Sync because:
// 1. sample_rate / timestamped_buffer are AudioThreadCell, which is Sync.
// 2. event_buffer / output_event_buffer (raw UnsafeCell) are only accessed
//    from the audio callback (single-threaded), one borrow at a time.
// 3. All other fields (ArcSwap, RwLock, primitives) are already Sync.
unsafe impl Sync for MidiPortManager {}

impl MidiPortManager {
    pub fn new(fifo_size: usize) -> Self {
        Self {
            input_ports: Arc::new(ArcSwap::from_pointee(Vec::new())),
            output_ports: Arc::new(ArcSwap::from_pointee(Vec::new())),
            output_handles: Arc::new(ArcSwap::from_pointee(Vec::new())),
            port_info: Arc::new(RwLock::new(Vec::new())),
            fifo_size,
            sample_rate: AudioThreadCell::new(44100.0),
            timestamped_buffer: AudioThreadCell::new(Vec::with_capacity(256)),
            event_buffer: UnsafeCell::new(Vec::with_capacity(256)),
            output_event_buffer: UnsafeCell::new(Vec::with_capacity(256)),
        }
    }

    /// Set sample rate. Call before starting the audio stream.
    pub fn set_sample_rate(&self, sample_rate: f64) {
        *self.sample_rate.borrow_mut() = sample_rate;
    }

    pub fn create_input_port(&self, name: impl Into<String>) -> usize {
        let name = name.into();
        let port = Arc::new(AsyncMidiPort::new(&name, self.fifo_size));
        let current_ports = self.input_ports.load();
        let mut new_ports = (**current_ports).clone();
        let port_index = new_ports.len();
        new_ports.push(port);
        self.input_ports.store(Arc::new(new_ports));

        let mut port_info = self.port_info.write();
        let info = PortInfo {
            index: port_index,
            name: name.clone(),
            port_type: PortType::Input,
            active: Arc::new(AtomicBool::new(true)),
        };
        port_info.push(info);

        port_index
    }

    pub fn create_output_port(&self, name: impl Into<String>) -> usize {
        let name = name.into();
        let port = Arc::new(AsyncMidiPort::new(&name, self.fifo_size));

        let output_handle = port.output_producer_handle();

        let current_ports = self.output_ports.load();
        let mut new_ports = (**current_ports).clone();
        let port_index = new_ports.len();
        new_ports.push(port);
        self.output_ports.store(Arc::new(new_ports));

        let current_handles = self.output_handles.load();
        let mut new_handles = (**current_handles).clone();
        new_handles.push(output_handle);
        self.output_handles.store(Arc::new(new_handles));

        let mut port_info = self.port_info.write();
        let info = PortInfo {
            index: port_index,
            name: name.clone(),
            port_type: PortType::Output,
            active: Arc::new(AtomicBool::new(true)),
        };
        port_info.push(info);

        port_index
    }

    pub fn get_port_info(&self, port_type: PortType, port_index: usize) -> Option<PortInfo> {
        let port_info = self.port_info.read();
        port_info
            .iter()
            .find(|info| info.port_type == port_type && info.index == port_index)
            .cloned()
    }

    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        let port_info = self.port_info.read();
        port_info
            .iter()
            .filter(|info| info.port_type == PortType::Input)
            .cloned()
            .collect()
    }

    pub fn list_output_ports(&self) -> Vec<PortInfo> {
        let port_info = self.port_info.read();
        port_info
            .iter()
            .filter(|info| info.port_type == PortType::Output)
            .cloned()
            .collect()
    }

    pub fn set_port_active(&self, port_type: PortType, port_index: usize, active: bool) -> bool {
        let port_info = self.port_info.read();
        if let Some(info) = port_info
            .iter()
            .find(|info| info.port_type == port_type && info.index == port_index)
        {
            info.active.store(active, Ordering::Release);
            match info.port_type {
                PortType::Input => {
                    let input_ports = self.input_ports.load();
                    if let Some(port) = input_ports.get(info.index) {
                        port.set_active(active);
                    }
                }
                PortType::Output => {
                    let output_ports = self.output_ports.load();
                    if let Some(port) = output_ports.get(info.index) {
                        port.set_active(active);
                    }
                }
            }
            true
        } else {
            false
        }
    }

    /// NOT RT-safe (acquires lock).
    pub fn is_port_active(&self, port_type: PortType, port_index: usize) -> bool {
        let port_info = self.port_info.read();
        port_info
            .iter()
            .find(|info| info.port_type == port_type && info.index == port_index)
            .is_some_and(|info| info.active.load(Ordering::Acquire))
    }

    pub fn output_port_count(&self) -> usize {
        self.output_ports.load().len()
    }

    pub fn output_ports(&self) -> arc_swap::Guard<Arc<Vec<Arc<AsyncMidiPort>>>> {
        self.output_ports.load()
    }

    /// RT-safe (lock-free).
    ///
    /// # Safety
    /// Must only be called from a single thread (the audio thread).
    pub fn write_output_event(&self, port_index: usize, event: MidiEvent) -> bool {
        let output_handles = self.output_handles.load();
        if let Some(handle) = output_handles.get(port_index) {
            handle.push(event)
        } else {
            false
        }
    }

    /// RT-safe (lock-free, no heap allocation).
    ///
    /// Drains all active input port ring buffers, converts timestamps to
    /// sample-accurate frame_offsets, and returns a flat event slice.
    pub fn cycle_start_read_all_inputs(&self, nframes: usize) -> &[(usize, MidiEvent)] {
        let buffer_start = Instant::now();
        let sample_rate = *self.sample_rate.borrow();

        // Drain all active input ports into the scratch buffer, then drop the
        // guard before touching `event_buffer` (one borrow at a time).
        {
            let mut timestamped = self.timestamped_buffer.borrow_mut();
            timestamped.clear();
            let input_ports = self.input_ports.load();
            for (port_index, port) in input_ports.iter().enumerate() {
                if !port.is_active() {
                    continue;
                }
                port.cycle_start_read_input_into(&mut timestamped, port_index);
            }

            // Convert timestamps to frame_offsets into the returned buffer.
            // SAFETY: single-audio-thread access; the returned slice borrows
            // `self`, which is why this buffer is a raw UnsafeCell (see the
            // field comment).
            let all_events = unsafe { &mut *self.event_buffer.get() };
            all_events.clear();
            for &(midi_instant, port_index, mut event) in timestamped.iter() {
                let delta = buffer_start.saturating_duration_since(midi_instant);
                let samples_ago = (delta.as_secs_f64() * sample_rate) as u32;
                let nframes_u32 = nframes as u32;
                event.frame_offset = nframes_u32.saturating_sub(samples_ago);
                if event.frame_offset >= nframes_u32 {
                    event.frame_offset = nframes_u32.saturating_sub(1);
                }
                all_events.push((port_index, event));
            }
        }

        // SAFETY: as above; the `timestamped_buffer` guard has been dropped.
        unsafe { (*self.event_buffer.get()).as_slice() }
    }

    /// RT-safe (lock-free, no heap allocation).
    ///
    /// Returns a flat slice of (port_index, event) pairs from all active output ports.
    pub fn cycle_end_flush_all_outputs(&self) -> &[(usize, MidiEvent)] {
        unsafe {
            let all_events = &mut *self.output_event_buffer.get();
            all_events.clear();
            let output_ports = self.output_ports.load();

            for (port_index, port) in output_ports.iter().enumerate() {
                if !port.is_active() {
                    continue;
                }
                port.cycle_end_flush_output_into(all_events, port_index);
            }
            all_events.as_slice()
        }
    }

    /// RT-safe (lock-free).
    ///
    /// # Safety
    /// Must only be called from a single thread (the audio thread).
    pub fn write_event_to_port(&self, port_index: usize, event: MidiEvent) -> bool {
        let output_ports = self.output_ports.load();
        if let Some(port) = output_ports.get(port_index) {
            if !port.is_active() {
                return false;
            }
        } else {
            return false;
        }

        let output_handles = self.output_handles.load();
        if let Some(handle) = output_handles.get(port_index) {
            handle.push(event)
        } else {
            false
        }
    }

    pub fn get_input_producer_handle(
        &self,
        port_index: usize,
    ) -> Option<super::async_port::InputProducerHandle> {
        let input_ports = self.input_ports.load();
        input_ports
            .get(port_index)
            .map(|port| port.input_producer_handle())
    }
    /// RT-safe (lock-free). Uses `Instant::now()` as the timestamp.
    pub fn push_input_event(&self, port_index: usize, event: MidiEvent) -> bool {
        let input_ports = self.input_ports.load();
        if let Some(port) = input_ports.get(port_index) {
            let handle = port.input_producer_handle();
            handle.push(event, Instant::now())
        } else {
            false
        }
    }
}

impl Default for MidiPortManager {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl tutti_midi_types::MidiInputSource for MidiPortManager {
    fn cycle_read(&self, nframes: usize) -> &[(usize, MidiEvent)] {
        self.cycle_start_read_all_inputs(nframes)
    }
}

impl core::fmt::Debug for MidiPortManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let input_ports = self.input_ports.load();
        let output_ports = self.output_ports.load();

        f.debug_struct("MidiPortManager")
            .field("num_input_ports", &input_ports.len())
            .field("num_output_ports", &output_ports.len())
            .field("fifo_size", &self.fifo_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn test_create_ports() {
        let manager = MidiPortManager::new(256);

        let input_id = manager.create_input_port("Test Input");
        let output_id = manager.create_output_port("Test Output");

        assert_eq!(input_id, 0);
        assert_eq!(output_id, 0);

        let input_ports = manager.list_input_ports();
        assert_eq!(input_ports.len(), 1);
        let input_info = &input_ports[0];
        assert_eq!(input_info.name, "Test Input");
        assert_eq!(input_info.port_type, PortType::Input);
        assert!(input_info.active.load(Ordering::Acquire));

        let output_ports = manager.list_output_ports();
        assert_eq!(output_ports.len(), 1);
        let output_info = &output_ports[0];
        assert_eq!(output_info.name, "Test Output");
        assert_eq!(output_info.port_type, PortType::Output);
        assert!(output_info.active.load(Ordering::Acquire));
    }

    #[test]
    fn test_list_ports() {
        let manager = MidiPortManager::new(256);

        let id1 = manager.create_input_port("Input 1");
        let id2 = manager.create_input_port("Input 2");
        let id3 = manager.create_output_port("Output 1");

        let inputs = manager.list_input_ports();
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().any(|p| p.index == id1));
        assert!(inputs.iter().any(|p| p.index == id2));

        let outputs = manager.list_output_ports();
        assert_eq!(outputs.len(), 1);
        assert!(outputs.iter().any(|p| p.index == id3));
    }

    #[test]
    fn test_port_active_state() {
        let manager = MidiPortManager::new(256);

        let port_id = manager.create_input_port("Test");
        assert!(manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, false);
        assert!(!manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, true);
        assert!(manager.is_port_active(PortType::Input, port_id));
    }

    #[test]
    fn test_input_output_flow() {
        let manager = MidiPortManager::new(256);

        let input_id = manager.create_input_port("Input");
        let output_id = manager.create_output_port("Output");

        let producer_handle = manager.get_input_producer_handle(input_id).unwrap();
        // Upconvert 7-bit 127 so the downconverted velocity_u7 round-trips.
        let event = MidiEvent::note_on(
            0,
            0,
            0x3C,
            tutti_midi_types::convert::midi1_velocity_to_midi2(0x7F),
        );
        assert!(producer_handle.push(event, now()));

        let events = manager.cycle_start_read_all_inputs(512);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, input_id);
        assert!(events[0].1.is_note_on());
        assert_eq!(events[0].1.note(), Some(0x3C));
        assert_eq!(events[0].1.velocity_u7(), Some(0x7F));

        let out_event = MidiEvent::note_off(10, 0, 0x3C, 0);
        assert!(manager.write_event_to_port(output_id, out_event));

        let output_events = manager.cycle_end_flush_all_outputs();
        assert_eq!(output_events.len(), 1);
        assert_eq!(output_events[0].0, output_id);
        assert!(output_events[0].1.is_note_off());
        assert_eq!(output_events[0].1.note(), Some(0x3C));
    }

    #[test]
    fn test_inactive_ports_ignored() {
        let manager = MidiPortManager::new(256);

        let input_id = manager.create_input_port("Input");
        let producer_handle = manager.get_input_producer_handle(input_id).unwrap();

        let event = MidiEvent::note_on(0, 0, 0x3C, 0x7F);
        assert!(producer_handle.push(event, now()));

        manager.set_port_active(PortType::Input, input_id, false);

        let events = manager.cycle_start_read_all_inputs(512);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_multiple_input_ports() {
        let manager = MidiPortManager::new(256);

        let id1 = manager.create_input_port("Input 1");
        let id2 = manager.create_input_port("Input 2");

        let handle1 = manager.get_input_producer_handle(id1).unwrap();
        let handle2 = manager.get_input_producer_handle(id2).unwrap();

        handle1.push(MidiEvent::note_on(0, 0, 60, 100), now());
        handle2.push(MidiEvent::note_on(0, 0, 64, 100), now());

        let events = manager.cycle_start_read_all_inputs(512);
        assert_eq!(events.len(), 2);

        let port_ids: Vec<_> = events.iter().map(|(id, _)| *id).collect();
        assert!(port_ids.contains(&id1));
        assert!(port_ids.contains(&id2));
    }

    #[test]
    fn test_timestamp_to_frame_offset_conversion() {
        let manager = MidiPortManager::new(256);
        let input_id = manager.create_input_port("Input");
        let handle = manager.get_input_producer_handle(input_id).unwrap();

        let nframes = 256;

        handle.push(MidiEvent::note_on(0, 0, 60, 100), Instant::now());
        let events = manager.cycle_start_read_all_inputs(nframes);
        assert_eq!(events.len(), 1);
        assert!(
            (events[0].1.frame_offset as usize) <= nframes,
            "frame_offset should be within buffer, got {}",
            events[0].1.frame_offset,
        );
    }
}
