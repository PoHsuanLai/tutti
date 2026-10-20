//! [`HardwareMidiInputs`] — every open input port, and the per-block drain the
//! audio thread calls.
//!
//! One manager holds N input rings. `cycle_start_read_all_inputs` fans them all
//! in for one block, converting each event's arrival `Instant` into a
//! `frame_offset` within that block. Everything on that path is RT-safe: bounded
//! work, no locks, no allocation, and no free.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// One port's identity: its slot in the manager, and which of the ports ever
/// created there it is.
///
/// A removed port's slot is reused by the next port created, so the slot
/// alone does not name a port: an id kept past its port's removal would
/// otherwise reach whichever port took the slot. The generation tells them
/// apart. Every method taking a `PortId` checks both, and a stale id (its port
/// removed, whether or not the slot was reused) matches nothing: it removes
/// nothing, toggles nothing, and reads as no port.
///
/// The drain tags each event with the `PortId` of the port it came from, so
/// a consumer can tell the ports apart within a block and across a reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId {
    index: usize,
    generation: u64,
}

impl PortId {
    /// A port id for this crate's unit tests of a lone port.
    #[cfg(test)]
    pub(crate) const TEST: PortId = PortId {
        index: 0,
        generation: 0,
    };

    /// The slot this port occupies. Unique among the ports open at one time,
    /// and reused after the port is removed; compare `PortId`s, not slots, to
    /// tell ports apart.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }

    /// Which port created in this manager this is: unique for the manager's
    /// life, increasing in creation order.
    #[must_use]
    pub fn generation(self) -> u64 {
        self.generation
    }
}

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's id/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The port's identity, and the key every other method on
    /// [`HardwareMidiInputs`] takes.
    pub id: PortId,
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
    fn of(port: &HardwareMidiInput, port_type: PortType) -> Self {
        Self {
            id: port.id(),
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

/// The drain's per-block state.
///
/// `events` is touched **only** from the audio callback, one borrow at a time,
/// so it is an [`AudioThreadCell`]. `sample_rate` is written by the control
/// side and read by the drain, so it is an atomic (the `f64`'s bits), read
/// once per block. `draining` marks a drain in progress, so one re-entered
/// from its own visitor is refused rather than clearing the events the outer
/// one is still handing out. All three are `Sync` on their own; the type
/// derives it with no hand-written `unsafe impl`.
///
/// One buffer, not two: the timestamp → `frame_offset` conversion needs only
/// the rate, `nframes` and the block start, all known before the first pop, so
/// each event is converted as it leaves its ring and stored once, already
/// timed. The drained events reach the caller through a visitor rather than a
/// borrowed slice, copied out one at a time, so nothing holds a borrow of the
/// scratch while the visitor runs.
struct CycleScratch {
    sample_rate: AtomicU64,
    draining: AtomicBool,
    events: AudioThreadCell<RtVec<(PortId, MidiEvent), CYCLE_SCRATCH_CAP>>,
}

/// Clears [`CycleScratch::draining`] when the drain ends, however it ends: a
/// visitor that panics must not leave every later drain refused.
struct Draining<'a>(&'a AtomicBool);

impl Drop for Draining<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl CycleScratch {
    fn new() -> Self {
        Self {
            sample_rate: AtomicU64::new(SampleRate::SR_44K1.get().to_bits()),
            draining: AtomicBool::new(false),
            events: AudioThreadCell::new(RtVec::new()),
        }
    }

    fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate
            .store(sample_rate.into().get().to_bits(), Ordering::Relaxed);
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from_bits(self.sample_rate.load(Ordering::Relaxed)))
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
        // Once per block: a rate the control side changes mid-drain applies
        // from the next one.
        let sample_rate = self.sample_rate();
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
        for port in input_ports.iter().flatten() {
            let headroom = events.remaining();
            if headroom == 0 {
                break;
            }
            if !port.is_active() {
                continue;
            }
            let id = port.id();
            port.drain_limited(headroom, |arrival, mut event| {
                event.frame_offset = frame_offset(arrival);
                let stored = events.push((id, event));
                debug_assert!(stored, "the drain is limited to the headroom");
            });
        }
        events.len()
    }
}

