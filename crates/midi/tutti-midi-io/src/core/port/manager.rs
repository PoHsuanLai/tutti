use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use tutti_core::{AudioThreadCell, RtEventBuf, SampleRate};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Events carried from all ports through one audio block.
///
/// This is a hard cap, not a hint: the drain takes at most this many events per
/// block and leaves the rest in the port rings. Both scratch buffers are sized
/// to it and neither is allowed to grow, because growing means `realloc` inside
/// the audio callback. A dense SysEx dump or a multi-port sweep therefore
/// spreads across blocks — one block of added latency, no missed deadline, and
/// no dropped events.
const CYCLE_SCRATCH_CAP: usize = 256;

/// Audio-thread-only scratch state for the per-cycle fan-in/fan-out.
///
/// Every field is touched **only** from the audio callback, one borrow at a
/// time. Isolating them here keeps that single-thread reasoning contained to
/// one small type rather than spread across the whole [`HardwareMidiInputs`] — and
/// because every field is a `Sync` primitive ([`AudioThreadCell`] /
/// [`RtEventBuf`]), this type *derives* `Sync` with no hand-written
/// `unsafe impl`.
///
/// All three are safe wrappers: this module contains no `unsafe`. The drained
/// events reach the caller through a visitor rather than a borrowed slice, so
/// nothing holds a reference into the scratch past the call — which is what
/// keeps `event_buffer` a plain [`RtEventBuf`] and the `(port_index, event)`
/// pairing intact.
struct CycleScratch {
    sample_rate: AudioThreadCell<SampleRate>,
    timestamped_buffer: AudioThreadCell<Vec<(Instant, usize, MidiEvent)>>,
    event_buffer: RtEventBuf<(usize, MidiEvent), CYCLE_SCRATCH_CAP>,
}

impl CycleScratch {
    fn new() -> Self {
        Self {
            sample_rate: AudioThreadCell::new(SampleRate::SR_44K1),
            timestamped_buffer: AudioThreadCell::new(Vec::with_capacity(CYCLE_SCRATCH_CAP)),
            event_buffer: RtEventBuf::new(),
        }
    }

    fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        *self.sample_rate.borrow_mut() = sample_rate.into();
    }

    /// Drain `input_ports`' active rings into `event_buffer`, converting arrival
    /// timestamps to sample-accurate `frame_offset`s. RT-safe (lock-free, no
    /// heap allocation). Read the result with `event_buffer.drain_each`.
    fn read_inputs(&self, input_ports: &[Arc<HardwareMidiInput>], nframes: usize) {
        let buffer_start = Instant::now();
        let sample_rate = *self.sample_rate.borrow();

        // Drain all active input ports into the timestamp scratch. Each port
        // gets the headroom left by the ports before it, so the total can never
        // exceed the buffer's reserved capacity.
        let mut timestamped = self.timestamped_buffer.borrow_mut();
        timestamped.clear();
        let mut headroom = CYCLE_SCRATCH_CAP;
        for (port_index, port) in input_ports.iter().enumerate() {
            if headroom == 0 {
                break;
            }
            if !port.is_active() {
                continue;
            }
            headroom -= port.cycle_start_read_input_into(&mut *timestamped, port_index, headroom);
        }
        let timestamped_snapshot = timestamped;

        self.event_buffer.clear();
        for &(midi_instant, port_index, mut event) in timestamped_snapshot.iter() {
            let delta = buffer_start.saturating_duration_since(midi_instant);
            let samples_ago = (delta.as_secs_f64() * sample_rate.get()) as u32;
            let nframes_u32 = nframes as u32;
            event.frame_offset = nframes_u32.saturating_sub(samples_ago);
            if event.frame_offset >= nframes_u32 {
                event.frame_offset = nframes_u32.saturating_sub(1);
            }
            if !self.event_buffer.push((port_index, event)) {
                break;
            }
        }
    }
}

