use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use tutti_core::{AudioThreadCell, RtScratchBuf};

use super::async_port::HardwareMidiInput;
use tutti_midi_types::ump::MidiEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Input,
    Output,
}

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's index/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub index: usize,
    pub name: String,
    pub port_type: PortType,
    pub active: bool,
}

impl PortInfo {
    fn of(port: &HardwareMidiInput, index: usize, port_type: PortType) -> Self {
        Self {
            index,
            name: port.name().to_string(),
            port_type,
            active: port.is_active(),
        }
    }
}

/// Worst-case events drained from all ports in one audio block. Sized to the
/// previous `Vec::with_capacity(256)`; overflow beyond this spills (SmallVec),
/// which is acceptable off the hot path and vanishingly rare in practice.
const CYCLE_SCRATCH_CAP: usize = 256;

/// Audio-thread-only scratch state for the per-cycle fan-in/fan-out.
///
/// Every field is touched **only** from the audio callback, one borrow at a
/// time. Isolating them here keeps that single-thread reasoning contained to
/// one small type rather than spread across the whole [`HardwareMidiInputs`] — and
/// because every field is a `Sync` primitive ([`AudioThreadCell`] /
/// [`RtScratchBuf`]), this type *derives* `Sync` with no hand-written
/// `unsafe impl`.
///
/// `sample_rate` and `timestamped_buffer` use [`AudioThreadCell`] (scoped
/// guards, never lent out). `event_buffer` uses [`RtScratchBuf`] precisely
/// because its filled slice is returned out of the `cycle_*` methods with
/// `&self` lifetime (the manager's `MidiIn::poll_into` copies from it) — the
/// "lend a borrow back to the caller" shape `AudioThreadCell` can't give.
struct CycleScratch {
    sample_rate: AudioThreadCell<f64>,
    timestamped_buffer: AudioThreadCell<Vec<(Instant, usize, MidiEvent)>>,
    event_buffer: RtScratchBuf<(usize, MidiEvent), CYCLE_SCRATCH_CAP>,
}

impl CycleScratch {
    fn new() -> Self {
        Self {
            sample_rate: AudioThreadCell::new(44100.0),
            timestamped_buffer: AudioThreadCell::new(Vec::with_capacity(CYCLE_SCRATCH_CAP)),
            event_buffer: RtScratchBuf::new(),
        }
    }

    fn set_sample_rate(&self, sample_rate: f64) {
        *self.sample_rate.borrow_mut() = sample_rate;
    }

    /// Drain `input_ports`' active rings, converting arrival timestamps to
    /// sample-accurate `frame_offset`s, and return a flat slice borrowing the
    /// internal scratch buffer. RT-safe (lock-free, no heap allocation).
    fn read_inputs(
        &self,
        input_ports: &[Arc<HardwareMidiInput>],
        nframes: usize,
    ) -> &[(usize, MidiEvent)] {
        let buffer_start = Instant::now();
        let sample_rate = *self.sample_rate.borrow();

        // Drain all active input ports into the timestamp scratch, dropping that
        // guard before the fill closure borrows `event_buffer`.
        let mut timestamped = self.timestamped_buffer.borrow_mut();
        timestamped.clear();
        for (port_index, port) in input_ports.iter().enumerate() {
            if !port.is_active() {
                continue;
            }
            port.cycle_start_read_input_into(&mut *timestamped, port_index);
        }
        let timestamped_snapshot = timestamped;

        // SAFETY: single-audio-thread access — `read_inputs` is only reached
        // from the audio callback (`MidiIn::poll_into`).
        unsafe {
            self.event_buffer.fill_and_read(|out| {
                for &(midi_instant, port_index, mut event) in timestamped_snapshot.iter() {
                    let delta = buffer_start.saturating_duration_since(midi_instant);
                    let samples_ago = (delta.as_secs_f64() * sample_rate) as u32;
                    let nframes_u32 = nframes as u32;
                    event.frame_offset = nframes_u32.saturating_sub(samples_ago);
                    if event.frame_offset >= nframes_u32 {
                        event.frame_offset = nframes_u32.saturating_sub(1);
                    }
                    out.push((port_index, event));
                }
            })
        }
    }

}

pub struct HardwareMidiInputs {
    input_ports: ArcSwap<Vec<Arc<HardwareMidiInput>>>,
    fifo_size: usize,
    /// Audio-thread-only scratch buffers. All the manager's `unsafe` lives in
    /// `CycleScratch`; everything else here is `Sync` on its own, so the
    /// manager derives `Sync` rather than asserting it by hand.
    scratch: CycleScratch,
}

impl HardwareMidiInputs {
    pub fn new(fifo_size: usize) -> Self {
        Self {
            input_ports: ArcSwap::from_pointee(Vec::new()),
            fifo_size,
            scratch: CycleScratch::new(),
        }
    }

    /// Set sample rate. Call before starting the audio stream.
    pub fn set_sample_rate(&self, sample_rate: f64) {
        self.scratch.set_sample_rate(sample_rate);
    }

    /// Append `port` to `ports` (clone-and-swap) and return its index. The
    /// port owns its own name and active flag — there is no parallel metadata
    /// to keep in sync.
    fn push_port(ports: &ArcSwap<Vec<Arc<HardwareMidiInput>>>, port: Arc<HardwareMidiInput>) -> usize {
        let mut new_ports = (**ports.load()).clone();
        let port_index = new_ports.len();
        new_ports.push(port);
        ports.store(Arc::new(new_ports));
        port_index
    }