/// What the control side last published, and the next generation to hand out.
/// Behind [`HardwareMidiInputs::published`]'s lock.
struct Published {
    /// The set the drain currently reads: kept here so a change starts from it
    /// and [`collect`](HardwareMidiInputs::collect) can publish it again.
    slots: Arc<PortSlots>,
    next_generation: u64,
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
    /// still holds it is retired, and the port and its ring are freed on the
    /// control side (see [`collect`](Self::collect)).
    input_ports: RtPublish<PortSlots>,
    /// Serializes the read, change and publish of every control-side change
    /// (`RtPublish` has no compare-and-publish), so two control threads racing
    /// on hotplug cannot each publish a copy missing the other's change. The
    /// drain never takes it.
    published: Mutex<Published>,
    fifo_size: usize,
    /// The drain's state. Every field here is `Sync` on its own, so the
    /// manager derives `Sync` rather than asserting it by hand.
    scratch: CycleScratch,
}

impl HardwareMidiInputs {
    /// A manager with no ports, whose rings will each hold `fifo_size` events.
    ///
    /// `fifo_size` is per port, in events: a driver pushing past it drops the
    /// overflow (`push` returns `false`) rather than blocking its callback.
    /// [`Default`] uses 2048.
    pub fn new(fifo_size: usize) -> Self {
        let slots = Arc::new(PortSlots::new());
        Self {
            input_ports: RtPublish::from_arc(Arc::clone(&slots)),
            published: Mutex::new(Published {
                slots,
                next_generation: 0,
            }),
            fifo_size,
            scratch: CycleScratch::new(),
        }
    }