pub struct HardwareMidiInputs {
    input_ports: ArcSwap<Vec<Arc<HardwareMidiInput>>>,
    fifo_size: usize,
    /// Audio-thread-only scratch buffers. Every field here is `Sync` on its
    /// own, so the manager derives `Sync` rather than asserting it by hand.
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
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.scratch.set_sample_rate(sample_rate);
    }

    /// Append `port` to `ports` (clone-and-swap) and return its index. The
    /// port owns its own name and active flag — there is no parallel metadata
    /// to keep in sync.
    fn push_port(
        ports: &ArcSwap<Vec<Arc<HardwareMidiInput>>>,
        port: Arc<HardwareMidiInput>,
    ) -> usize {
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
        match self
            .ports_of(port_type)
            .and_then(|p| p.load().get(port_index).cloned())
        {
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

    /// Drain all active input port rings for this block, converting arrival
    /// timestamps to sample-accurate `frame_offset`s, and hand each event to
    /// `visit` as `(port_index, event)`. Returns how many were visited.
    ///
    /// RT-safe (lock-free, no heap allocation). A visitor rather than a
    /// returned slice: the events live in audio-thread-only scratch, and
    /// lending a borrow into it out of a `&self` method is the one shape that
    /// would need an `UnsafeCell`. `visit` is free to touch this manager —
    /// `drain_each` releases its borrow around each call.
    pub fn cycle_start_read_all_inputs(
        &self,
        nframes: usize,
        mut visit: impl FnMut(usize, MidiEvent),
    ) -> usize {
        // Hold the ArcSwap guard across the drain so the port set can't be
        // swapped out mid-read. Events are copied into the scratch by value
        // (`MidiEvent: Copy`), so nothing borrows from the snapshot.
        let input_ports = self.input_ports.load();
        self.scratch.read_inputs(&input_ports, nframes);
        let mut n = 0;
        self.scratch.event_buffer.drain_each(|(port_index, event)| {
            visit(port_index, event);
            n += 1;
        });
        n
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

impl tutti_midi_types::MidiSource for HardwareMidiInputs {
    /// Drain every connected hardware input for this block into `buffer`.
    ///
    /// `block_size` drives the timestamp → `frame_offset` conversion (it is the
    /// block's `nframes`). RT-safe: the events already sit in the manager's
    /// internal scratch, so this is a bounded copy with no allocation.
    ///
    /// A full `buffer` drops the overflow — the drain is destructive at the
    /// ring, so by the time there is no room the events are already out of it.
    fn poll_block(&self, block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        let mut written = 0usize;
        self.cycle_start_read_all_inputs(block_size, |_port, event| {
            if written < buffer.len() {
                buffer[written] = event;
                written += 1;
            }
        });
        written
    }
}

impl tutti_midi_types::MidiIn for HardwareMidiInputs {
    /// Drain all connected hardware inputs for this block into `buffer`. The
    /// hardware is pre-routing — it isn't addressed to one unit, so `unit_id` is
    /// ignored and every pending event is returned; the caller (the
    /// `MidiPreBlock`) routes them. `block_size` drives the timestamp →
    /// `frame_offset` conversion (it is the block's `nframes`). RT-safe: the
    /// events already sit in the manager's internal scratch, so this is a bounded
    /// copy with no allocation.
    fn poll_into(
        &self,
        _unit_id: tutti_midi_types::MidiUnitId,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize {
        let mut written = 0usize;
        self.cycle_start_read_all_inputs(block_size, |_port, event| {
            if written < buffer.len() {
                buffer[written] = event;
                written += 1;
            }
        });
        written
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
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    /// Test helper: drain a cycle into a `Vec`, reproducing the shape the
    /// public API used to return so these assertions stay unchanged.
    fn drain_cycle(manager: &HardwareMidiInputs, nframes: usize) -> Vec<(usize, MidiEvent)> {
        let mut out = Vec::new();
        manager.cycle_start_read_all_inputs(nframes, |port, event| out.push((port, event)));
        out
    }

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

    /// A multi-port sweep must not push either scratch buffer past its reserved
    /// capacity — that push is a `realloc` inside the audio callback.
    ///
    /// Four ports of 256 events each offer 1024, four times the cap.
    #[test]
    fn cycle_read_is_capped_across_ports() {
        let manager = HardwareMidiInputs::new(256);
        for port in 0..4 {
            let index = manager.create_input_port(format!("Input {port}"));
            for _ in 0..256 {
                assert!(manager.push_input_event(index, MidiEvent::noop()));
            }
        }

        let events = drain_cycle(&manager, 512);
        assert_eq!(
            events.len(),
            CYCLE_SCRATCH_CAP,
            "one block drains at most the cap, however many ports are flooded"
        );

        // The remainder is deferred, not dropped: successive blocks drain it.
        let total: usize = (0..3).map(|_| drain_cycle(&manager, 512).len()).sum();
        assert_eq!(
            total + CYCLE_SCRATCH_CAP,
            1024,
            "every queued event is eventually delivered"
        );
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
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            0x3C,
            tutti_midi_types::convert::midi1_velocity_to_midi2(0x7F),
        );
        assert!(producer_handle.push(event, now()));

        let events = drain_cycle(&manager, 512);
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

        let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 0x3C, 0x7F);
        assert!(producer_handle.push(event, now()));

        manager.set_port_active(PortType::Input, input_id, false);

        let events = drain_cycle(&manager, 512);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_multiple_input_ports() {
        let manager = HardwareMidiInputs::new(256);

        let id1 = manager.create_input_port("Input 1");
        let id2 = manager.create_input_port("Input 2");

        let handle1 = manager.get_input_producer_handle(id1).unwrap();
        let handle2 = manager.get_input_producer_handle(id2).unwrap();

        handle1.push(
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
            now(),
        );
        handle2.push(
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 100),
            now(),
        );

        let events = drain_cycle(&manager, 512);
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

        handle.push(
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
            Instant::now(),
        );
        let events = drain_cycle(&manager, nframes);
        assert_eq!(events.len(), 1);
        assert!(
            (events[0].1.frame_offset as usize) <= nframes,
            "frame_offset should be within buffer, got {}",
            events[0].1.frame_offset,
        );
    }
}
