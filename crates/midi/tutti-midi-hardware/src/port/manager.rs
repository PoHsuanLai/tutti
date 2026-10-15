//! [`HardwareMidiInputs`] — every open input port, and the per-block drain the
//! audio thread calls.
//!
//! One manager holds N input rings. `cycle_start_read_all_inputs` fans them all
//! in for one block, converting each event's arrival `Instant` into a
//! `frame_offset` within that block. Everything on that path is RT-safe: bounded
//! work, no locks, no allocation, and no free.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tutti_core::{AudioThreadCell, RtPublish, RtVec, SampleRate};

use super::async_port::{HardwareMidiInput, InputProducerHandle};
use tutti_midi_types::ump::MidiEvent;

/// Which direction a port carries.
///
/// Only [`Input`](PortType::Input) is backed by anything here — see
/// [`HardwareMidiInputs::get_port_info`] for what `Output` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    /// A hardware source feeding this engine, backed by a ring.
    Input,
    /// A hardware destination. Not a port-manager concern: outbound MIDI rides
    /// the engine mailbox to a [`MidiOut`](tutti_midi_types::MidiOut) sink, so
    /// no ring exists and every query answers empty.
    Output,
}

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's index/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The port's slot in the manager, and the key every other method on
    /// [`HardwareMidiInputs`] takes. Stable for the life of the port: ports are
    /// never reordered, and a removed port's index is free for the next port
    /// created, so the set stays as large as what is connected rather than
    /// growing with every hot-plug.
    pub index: usize,
    /// The endpoint name the port was created with — for an open connection,
    /// the name the OS reported for that device.
    pub name: String,
    /// Which direction this port carries. Always
    /// [`Input`](PortType::Input) in practice, since only inputs are backed.
    pub port_type: PortType,
    /// Whether the drain reads this port. An inactive port keeps accepting
    /// pushes into its ring; they are simply never taken.
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
/// block and leaves the rest in the port rings. The scratch is sized to it and
/// cannot grow, because growing means `realloc` inside the audio callback. A
/// dense SysEx dump or a multi-port sweep therefore spreads across blocks — one
/// block of added latency, no missed deadline, and no dropped events.
const CYCLE_SCRATCH_CAP: usize = 256;

/// The published port set: one slot per index, `None` where a port was
/// removed and no new one has taken its place yet.
type PortSlots = Vec<Option<Arc<HardwareMidiInput>>>;

/// Audio-thread-only scratch state for the per-cycle fan-in.
///
/// Both fields are touched **only** from the audio callback, one borrow at a
/// time, so each is an [`AudioThreadCell`] and this type derives `Sync` with no
/// hand-written `unsafe impl`.
///
/// One buffer, not two: the timestamp → `frame_offset` conversion needs only
/// the rate, `nframes` and the block start, all known before the first pop, so
/// each event is converted as it leaves its ring and stored once, already
/// timed. The drained events reach the caller through a visitor rather than a
/// borrowed slice, copied out one at a time, so nothing holds a borrow of the
/// scratch while the visitor runs.
struct CycleScratch {
    sample_rate: AudioThreadCell<SampleRate>,
    events: AudioThreadCell<RtVec<(usize, MidiEvent), CYCLE_SCRATCH_CAP>>,
}

impl CycleScratch {
    fn new() -> Self {
        Self {
            sample_rate: AudioThreadCell::new(SampleRate::SR_44K1),
            events: AudioThreadCell::new(RtVec::new()),
        }
    }

    fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        *self.sample_rate.borrow_mut() = sample_rate.into();
    }

    /// Drain `input_ports`' active rings into `events`, converting arrival
    /// timestamps to sample-accurate `frame_offset`s, and return how many were
    /// taken. RT-safe (lock-free, no heap allocation).
    ///
    /// `buffer_start` is when this block began — the instant every event's age
    /// is measured back from. Taken as a parameter rather than read from the
    /// clock here: it is the *only* input to the conversion a caller does not
    /// otherwise control, so passing it is what makes the arithmetic below
    /// checkable at all. See
    /// [`cycle_start_read_all_inputs_at`](HardwareMidiInputs::cycle_start_read_all_inputs_at).
    fn read_inputs(&self, input_ports: &PortSlots, nframes: usize, buffer_start: Instant) -> usize {
        let sample_rate = *self.sample_rate.borrow();
        let nframes_u32 = nframes as u32;
        let frame_offset = |arrival: Instant| {
            let delta = buffer_start.saturating_duration_since(arrival);
            let samples_ago = (delta.as_secs_f64() * sample_rate.get()) as u32;
            let offset = nframes_u32.saturating_sub(samples_ago);
            if offset >= nframes_u32 {
                nframes_u32.saturating_sub(1)
            } else {
                offset
            }
        };

        // Each port gets the headroom left by the ports before it, so the
        // total can never exceed the scratch's capacity and no push is refused.
        let mut events = self.events.borrow_mut();
        events.clear();
        for (port_index, port) in input_ports.iter().enumerate() {
            let headroom = events.remaining();
            if headroom == 0 {
                break;
            }
            let Some(port) = port else { continue };
            if !port.is_active() {
                continue;
            }
            port.drain_limited(headroom, |arrival, mut event| {
                event.frame_offset = frame_offset(arrival);
                let stored = events.push((port_index, event));
                debug_assert!(stored, "the drain is limited to the headroom");
            });
        }
        events.len()
    }
}

/// Every open hardware MIDI input, and the per-block drain over them.
///
/// A backend creates one port per connection with
/// [`create_input_port`](Self::create_input_port) and pushes into the
/// [`InputProducerHandle`] it returns from the driver's own thread; the audio
/// thread calls [`cycle_start_read_all_inputs`] once per block to take what
/// arrived. When the connection closes, [`remove_input_port`] retires the port
/// and frees its slot for the next one, so hot-plug cycles do not grow the set.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session and the
/// audio graph.
///
/// [`cycle_start_read_all_inputs`]: HardwareMidiInputs::cycle_start_read_all_inputs
/// [`remove_input_port`]: HardwareMidiInputs::remove_input_port
pub struct HardwareMidiInputs {
    /// The port set, published to the audio thread's drain. An [`RtPublish`]
    /// rather than an `ArcSwap`: the drain only borrows the set, so a hotplug
    /// that publishes a new one mid-drain retires the old one for the
    /// *control* side to free. (An `ArcSwap` guard held across that store is
    /// upgraded to a strong count, and its drop then freed the old `Vec` on
    /// the audio thread.) The same holds for a removed port: the set that
    /// still holds it is retired, and the port and its ring are freed by the
    /// next publish, on the control side.
    input_ports: RtPublish<PortSlots>,
    /// Serializes the read, change and publish of [`create_input_port`] and
    /// [`remove_input_port`] (`RtPublish` has no compare-and-publish), so two
    /// control threads racing on hotplug cannot each publish a copy missing
    /// the other's change. Control side only: the drain never takes it.
    ///
    /// [`create_input_port`]: Self::create_input_port
    /// [`remove_input_port`]: Self::remove_input_port
    publish_lock: Mutex<()>,
    fifo_size: usize,
    /// Audio-thread-only scratch buffers. Every field here is `Sync` on its
    /// own, so the manager derives `Sync` rather than asserting it by hand.
    scratch: CycleScratch,
}

impl HardwareMidiInputs {
    /// A manager with no ports, whose rings will each hold `fifo_size` events.
    ///
    /// `fifo_size` is per port, in events: a driver pushing past it drops the
    /// overflow (`push` returns `false`) rather than blocking its callback.
    /// [`Default`] uses 2048.
    pub fn new(fifo_size: usize) -> Self {
        Self {
            input_ports: RtPublish::new(Vec::new()),
            publish_lock: Mutex::new(()),
            fifo_size,
            scratch: CycleScratch::new(),
        }
    }