    /// Sets the [`SampleRate`] the arrival-timestamp → `frame_offset` conversion
    /// divides by.
    ///
    /// Safe to call from any thread at any time: the rate is an atomic the
    /// drain reads once per block, so a change lands on the next block. A stale
    /// rate does not drop events; it places them at the wrong offset within the
    /// block, which reads as timing jitter rather than as an error.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.scratch.set_sample_rate(sample_rate);
    }

    /// Runs `change` on a copy of the port set and publishes the copy, as one
    /// step against every other control-side change.
    fn republish<R>(&self, change: impl FnOnce(&mut PortSlots, &mut u64) -> R) -> R {
        // A poisoned lock guards nothing inconsistent: the published set is
        // replaced whole or not at all, and the generation only ever grows.
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let published = &mut *published;
        let mut slots = (*published.slots).clone();
        let out = change(&mut slots, &mut published.next_generation);
        published.slots = Arc::new(slots);
        self.input_ports.publish(Arc::clone(&published.slots));
        out
    }

    /// Creates an input port named `name` and returns the **only** producer
    /// handle for its ring; [`InputProducerHandle::port_id`] is its identity.
    ///
    /// The port takes the lowest free slot (one a removed port left), or the
    /// next one past the end, and a generation no port before it had. It
    /// starts **active**. Control-thread only: this copies the port set and
    /// publishes the copy, so it allocates, may free the set it replaces, and
    /// must not be called from the audio thread. Safe to call from several
    /// control threads at once: each creator's port lands in its own slot.
    ///
    /// There is no other way to reach a port's producer: give the handle to the
    /// one thread that pushes. Dropping it leaves the port drainable but fed
    /// by nothing; [`remove_input_port`](Self::remove_input_port) retires it.
    #[must_use = "the handle is the port's only producer; dropping it leaves the port fed by nothing"]
    pub fn create_input_port(&self, name: impl Into<String>) -> InputProducerHandle {
        let name = name.into();
        let fifo_size = self.fifo_size;
        self.republish(move |slots, next_generation| {
            let index = slots
                .iter()
                .position(Option::is_none)
                .unwrap_or(slots.len());
            let id = PortId {
                index,
                generation: *next_generation,
            };
            *next_generation += 1;
            let (port, handle) = HardwareMidiInput::new(name, fifo_size, id);
            let port = Some(Arc::new(port));
            match slots.get_mut(index) {
                Some(slot) => *slot = port,
                None => slots.push(port),
            }
            handle
        })
    }

    /// Retires the port `id` names, freeing its slot for the next
    /// [`create_input_port`](Self::create_input_port). Returns `false`, and
    /// changes nothing, when `id` names no live port: one already removed,
    /// including one whose slot a newer port has taken.
    ///
    /// Events still in its ring are discarded with it. Control-thread only, as
    /// `create_input_port` is. Close the port's driver connection first, so
    /// nothing is still pushing into a ring nobody reads; a handle that
    /// outlives the port is harmless, since it owns its half of the ring, but
    /// whatever it pushes is lost.
    ///
    /// # When the memory goes
    ///
    /// The audio thread may be draining the port at that moment. The set it
    /// borrowed is retired rather than freed, so the drain never frees, and
    /// the port and its ring (once its producer is gone too) are freed on the
    /// control side: by this call if no drain holds the old set, or else by
    /// the next control-side change or [`collect`](Self::collect), whichever
    /// comes first. With no further changes a removed port's ring can stay
    /// allocated indefinitely; it is bounded (one retired set, a fixed number
    /// of rings) and correct, and a host that wants it back sooner calls
    /// `collect` from a periodic control-side task.
    pub fn remove_input_port(&self, id: PortId) -> bool {
        self.republish(|slots, _| {
            let removed = match slots.get_mut(id.index) {
                Some(slot) if slot.as_ref().is_some_and(|port| port.id() == id) => {
                    *slot = None;
                    true
                }
                _ => false,
            };
            // Trailing empty slots go too, so the set shrinks back once the
            // last port is gone rather than keeping its high-water mark.
            while slots.last().is_some_and(Option::is_none) {
                slots.pop();
            }
            removed
        })
    }

    /// Frees, on this thread, every retired port set no drain still holds, and
    /// with it every removed port only those sets kept alive. Returns how many
    /// retired sets are still held (by a drain in progress).
    ///
    /// Control-thread only. Cheap when there is nothing to do (a lock and a
    /// load); otherwise it publishes the current set again, which takes the
    /// same route every change does to free what no reader holds. A host that
    /// removes ports and then makes no further change calls this from a
    /// periodic control-side task to get the memory back.
    pub fn collect(&self) -> usize {
        let published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.input_ports.retired_len() > 0 {
            self.input_ports.publish(Arc::clone(&published.slots));
        }
        self.input_ports.retired_len()
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

    /// The live port `id` names, if any: a stale id names none.
    fn port(&self, port_type: PortType, id: PortId) -> Option<Arc<HardwareMidiInput>> {
        self.ports_of(port_type)?
            .read()
            .get(id.index)
            .and_then(Option::clone)
            .filter(|port| port.id() == id)
    }

    /// A snapshot of one port, or `None` when `id` names no live port.
    ///
    /// Always `None` for [`PortType::Output`]: outputs are not backed here.
    pub fn get_port_info(&self, port_type: PortType, id: PortId) -> Option<PortInfo> {
        self.port(port_type, id)
            .map(|port| PortInfo::of(&port, port_type))
    }

    /// Every input port, in slot order. Allocates — control thread only.
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
            .flatten()
            .map(|port| PortInfo::of(port, port_type))
            .collect()
    }

    /// How many slots the published set holds, live or free: the length the
    /// drain walks each block. Bounded by the most ports ever open at once.
    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.input_ports.read().len()
    }

    /// Sets whether the drain reads the port `id` names. Returns `false`, and
    /// changes nothing, when `id` names no live port (always so for
    /// [`PortType::Output`]).
    ///
    /// Deactivating does not drain or clear the ring: a driver keeps pushing and
    /// the events sit there until the port is active again, so a long pause is
    /// followed by a burst rather than by silence.
    pub fn set_port_active(&self, port_type: PortType, id: PortId, active: bool) -> bool {
        match self.port(port_type, id) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Whether the drain currently reads the port `id` names. `false` for a
    /// stale id and for every [`PortType::Output`] query.
    pub fn is_port_active(&self, port_type: PortType, id: PortId) -> bool {
        self.port(port_type, id)
            .is_some_and(|port| port.is_active())
    }

    /// Drains all active input port rings for this block, converting arrival
    /// timestamps to sample-accurate `frame_offset`s, and hand each event to
    /// `visit` with the [`PortId`] of the port it came from. Returns how many
    /// were visited.
    ///
    /// RT-safe (lock-free, no heap allocation, no free). A visitor rather than
    /// a returned slice: the events live in audio-thread-only scratch, and
    /// lending a borrow into it out of a `&self` method is the one shape that
    /// would need an `UnsafeCell`. `visit` is free to touch this manager — no
    /// borrow of the scratch is held while it runs — but a drain started from
    /// inside `visit` drains nothing and returns 0: the outer drain still has
    /// events to hand out from the same scratch, and the next block takes
    /// whatever arrived meanwhile. Audio thread only: one drain at a time.
    pub fn cycle_start_read_all_inputs(
        &self,
        nframes: usize,
        visit: impl FnMut(PortId, MidiEvent),
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
        mut visit: impl FnMut(PortId, MidiEvent),
    ) -> usize {
        if self.scratch.draining.swap(true, Ordering::Relaxed) {
            // Re-entered from the visitor of a drain in progress: refilling
            // the scratch would drop the events the outer drain has yet to
            // hand out.
            return 0;
        }
        let _draining = Draining(&self.scratch.draining);

        // One borrow of the port set for the drain, so a hotplug publishing a
        // new set mid-read leaves this one intact; the borrow cannot free it
        // when it drops, whatever was published meanwhile. Events are copied
        // into the scratch by value (`MidiEvent: Copy`), so nothing borrows
        // from the snapshot past it.
        let input_ports = self.input_ports.read();
        self.scratch
            .read_inputs(&input_ports, nframes, buffer_start);
        let mut visited = 0;
        loop {
            // Re-borrow per event rather than across the visitor, so `visit`
            // may reach back into this manager. Stop at the scratch's end
            // rather than at a count taken before the visits.
            let Some(&(port, event)) = self.scratch.events.borrow().get(visited) else {
                break;
            };
            visit(port, event);
            visited += 1;
        }
        visited
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
    fn drain_cycle(manager: &HardwareMidiInputs, nframes: usize) -> Vec<(PortId, MidiEvent)> {
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

        let input_id = manager.create_input_port("Test Input").port_id();
        assert_eq!(input_id.index(), 0);

        let input_ports = manager.list_input_ports();
        assert_eq!(input_ports.len(), 1);
        let input_info = &input_ports[0];
        assert_eq!(input_info.name, "Test Input");
        assert_eq!(input_info.port_type, PortType::Input);
        assert!(input_info.active);

        // Output is not a port-manager concern: it has no backing store, so it
        // always lists empty (outbound MIDI rides the mailbox → OutputThread).
        assert!(manager.get_port_info(PortType::Output, input_id).is_none());
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

        let id1 = manager.create_input_port("Input 1").port_id();
        let id2 = manager.create_input_port("Input 2").port_id();

        let inputs = manager.list_input_ports();
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().any(|p| p.id == id1));
        assert!(inputs.iter().any(|p| p.id == id2));
    }

    #[test]
    fn test_port_active_state() {
        let manager = HardwareMidiInputs::new(256);

        let port_id = manager.create_input_port("Test").port_id();
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
    fn manager_tags_drained_events_with_their_port_id() {
        let manager = HardwareMidiInputs::new(256);

        let mut producer_handle = manager.create_input_port("Input");
        let input_id = producer_handle.port_id();
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
        let input_id = producer_handle.port_id();

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
        let (id1, id2) = (handle1.port_id(), handle2.port_id());

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
    /// Mutation: releasing `republish`'s `published` lock between the copy
    /// and the publish (the read, append and publish then run unserialized)
    /// fails this with fewer than 8 x 200 ports listed and repeated indices.
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
                                manager
                                    .create_input_port(format!("{t}/{i}"))
                                    .port_id()
                                    .index(),
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
        let mut interior = manager.create_input_port("Interior").port_id();
        let anchor = manager.create_input_port("Anchor").port_id();
        assert_eq!((interior.index(), anchor.index()), (0, 1));

        let mut trailing_gone = None;
        for cycle in 0..100 {
            // An interior slot: index 0 is below the live anchor at 1.
            assert!(
                manager.remove_input_port(interior),
                "cycle {cycle}: the interior port is there"
            );
            let reused = manager.create_input_port("Interior").port_id();
            assert_eq!(reused.index(), 0, "cycle {cycle}: the freed slot is reused");
            assert_ne!(reused, interior, "cycle {cycle}: by a new port");
            interior = reused;

            // A trailing slot: index 2 is past every live port.
            let trailing = manager.create_input_port("Trailing").port_id();
            assert_eq!(trailing.index(), 2, "cycle {cycle}");
            assert!(manager.remove_input_port(trailing));
            trailing_gone = Some(trailing);

            assert_eq!(manager.list_input_ports().len(), 2, "cycle {cycle}");
            assert_eq!(
                manager.slot_count(),
                2,
                "cycle {cycle}: the set does not grow"
            );
        }
        let trailing_gone = trailing_gone.expect("the loop ran");
        assert!(
            !manager.remove_input_port(trailing_gone),
            "a removed port is gone"
        );
        assert!(manager
            .get_port_info(PortType::Input, trailing_gone)
            .is_none());
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
        let id = handle.port_id();
        assert!(handle.push(MidiEvent::noop(), now()));
        let port = {
            let slots = manager.input_ports.read();
            Arc::downgrade(slots[0].as_ref().expect("the port is live"))
        };
        drop(handle);

        assert!(manager.remove_input_port(id));
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
        assert!(manager.remove_input_port(handle.port_id()));
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

    /// A drain re-entered from its own visitor hands out nothing, and the
    /// outer drain still hands out every event, in order. The visitor may also
    /// touch the rest of the manager (list, toggle, read the rate).
    ///
    /// Ported from `RtEventBuf`'s `drain_each_callback_may_reborrow_the_buffer`.
    ///
    /// Mutations (run): dropping the `draining` guard → the nested drain clears
    /// the scratch, the outer one stops after its first event, and this fails
    /// on the count; dropping the guard *and* indexing `events[i]` for a count
    /// taken before the visits (the shape the review found) → it panics with
    /// "index out of bounds".
    #[test]
    fn a_drain_re_entered_from_its_visitor_hands_out_every_event() {
        let manager = HardwareMidiInputs::new(16);
        let mut a = manager.create_input_port("A");
        let mut b = manager.create_input_port("B");
        for i in 0..2 {
            assert!(a.push(MidiEvent::noop(), now()));
            assert!(b.push(MidiEvent::noop(), now()), "event {i}");
        }

        let mut outer = Vec::new();
        let mut nested = 0;
        let mut nested_visits = 0;
        let visited = manager.cycle_start_read_all_inputs(512, |port, _| {
            outer.push(port);
            // Push one more so a nested drain would have something to take.
            assert!(a.push(MidiEvent::noop(), now()));
            nested += manager.cycle_start_read_all_inputs(512, |_, _| nested_visits += 1);
            assert_eq!(manager.list_input_ports().len(), 2);
            assert!(manager.set_port_active(PortType::Input, b.port_id(), true));
            manager.set_sample_rate(48_000.0);
        });

        assert_eq!(visited, 4, "every event of the outer drain was visited");
        assert_eq!(
            outer,
            vec![a.port_id(), a.port_id(), b.port_id(), b.port_id()]
        );
        assert_eq!(nested, 0, "each nested drain returned 0");
        assert_eq!(nested_visits, 0, "and visited nothing");
        assert_eq!(
            drain_cycle(&manager, 512).len(),
            4,
            "what arrived during the visits waits for the next block"
        );
    }

    /// A stale `PortId` cannot reach the port that took its slot.
    ///
    /// Port A is removed and port B created in the same slot. A's id must not
    /// remove, toggle or describe B, and B's events carry B's id, not A's.
    ///
    /// Mutation (run): `PortId`'s generation ignored (the lookup and the
    /// removal compare `index` only, as keys did before) → A's id removes B →
    /// fails.
    #[test]
    fn a_stale_port_id_cannot_reach_the_port_that_reused_its_slot() {
        let manager = HardwareMidiInputs::new(16);
        let a = manager.create_input_port("A").port_id();
        assert!(manager.remove_input_port(a));
        let mut b = manager.create_input_port("B");
        let b_id = b.port_id();
        assert_eq!(b_id.index(), a.index(), "B reuses A's slot");
        assert_ne!(b_id, a);

        assert!(!manager.set_port_active(PortType::Input, a, false));
        assert!(!manager.is_port_active(PortType::Input, a));
        assert!(manager.get_port_info(PortType::Input, a).is_none());
        assert!(!manager.remove_input_port(a), "a stale id removes nothing");

        let info = manager
            .get_port_info(PortType::Input, b_id)
            .expect("B is still there");
        assert_eq!((info.name.as_str(), info.active), ("B", true));
        assert!(b.push(MidiEvent::noop(), now()));
        let events = drain_cycle(&manager, 512);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, b_id, "B's event carries B's id");
    }

    /// The sample rate may be set from a control thread while the audio thread
    /// drains: no shared borrow, no torn value, and the rate a drain uses is
    /// one of those set.
    ///
    /// Mutation (run): the rate back in an `AudioThreadCell` written with
    /// `borrow_mut` (the old shape) → the two threads trip the cell's in-use
    /// flag and the test panics (in a debug build when this was run; the flag
    /// is checked in release builds too now).
    #[test]
    #[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
    fn the_sample_rate_can_be_set_while_a_drain_runs() {
        use core::time::Duration;
        use std::sync::atomic::AtomicBool;

        let manager = Arc::new(HardwareMidiInputs::new(64));
        manager.set_sample_rate(48_000.0);
        let mut handle = manager.create_input_port("Input");
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let manager = Arc::clone(&manager);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut flip = false;
                while !stop.load(Ordering::Relaxed) {
                    manager.set_sample_rate(if flip { 48_000.0 } else { 96_000.0 });
                    flip = !flip;
                }
            })
        };

        let block_start = Instant::now();
        for _ in 0..20_000 {
            assert!(handle.push(MidiEvent::noop(), block_start - Duration::from_millis(1)));
            manager.cycle_start_read_all_inputs_at(1024, block_start, |_, e| {
                // 1 ms at 48 kHz or at 96 kHz, never anything else.
                assert!(
                    e.frame_offset == 1024 - 48 || e.frame_offset == 1024 - 96,
                    "offset {} is from neither rate",
                    e.frame_offset
                );
            });
        }
        stop.store(true, Ordering::Relaxed);
        setter.join().expect("setter thread");
    }

    /// A port removed while a drain holds the set is not freed by the drain;
    /// `collect` frees it on the control side once the drain is done, with no
    /// further change needed.
    ///
    /// Mutation (run): `collect` not publishing (returning `retired_len` only)
    /// → the port stays alive → fails.
    #[test]
    fn collect_frees_a_port_removed_during_a_drain() {
        let manager = HardwareMidiInputs::new(16);
        let mut handle = manager.create_input_port("Gone");
        let id = handle.port_id();
        assert!(handle.push(MidiEvent::noop(), now()));
        drop(handle);
        let port = Arc::downgrade(
            manager.input_ports.read()[0]
                .as_ref()
                .expect("the port is live"),
        );

        manager.cycle_start_read_all_inputs(512, |_, _| {
            assert!(manager.remove_input_port(id));
        });
        assert!(
            port.upgrade().is_some(),
            "the drain held the set when the port was removed"
        );

        assert_eq!(manager.collect(), 0, "nothing is still held");
        assert!(port.upgrade().is_none(), "collect freed the port");
        assert_eq!(
            manager.collect(),
            0,
            "and a second collect has nothing to do"
        );
    }
}