    pub fn create_input_port(&self, name: impl Into<String>) -> usize {
        let port = Arc::new(HardwareMidiInput::new(name.into(), self.fifo_size));
        Self::push_port(&self.input_ports, port)
    }

    /// The port vec for `port_type`. Only [`PortType::Input`] is backed by an
    /// `HardwareMidiInput` ring — outbound MIDI rides the engine mailbox →
    /// [`OutputThread`](crate) sink, not a port ring — so `Output` returns
    /// `None` (lists as empty / no-op).
    fn ports_of(&self, port_type: PortType) -> Option<&ArcSwap<Vec<Arc<HardwareMidiInput>>>> {
        match port_type {
            PortType::Input => Some(&self.input_ports),
            PortType::Output => None,
        }
    }

    pub fn get_port_info(&self, port_type: PortType, port_index: usize) -> Option<PortInfo> {
        self.ports_of(port_type)?
            .load()
            .get(port_index)
            .map(|port| PortInfo::of(port, port_index, port_type))
    }

    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.list_ports(PortType::Input)
    }

    fn list_ports(&self, port_type: PortType) -> Vec<PortInfo> {
        let Some(ports) = self.ports_of(port_type) else {
            return Vec::new();
        };
        ports
            .load()
            .iter()
            .enumerate()
            .map(|(index, port)| PortInfo::of(port, index, port_type))
            .collect()
    }

    pub fn set_port_active(&self, port_type: PortType, port_index: usize, active: bool) -> bool {
        match self.ports_of(port_type).and_then(|p| p.load().get(port_index).cloned()) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    pub fn is_port_active(&self, port_type: PortType, port_index: usize) -> bool {
        self.ports_of(port_type)
            .and_then(|p| p.load().get(port_index).map(|port| port.is_active()))
            .unwrap_or(false)
    }

    /// RT-safe (lock-free, no heap allocation).
    ///
    /// Drains all active input port ring buffers, converts timestamps to
    /// sample-accurate frame_offsets, and returns a flat event slice.
    pub fn cycle_start_read_all_inputs(&self, nframes: usize) -> &[(usize, MidiEvent)] {
        // Hold the ArcSwap guard across the drain so the snapshot can't be
        // swapped out mid-read; the scratch borrows from it.
        let input_ports = self.input_ports.load();
        self.scratch.read_inputs(&input_ports, nframes)
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

impl Default for HardwareMidiInputs {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl tutti_midi_types::MidiIn for HardwareMidiInputs {
    /// Drain all connected hardware inputs for this block into `buffer`. The
    /// hardware is pre-routing — it isn't addressed to one unit, so `unit_id` is
    /// ignored and every pending event is returned; the caller (the
    /// `MidiProcessor`) routes them. `block_size` drives the timestamp →
    /// `frame_offset` conversion (it is the block's `nframes`). RT-safe: the
    /// events already sit in the manager's internal scratch, so this is a bounded
    /// copy with no allocation.
    fn poll_into(
        &self,
        _unit_id: tutti_midi_types::MidiUnitId,
        _block_start_sample: u64,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize {
        let events = self.cycle_start_read_all_inputs(block_size);
        let n = events.len().min(buffer.len());
        for (slot, &(_port, event)) in buffer.iter_mut().zip(events.iter()).take(n) {
            *slot = event;
        }
        n
    }
}

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let input_ports = self.input_ports.load();

        f.debug_struct("HardwareMidiInputs")
            .field("num_input_ports", &input_ports.len())
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
        let manager = HardwareMidiInputs::new(256);

        let input_id = manager.create_input_port("Test Input");
        assert_eq!(input_id, 0);

        let input_ports = manager.list_input_ports();
        assert_eq!(input_ports.len(), 1);
        let input_info = &input_ports[0];
        assert_eq!(input_info.name, "Test Input");
        assert_eq!(input_info.port_type, PortType::Input);
        assert!(input_info.active);

        // Output is not a port-manager concern: it has no backing store, so it
        // always lists empty (outbound MIDI rides the mailbox → OutputThread).
        assert!(manager.get_port_info(PortType::Output, 0).is_none());
    }

    #[test]
    fn test_list_ports() {
        let manager = HardwareMidiInputs::new(256);

        let id1 = manager.create_input_port("Input 1");
        let id2 = manager.create_input_port("Input 2");

        let inputs = manager.list_input_ports();
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().any(|p| p.index == id1));
        assert!(inputs.iter().any(|p| p.index == id2));
    }

    #[test]
    fn test_port_active_state() {
        let manager = HardwareMidiInputs::new(256);

        let port_id = manager.create_input_port("Test");
        assert!(manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, false);
        assert!(!manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, true);
        assert!(manager.is_port_active(PortType::Input, port_id));
    }

    #[test]
    fn test_input_flow() {
        let manager = HardwareMidiInputs::new(256);

        let input_id = manager.create_input_port("Input");

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
    }

    #[test]
    fn test_inactive_ports_ignored() {
        let manager = HardwareMidiInputs::new(256);

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
        let manager = HardwareMidiInputs::new(256);

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
        let manager = HardwareMidiInputs::new(256);
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