    /// Sets the [`SampleRate`] the arrival-timestamp → `frame_offset` conversion
    /// divides by.
    ///
    /// Call before starting the audio stream. A stale rate does not drop events;
    /// it places them at the wrong offset within the block, which reads as
    /// timing jitter rather than as an error.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.scratch.set_sample_rate(sample_rate);
    }

    /// Runs `change` on a copy of the port set and publishes the copy, as one
    /// step against every other creator and remover.
    fn republish<R>(&self, change: impl FnOnce(&mut PortSlots) -> R) -> R {
        // The lock holds no data: it makes read, change and publish one step.
        // A poisoned lock guards nothing that could be inconsistent, since the
        // published set is replaced whole or not at all.
        let _publisher = self
            .publish_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut slots = (*self.input_ports.read()).clone();
        let out = change(&mut slots);
        self.input_ports.publish(Arc::new(slots));
        out
    }

    /// Creates an input port named `name` and returns the **only** producer
    /// handle for its ring; [`InputProducerHandle::port_index`] is its index.
    ///
    /// The port takes the lowest free index (one a removed port left), or the
    /// next one past the end. It starts **active**. Control-thread only: this
    /// copies the port set and publishes the copy, so it allocates, may free
    /// the set it replaces, and must not be called from the audio thread. Safe
    /// to call from several control threads at once: each creator's port lands,
    /// at its own index.
    ///
    /// There is no other way to reach a port's producer: give the handle to the
    /// one thread that pushes. Dropping it leaves the port drainable but fed
    /// by nothing; [`remove_input_port`](Self::remove_input_port) retires it.
    #[must_use = "the handle is the port's only producer; dropping it leaves the port fed by nothing"]
    pub fn create_input_port(&self, name: impl Into<String>) -> InputProducerHandle {
        let name = name.into();
        let fifo_size = self.fifo_size;
        self.republish(move |slots| {
            let index = slots
                .iter()
                .position(Option::is_none)
                .unwrap_or(slots.len());
            let (port, handle) = HardwareMidiInput::new(name, fifo_size, index);
            let port = Some(Arc::new(port));
            match slots.get_mut(index) {
                Some(slot) => *slot = port,
                None => slots.push(port),
            }
            handle
        })
    }

    /// Retires the port at `port_index`, freeing its index for the next
    /// [`create_input_port`](Self::create_input_port). Returns `false` when no
    /// port is there.
    ///
    /// Events still in its ring are discarded with it. Control-thread only, as
    /// `create_input_port` is. The audio thread may be draining the port at
    /// that moment: the set it borrowed is retired, not freed, so the port and
    /// its ring are freed by a later publish on the control side, never by the
    /// drain. Close the port's driver connection first (which drops its
    /// producer handle) so nothing is still pushing into a ring nobody reads;
    /// a handle that outlives the port is harmless, since it owns its half of
    /// the ring, but whatever it pushes is lost.
    pub fn remove_input_port(&self, port_index: usize) -> bool {
        self.republish(|slots| {
            let removed = slots.get_mut(port_index).and_then(Option::take).is_some();
            // Trailing empty slots go too, so the set shrinks back once the
            // last port is gone rather than keeping its high-water mark.
            while slots.last().is_some_and(Option::is_none) {
                slots.pop();
            }
            removed
        })
    }

    /// The port vec for `port_type`. Only [`PortType::Input`] is backed by an
    /// `HardwareMidiInput` ring — outbound MIDI rides the engine mailbox to a
    /// `MidiOut` sink, not a port ring — so `Output` returns `None` (lists as
    /// empty / no-op).
    fn ports_of(&self, port_type: PortType) -> Option<&RtPublish<PortSlots>> {
        match port_type {
            PortType::Input => Some(&self.input_ports),
            PortType::Output => None,
        }
    }

    /// The live port at `port_index`, if any.
    fn port(&self, port_type: PortType, port_index: usize) -> Option<Arc<HardwareMidiInput>> {
        self.ports_of(port_type)?
            .read()
            .get(port_index)
            .and_then(Option::clone)
    }

    /// A snapshot of one port, or `None` when `port_index` names no port.
    ///
    /// Always `None` for [`PortType::Output`]: outputs are not backed here.
    pub fn get_port_info(&self, port_type: PortType, port_index: usize) -> Option<PortInfo> {
        self.port(port_type, port_index)
            .map(|port| PortInfo::of(&port, port_index, port_type))
    }

    /// Every input port, in index order. Allocates — control thread only.
    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.list_ports(PortType::Input)
    }

    fn list_ports(&self, port_type: PortType) -> Vec<PortInfo> {
        let Some(ports) = self.ports_of(port_type) else {
            return Vec::new();
        };
        ports
            .read()
            .iter()
            .enumerate()
            .filter_map(|(index, port)| Some(PortInfo::of(port.as_ref()?, index, port_type)))
            .collect()
    }

    /// How many slots the published set holds, live or free: the length the
    /// drain walks each block. Bounded by the most ports ever open at once.
    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.input_ports.read().len()
    }

    /// Sets whether the drain reads this port. Returns `false` when
    /// `port_index` names no port (always so for [`PortType::Output`]).
    ///
    /// Deactivating does not drain or clear the ring: a driver keeps pushing and
    /// the events sit there until the port is active again, so a long pause is
    /// followed by a burst rather than by silence.
    pub fn set_port_active(&self, port_type: PortType, port_index: usize, active: bool) -> bool {
        match self.port(port_type, port_index) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Whether the drain currently reads this port. `false` for an unknown
    /// index and for every [`PortType::Output`] query.
    pub fn is_port_active(&self, port_type: PortType, port_index: usize) -> bool {
        self.port(port_type, port_index)
            .is_some_and(|port| port.is_active())
    }

    /// Drains all active input port rings for this block, converting arrival
    /// timestamps to sample-accurate `frame_offset`s, and hand each event to
    /// `visit` as `(port_index, event)`. Returns how many were visited.
    ///
    /// RT-safe (lock-free, no heap allocation, no free). A visitor rather than
    /// a returned slice: the events live in audio-thread-only scratch, and
    /// lending a borrow into it out of a `&self` method is the one shape that
    /// would need an `UnsafeCell`. `visit` is free to touch this manager — no
    /// borrow of the scratch is held while it runs. Audio thread only: one
    /// drain at a time.
    pub fn cycle_start_read_all_inputs(
        &self,
        nframes: usize,
        visit: impl FnMut(usize, MidiEvent),
    ) -> usize {
        self.cycle_start_read_all_inputs_at(nframes, Instant::now(), visit)
    }

    /// [`cycle_start_read_all_inputs`](Self::cycle_start_read_all_inputs) with
    /// the block's start instant supplied rather than read from the clock.
    ///
    /// Every inbound event's `frame_offset` is
    /// `nframes - (buffer_start - arrival) * sample_rate`, clamped into the
    /// block. `buffer_start` is the one term a caller cannot otherwise pin — the
    /// arrival stamp rides in with the event and `nframes` is an argument — so
    /// with it supplied the whole conversion becomes an equation with a single
    /// right answer.
    ///
    /// That is not a testing nicety. The conversion has a clamp at the block
    /// boundary and a saturating subtraction under it, and neither branch was
    /// reachable from a test that let `Instant::now()` decide the age: an
    /// assertion could only be a band wide enough to absorb however long the
    /// scheduler took between pushing an event and draining it, which meant the
    /// clamp's numeric result was asserted nowhere. Passing the instant makes
    /// the boundary a value a test can *choose*.
    ///
    /// A real caller uses [`cycle_start_read_all_inputs`](Self::cycle_start_read_all_inputs),
    /// which supplies `Instant::now()`. Both are RT-safe.
    pub fn cycle_start_read_all_inputs_at(
        &self,
        nframes: usize,
        buffer_start: Instant,
        mut visit: impl FnMut(usize, MidiEvent),
    ) -> usize {
        // One borrow of the port set for the drain, so a hotplug publishing a
        // new set mid-read leaves this one intact; the borrow cannot free it
        // when it drops, whatever was published meanwhile. Events are copied
        // into the scratch by value (`MidiEvent: Copy`), so nothing borrows
        // from the snapshot past it.
        let input_ports = self.input_ports.read();
        let n = self
            .scratch
            .read_inputs(&input_ports, nframes, buffer_start);
        for i in 0..n {
            // Re-borrow per event rather than across the visitor, so `visit`
            // may reach back into this manager.
            let (port_index, event) = self.scratch.events.borrow()[i];
            visit(port_index, event);
        }
        n
    }
}

impl Default for HardwareMidiInputs {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl tutti_midi_types::MidiIn for HardwareMidiInputs {
    /// Drains every connected hardware input for this block into `buffer`.
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

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let input_ports = self.input_ports.read();

        f.debug_struct("HardwareMidiInputs")
            .field(
                "num_input_ports",
                &input_ports.iter().filter(|p| p.is_some()).count(),
            )
            .field("fifo_size", &self.fifo_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// Test helper: collect one cycle's visits into a `Vec`, so the assertions
    /// below can index and count without threading a closure through each.
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

        let input_id = manager.create_input_port("Test Input").port_index();
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
            let mut handle = manager.create_input_port(format!("Input {port}"));
            for _ in 0..256 {
                assert!(handle.push(MidiEvent::noop(), now()));
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

        let id1 = manager.create_input_port("Input 1").port_index();
        let id2 = manager.create_input_port("Input 2").port_index();

        let inputs = manager.list_input_ports();
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().any(|p| p.index == id1));
        assert!(inputs.iter().any(|p| p.index == id2));
    }

    #[test]
    fn test_port_active_state() {
        let manager = HardwareMidiInputs::new(256);

        let port_id = manager.create_input_port("Test").port_index();
        assert!(manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, false);
        assert!(!manager.is_port_active(PortType::Input, port_id));

        manager.set_port_active(PortType::Input, port_id, true);
        assert!(manager.is_port_active(PortType::Input, port_id));
    }

    /// A drained event is tagged with the port it came from.
    ///
    /// Named for the seam rather than "input flow": `async_port` has a test of
    /// the same shape one layer down, and two tests sharing a name make
    /// `-E 'test(name)'` ambiguous. What is unique here is `events[0].0` — the
    /// port attribution the single-port drain cannot assert.
    #[test]
    fn manager_tags_drained_events_with_their_port_index() {
        let manager = HardwareMidiInputs::new(256);

        let mut producer_handle = manager.create_input_port("Input");
        let input_id = producer_handle.port_index();
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

        let mut producer_handle = manager.create_input_port("Input");
        let input_id = producer_handle.port_index();

        let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 0x3C, 0x7F);
        assert!(producer_handle.push(event, now()));

        manager.set_port_active(PortType::Input, input_id, false);

        let events = drain_cycle(&manager, 512);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_multiple_input_ports() {
        let manager = HardwareMidiInputs::new(256);

        let mut handle1 = manager.create_input_port("Input 1");
        let mut handle2 = manager.create_input_port("Input 2");
        let (id1, id2) = (handle1.port_index(), handle2.port_index());

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

    /// Concurrent creators each land their port: none is lost to another's
    /// publish, and the indices handed back are exactly `0..n`, each once.
    ///
    /// Hotplug can create ports from more than one control thread (a backend's
    /// notification thread, a session's rescan). Reading the set, appending
    /// and publishing is not one atomic step, so two creators that interleave
    /// would each publish a copy missing the other's port.
    ///
    /// Mutation: dropping `create_input_port`'s `publish_lock` guard (the
    /// read, append and publish then run unserialized) fails this with fewer
    /// than 8 x 200 ports listed and repeated indices.
    #[test]
    #[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
    fn concurrent_creators_all_land() {
        use std::collections::BTreeSet;
        use std::sync::Barrier;

        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;
        let manager = Arc::new(HardwareMidiInputs::new(4));
        let start = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    (0..PER_THREAD)
                        .map(|i| {
                            (
                                manager.create_input_port(format!("{t}/{i}")).port_index(),
                                t,
                                i,
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let created: Vec<(usize, usize, usize)> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("creator thread"))
            .collect();

        let ports = manager.list_input_ports();
        assert_eq!(
            ports.len(),
            THREADS * PER_THREAD,
            "a creator's port was lost"
        );
        let indices: BTreeSet<usize> = created.iter().map(|&(index, _, _)| index).collect();
        assert_eq!(
            indices,
            (0..THREADS * PER_THREAD).collect(),
            "every creator was handed its own index"
        );
        for (index, t, i) in created {
            assert_eq!(
                ports[index].name,
                format!("{t}/{i}"),
                "the index a creator was handed names its own port"
            );
        }
    }

    /// Push one event that arrived `age` before the block start, and read back
    /// the `frame_offset` the conversion gives it.
    ///
    /// Both instants are chosen here, so the answer is arithmetic rather than a
    /// measurement: `nframes - age * sample_rate`, clamped into the block. That
    /// is the whole reason `cycle_start_read_all_inputs_at` exists — with
    /// `Instant::now()` inside the drain, the age is whatever the scheduler made
    /// it, and the only assertion available is a band.
    fn offset_for(sample_rate: f64, nframes: usize, age: core::time::Duration) -> u32 {
        let manager = HardwareMidiInputs::new(256);
        manager.set_sample_rate(sample_rate);
        let mut handle = manager.create_input_port("Input");

        // A fixed block start, and an arrival a known age before it. Neither is
        // read from the clock during the conversion.
        let block_start = Instant::now();
        assert!(handle.push(
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
            block_start - age,
        ));

        let mut seen = Vec::new();
        manager.cycle_start_read_all_inputs_at(nframes, block_start, |_, e| seen.push(e));
        assert_eq!(seen.len(), 1, "exactly the event pushed");
        seen[0].frame_offset
    }

    /// A known age converts to exactly one frame offset.
    ///
    /// 1 ms at 48 kHz is 48 samples, so an event that old at the start of a
    /// 1024-frame block belongs at 1024 - 48 = 976. Not a band: with the block
    /// start supplied there is no scheduling slack left in the answer, and the
    /// previous version of this test could assert only `offset <= nframes` —
    /// which every possible value of a `u32` clamped into the block satisfies.
    ///
    /// Three rates, because the rate is a *multiplier* on the age: a conversion
    /// that ignored it, or that used a hardcoded 44100, agrees with the truth at
    /// one rate and disagrees at the others.
    #[test]
    fn a_known_age_converts_to_an_exact_frame_offset() {
        use core::time::Duration;

        for (rate, want) in [
            (48_000.0, 1024 - 48),
            (44_100.0, 1024 - 44), // 44.1 samples, truncated
            (96_000.0, 1024 - 96),
        ] {
            let got = offset_for(rate, 1024, Duration::from_millis(1));
            assert_eq!(
                got, want,
                "1 ms before a 1024-frame block at {rate} Hz is frame {want}, got {got}"
            );
        }
    }

    /// An event older than the whole block clamps to frame 0, not to a wrap.
    ///
    /// This is the branch `Instant::now()` inside the drain made unreachable
    /// from a test. `samples_ago` exceeding `nframes` makes the subtraction
    /// saturate at 0; without the saturation it would wrap to ~4 billion and the
    /// clamp below it would then pull it to `nframes - 1`, so an event from the
    /// distant past would be scheduled at the *end* of the block instead of the
    /// start — a stale note landing after everything that came later.
    #[test]
    fn an_event_older_than_the_block_clamps_to_its_start() {
        use core::time::Duration;

        // 100 ms at 48 kHz is 4800 samples, far past a 1024-frame block.
        let got = offset_for(48_000.0, 1024, Duration::from_millis(100));
        assert_eq!(
            got, 0,
            "an event 4800 samples old in a 1024-frame block belongs at its start, got {got}"
        );
    }

    /// A zero-age event lands at the last frame of the block, not past its end.
    ///
    /// The other boundary. `nframes - 0` is `nframes`, which is one past the
    /// last valid index, so the conversion pulls it back to `nframes - 1`. An
    /// off-by-one here writes an event at an index no renderer will read, and it
    /// is invisible to any assertion of the form `offset <= nframes`.
    #[test]
    fn an_event_arriving_at_the_block_start_lands_on_its_last_frame() {
        use core::time::Duration;

        let got = offset_for(48_000.0, 1024, Duration::ZERO);
        assert_eq!(
            got, 1023,
            "a zero-age event must sit at the last frame of a 1024-frame block, not at 1024"
        );
    }

    /// The rate the manager is *told* is the rate it converts against.
    ///
    /// `HardwareMidiInputs` is constructed before any device exists, so it
    /// starts at a placeholder 44100 and depends on `set_sample_rate` being
    /// called once the device is open. This pins both halves: the placeholder's
    /// exact wrong answer, and the corrected one.
    ///
    /// Naming the wrong value matters. Without it, someone changing
    /// `CycleScratch::new`'s default would silently move what the corrected test
    /// is being compared against rather than failing anything.
    #[test]
    fn the_default_rate_mistimes_an_event_the_set_rate_times_correctly() {
        use core::time::Duration;

        let manager = HardwareMidiInputs::new(256);
        let mut handle = manager.create_input_port("Input");
        let block_start = Instant::now();
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);

        // Untouched: the 44100 placeholder. 1 ms is 44 samples.
        assert!(handle.push(note, block_start - Duration::from_millis(1)));
        let mut seen = Vec::new();
        manager.cycle_start_read_all_inputs_at(1024, block_start, |_, e| seen.push(e));
        assert_eq!(
            seen[0].frame_offset, 980,
            "the placeholder rate puts a 1 ms-old event at 1024 - 44"
        );

        // Told the truth: 1 ms is 48 samples, and the event moves 4 frames.
        manager.set_sample_rate(48_000.0);
        assert!(handle.push(note, block_start - Duration::from_millis(1)));
        let mut seen = Vec::new();
        manager.cycle_start_read_all_inputs_at(1024, block_start, |_, e| seen.push(e));
        assert_eq!(
            seen[0].frame_offset, 976,
            "at 48 kHz the same event belongs 4 frames earlier -- the 8.8% the \
             placeholder was off by"
        );
    }

    /// Hot-plug cycles reuse the slot a removed port left, so the set the
    /// drain walks stays as large as what is connected.
    ///
    /// An anchor port at index 1 stays plugged while a port below it (index 0)
    /// and one above it (index 2) are each cycled 100 times, so both shapes of
    /// reuse run: an interior slot, and the trailing one (trimmed, then
    /// re-appended).
    ///
    /// Mutations: `remove_input_port` only deactivating the port → the list
    /// grows by one per cycle and fails the length check. `create_input_port`
    /// always appending → the interior cycle lands at 2, 3, 4 … and fails the
    /// index check and the slot bound.
    #[test]
    fn hot_plug_cycles_reuse_freed_slots() {
        let manager = HardwareMidiInputs::new(16);
        let first = manager.create_input_port("Interior");
        let _anchor = manager.create_input_port("Anchor");
        assert_eq!((first.port_index(), _anchor.port_index()), (0, 1));
        drop(first);

        for cycle in 0..100 {
            // An interior slot: index 0 is below the live anchor at 1.
            assert!(
                manager.remove_input_port(0),
                "cycle {cycle}: port 0 is there"
            );
            let interior = manager.create_input_port("Interior");
            assert_eq!(
                interior.port_index(),
                0,
                "cycle {cycle}: the freed slot is reused"
            );

            // A trailing slot: index 2 is past every live port.
            let trailing = manager.create_input_port("Trailing");
            assert_eq!(trailing.port_index(), 2, "cycle {cycle}");
            assert!(manager.remove_input_port(2));

            assert_eq!(manager.list_input_ports().len(), 2, "cycle {cycle}");
            assert_eq!(
                manager.slot_count(),
                2,
                "cycle {cycle}: the set does not grow"
            );
        }
        assert!(!manager.remove_input_port(2), "a removed port is gone");
        assert!(manager.get_port_info(PortType::Input, 2).is_none());
    }

    /// A removed port, and the ring its producer no longer holds, are freed by
    /// the control side's publish: nothing is left for the audio thread.
    ///
    /// Mutation: `remove_input_port` deactivating the port instead of taking
    /// it out of the set → the published set keeps it alive and the weak
    /// reference still upgrades.
    #[test]
    fn a_removed_port_is_freed_by_the_publish_that_removes_it() {
        let manager = HardwareMidiInputs::new(16);
        let mut handle = manager.create_input_port("Gone");
        assert!(handle.push(MidiEvent::noop(), now()));
        let port = {
            let slots = manager.input_ports.read();
            Arc::downgrade(slots[0].as_ref().expect("the port is live"))
        };
        drop(handle);

        assert!(manager.remove_input_port(0));
        assert!(
            port.upgrade().is_none(),
            "no reader held the set, so the publish freed the port"
        );
        assert_eq!(drain_cycle(&manager, 512).len(), 0);
    }

    /// A producer that outlives its port, and the whole manager, still pushes
    /// into memory it owns.
    ///
    /// This is the shape that was a use-after-free: the handle was a raw
    /// pointer into a ring the manager owned, so a driver thread pushing while
    /// its session tore down wrote into freed memory. Now the handle owns its
    /// half of the ring, so the ring lives exactly as long as someone holds it.
    /// There is no mutation to record: the old shape cannot be rebuilt without
    /// `unsafe`, and without it the compiler refuses a handle that borrows the
    /// manager. Run it under miri to check the claim directly.
    #[test]
    fn a_producer_outliving_its_manager_pushes_into_its_own_ring() {
        let manager = HardwareMidiInputs::new(4);
        let mut handle = manager.create_input_port("Orphan");
        assert!(manager.remove_input_port(handle.port_index()));
        drop(manager);

        for i in 0..4 {
            assert!(
                handle.push(MidiEvent::noop(), now()),
                "slot {i} of 4 is free"
            );
        }
        assert!(
            !handle.push(MidiEvent::noop(), now()),
            "nothing drains it, so it fills at its capacity"
        );
    }
}
