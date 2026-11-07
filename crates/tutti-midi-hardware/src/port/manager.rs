//! [`HardwareMidiInputs`] — every open input port — and [`HardwareMidiDrain`],
//! the per-block drain the audio thread calls.
//!
//! One manager holds N input rings. A drain's `cycle_start_read_all_inputs`
//! fans them all in for one block, converting each event's arrival `Instant`
//! into a `frame_offset` within that block. Everything on that path is
//! RT-safe: bounded work, no locks, no allocation, and no free.
//!
//! The manager is shared (`Arc`); a drain is owned by the one node that polls
//! it (a `MidiInputNode`), built on the control thread with
//! [`HardwareMidiDrain::new`]. Its scratch and its read of the published port
//! set are its own, so the audio thread reads nothing through the shared
//! manager but atomics.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tutti_rt::{ArcLend, Reader};
use tutti_types::SampleRate;

use super::async_port::{HardwareMidiInput, InputProducerHandle};
use tutti_midi_types::ump::MidiEvent;

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
    /// Whether the drain reads this port. An inactive port keeps accepting
    /// pushes into its ring; they are simply never taken.
    pub active: bool,
    /// How many events the driver dropped for this port because its ring was
    /// full, since the port opened.
    pub dropped: u64,
}

impl PortInfo {
    fn of(port: &HardwareMidiInput) -> Self {
        Self {
            id: port.id(),
            name: port.name().to_string(),
            active: port.is_active(),
            dropped: port.dropped(),
        }
    }
}

/// Events carried from all ports through one audio block, until the owner
/// sizes the drain for its block ([`HardwareMidiDrain::reserve`]).
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

/// The drain state every [`HardwareMidiDrain`] of one manager shares.
///
/// `sample_rate` is written by the control side and read by the drain, so it
/// is an atomic (the `f64`'s bits), read once per block. `draining` marks a
/// drain in progress, so a second drain of the same manager started while one
/// runs (from the first's visitor, or a second node on another worker) is
/// refused rather than contending for a port's ring. Both are `Sync` on their
/// own; the type derives it with no hand-written `unsafe impl`.
struct DrainShared {
    sample_rate: AtomicU64,
    draining: AtomicBool,
}

/// Clears [`DrainShared::draining`] when the drain ends, however it ends: a
/// visitor that panics must not leave every later drain refused.
struct Draining<'a>(&'a AtomicBool);

impl Drop for Draining<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl DrainShared {
    fn new(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate: AtomicU64::new(sample_rate.get().to_bits()),
            draining: AtomicBool::new(false),
        }
    }

    fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate
            .store(sample_rate.into().get().to_bits(), Ordering::Relaxed);
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from_bits(self.sample_rate.load(Ordering::Relaxed)))
    }
}

/// Drain `input_ports`' active rings into `events`, converting arrival
/// timestamps to sample-accurate `frame_offset`s at `sample_rate`, and return
/// how many were taken. RT-safe (lock-free, no heap allocation).
///
/// `buffer_start` is when this block began — the instant every event's age
/// is measured back from. Taken as a parameter rather than read from the
/// clock here: it is the *only* input to the conversion a caller does not
/// otherwise control, so passing it is what makes the arithmetic below
/// checkable at all. See
/// [`cycle_start_read_all_inputs_at`](HardwareMidiDrain::cycle_start_read_all_inputs_at).
fn read_inputs(
    events: &mut Vec<(PortId, MidiEvent)>,
    limit: usize,
    input_ports: &PortSlots,
    sample_rate: SampleRate,
    nframes: usize,
    buffer_start: Instant,
) -> usize {
    let nframes_u32 = u32::try_from(nframes).unwrap_or(u32::MAX);
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
    // total can never exceed the limit, which the scratch holds, and no push
    // reallocates.
    events.clear();
    for port in input_ports.iter().flatten() {
        let headroom = limit - events.len();
        if headroom == 0 {
            break;
        }
        if !port.is_active() {
            continue;
        }
        let id = port.id();
        port.drain_limited(headroom, |arrival, mut event| {
            event.frame_offset = frame_offset(arrival);
            debug_assert!(events.len() < events.capacity(), "limited to the headroom");
            events.push((id, event));
        });
    }
    events.len()
}

/// The next generation to hand out. Behind [`HardwareMidiInputs::published`]'s
/// lock, which also orders every change; the set itself is the cell's, read
/// with [`ArcLend::current`].
struct Published {
    next_generation: u64,
}

/// Every open hardware MIDI input.
///
/// A backend creates one port per connection with
/// [`open_input_port`](Self::open_input_port) and pushes into the
/// [`InputProducerHandle`] it returns from the driver's own thread; the audio
/// thread drains what arrived once per block through a [`HardwareMidiDrain`]
/// over these inputs ([`cycle_start_read_all_inputs`]), owned by the node
/// that reads. When the connection closes, [`remove_input_port`] retires the port
/// and frees its slot for the next one, so hot-plug cycles do not grow the set.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session and the
/// audio graph.
///
/// [`cycle_start_read_all_inputs`]: HardwareMidiDrain::cycle_start_read_all_inputs
/// [`remove_input_port`]: HardwareMidiInputs::remove_input_port
pub struct HardwareMidiInputs {
    /// The port set, published to the audio thread's drain. An [`ArcLend`]
    /// rather than an `ArcSwap`: the drain only borrows the set, so a hotplug
    /// that publishes a new one mid-drain retires the old one for the
    /// *control* side to free. (An `ArcSwap` guard held across that store is
    /// upgraded to a strong count, and its drop then freed the old `Vec` on
    /// the audio thread.) The same holds for a removed port: the set that
    /// still holds it is retired, and the port and its ring are freed on the
    /// control side (see [`collect`](Self::collect)). Each drain lends it
    /// through a reader of its own, registered in [`HardwareMidiDrain::new`].
    input_ports: Arc<ArcLend<PortSlots>>,
    /// Serializes the read, change and publish of every control-side change
    /// (`ArcLend` has no compare-and-publish), so two control threads racing
    /// on hotplug cannot each publish a copy missing the other's change. The
    /// drain never takes it.
    published: Mutex<Published>,
    fifo_size: usize,
    /// The drops of the ports removed so far, so a retired port's count stays
    /// in [`dropped_events`](Self::dropped_events).
    retired_dropped: AtomicU64,
    /// What every drain of this manager shares: the rate and the in-progress
    /// flag. Every field here is `Sync` on its own, so the manager derives
    /// `Sync` rather than asserting it by hand.
    shared: DrainShared,
}

impl HardwareMidiInputs {
    /// The ring size of a manager built for a session, in events.
    pub const DEFAULT_FIFO: usize = 2048;

    /// A manager with no ports, whose rings will each hold `fifo_size` events
    /// and whose drains convert arrival times at `sample_rate`.
    ///
    /// `fifo_size` is per port, in events: a driver pushing past it drops the
    /// overflow (`push` returns `false`) rather than blocking its callback.
    /// The rate is the device's: a wrong one places events at the wrong offset
    /// within a block ([`set_sample_rate`](Self::set_sample_rate) follows a
    /// device restart).
    #[must_use]
    pub fn new(fifo_size: usize, sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            input_ports: Arc::new(ArcLend::new(PortSlots::new())),
            published: Mutex::new(Published { next_generation: 0 }),
            fifo_size,
            retired_dropped: AtomicU64::new(0),
            shared: DrainShared::new(sample_rate.into()),
        }
    }

    /// Events the drivers dropped for want of room in a port's ring, over all
    /// ports, current and removed, since this manager was built. The audio
    /// thread's drain takes events out at a bounded rate per block and
    /// defers the rest to the next ([`HardwareMidiDrain::reserve`]); a ring
    /// fills only when events arrive faster than blocks drain them for long
    /// enough to fill [`new`](Self::new)'s `fifo_size`. Control side.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        let live: u64 = self
            .published_slots()
            .iter()
            .flatten()
            .map(|port| port.dropped())
            .sum();
        live + self.retired_dropped.load(Ordering::Relaxed)
    }

    /// Sets the [`SampleRate`] the arrival-timestamp → `frame_offset` conversion
    /// divides by.
    ///
    /// Safe to call from any thread at any time: the rate is an atomic the
    /// drain reads once per block, so a change lands on the next block. A stale
    /// rate does not drop events; it places them at the wrong offset within the
    /// block, which reads as timing jitter rather than as an error.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.shared.set_sample_rate(sample_rate);
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
        let mut slots = (*self.input_ports.current()).clone();
        let out = change(&mut slots, &mut published.next_generation);
        self.input_ports.publish(slots);
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
    pub fn open_input_port(&self, name: impl Into<String>) -> InputProducerHandle {
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
    /// [`open_input_port`](Self::open_input_port). Returns `false`, and
    /// changes nothing, when `id` names no live port: one already removed,
    /// including one whose slot a newer port has taken.
    ///
    /// Events still in its ring are discarded with it. Control-thread only, as
    /// `open_input_port` is. Close the port's driver connection first, so
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
                    if let Some(port) = slot.take() {
                        self.retired_dropped
                            .fetch_add(port.dropped(), Ordering::Relaxed);
                    }
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
    /// Control-thread only. Cheap when there is nothing to do (two locks and
    /// a length check); otherwise one scan of the drains' readers
    /// ([`ArcLend::reclaim`]), publishing nothing. A host that removes ports
    /// and then makes no further change calls this from a periodic
    /// control-side task to get the memory back (a `tutti-session` session
    /// does, in its `Reclaim` phase).
    pub fn collect(&self) -> usize {
        // Held so a change racing this call frees in one order: `reclaim`
        // takes the cell's publisher lock too, but this lock is the one
        // every control-side change takes first.
        let _published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.input_ports.reclaim()
    }

    /// How many port sets are retired and not yet freed (by a change or a
    /// [`collect`](Self::collect)). Control side. For a test of an owner's
    /// sweep; not API.
    #[doc(hidden)]
    pub fn retired_sets(&self) -> usize {
        self.input_ports.retired_len()
    }

    /// The port set last published: what every control-side query reads, as
    /// an owning handle from the cell ([`ArcLend::current`]), never a lend. A
    /// control-side call, from any thread but the audio thread's drain.
    fn published_slots(&self) -> Arc<PortSlots> {
        self.input_ports.current()
    }

    /// The live port `id` names, if any: a stale id names none.
    fn port(&self, id: PortId) -> Option<Arc<HardwareMidiInput>> {
        self.published_slots()
            .get(id.index)
            .and_then(Option::clone)
            .filter(|port| port.id() == id)
    }

    /// A snapshot of one port, or `None` when `id` names no live port.
    /// Control side, as every query here: see `published_slots`.
    pub fn port_info(&self, id: PortId) -> Option<PortInfo> {
        self.port(id).map(|port| PortInfo::of(&port))
    }

    /// Every input port, in slot order. Allocates — control thread only.
    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.published_slots()
            .iter()
            .flatten()
            .map(|port| PortInfo::of(port))
            .collect()
    }

    /// How many slots the published set holds, live or free: the length the
    /// drain walks each block. Bounded by the most ports ever open at once.
    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.published_slots().len()
    }

    /// Sets whether the drain reads the port `id` names. Returns `false`, and
    /// changes nothing, when `id` names no live port.
    ///
    /// Deactivating does not drain or clear the ring: a driver keeps pushing and
    /// the events sit there until the port is active again, so a long pause is
    /// followed by a burst rather than by silence.
    pub fn set_port_active(&self, id: PortId, active: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Whether the drain currently reads the port `id` names. `false` for a stale id.
    pub fn is_port_active(&self, id: PortId) -> bool {
        self.port(id).is_some_and(|port| port.is_active())
    }
}

/// One node's drain of a [`HardwareMidiInputs`]: what the audio thread calls
/// once per block to take every active port's events.
///
/// Built on the control thread ([`new`](Self::new)) and owned by the one node
/// that polls it (a `MidiInputNode` takes it as its `MidiIn`). Its scratch
/// and its read of the published port set are its own, so nothing the audio
/// thread reads is reached through the shared manager but its atomics.
///
/// A drain is polled through `&mut self`, so it cannot be re-entered from its
/// own visitor:
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use tutti_midi_hardware::{HardwareMidiDrain, HardwareMidiInputs};
/// let mut drain = HardwareMidiDrain::new(Arc::new(HardwareMidiInputs::new(16, 44_100.0)));
/// drain.cycle_start_read_all_inputs(512, |_, _| {
///     drain.cycle_start_read_all_inputs(512, |_, _| {});
/// });
/// ```
///
/// A *second* drain of the same manager started while one runs (from the
/// first's visitor, or on another worker) drains nothing and returns 0: one
/// drain at a time takes a manager's events.
pub struct HardwareMidiDrain {
    inputs: Arc<HardwareMidiInputs>,
    /// A reader of `inputs`' port set, registered in [`new`](Self::new): what
    /// the drain lends the set through, once per drain.
    ports: Reader<PortSlots>,
    /// The block's drained events, already timed. One buffer, not two: the
    /// timestamp → `frame_offset` conversion needs only the rate, `nframes`
    /// and the block start, all known before the first pop, so each event is
    /// converted as it leaves its ring and stored once. The events reach the
    /// caller through a visitor, copied out one at a time.
    events: Vec<(PortId, MidiEvent)>,
    /// The most events one drain takes: at most `events`' capacity.
    limit: usize,
}

impl HardwareMidiDrain {
    /// A drain of `inputs`, for one node. Control thread: it registers a
    /// reader of the port set, which may allocate.
    pub fn new(inputs: Arc<HardwareMidiInputs>) -> Self {
        Self {
            ports: inputs.input_ports.reader(),
            inputs,
            events: Vec::with_capacity(CYCLE_SCRATCH_CAP),
            limit: CYCLE_SCRATCH_CAP,
        }
    }

    /// Sizes the drain to take up to `events` events a block (never fewer
    /// than it takes by default). What a node does for the block it is
    /// prepared for: a longer block gathers more events between two drains,
    /// and the drain defers what it has no room for to the next block, one
    /// block later each. Control thread: allocates.
    pub fn reserve(&mut self, events: usize) {
        let want = events.max(CYCLE_SCRATCH_CAP);
        self.events
            .reserve_exact(want.saturating_sub(self.events.len()));
        self.limit = want.min(self.events.capacity());
    }

    /// The most events one drain takes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.limit
    }

    /// The manager this drains.
    pub fn inputs(&self) -> &Arc<HardwareMidiInputs> {
        &self.inputs
    }

    /// Drains all active input port rings for this block, converting arrival
    /// timestamps to sample-accurate `frame_offset`s, and hand each event to
    /// `visit` with the [`PortId`] of the port it came from. Returns how many
    /// were visited.
    ///
    /// RT-safe (lock-free, no heap allocation, no free). `visit` is free to
    /// touch the manager (its queries take the control side's lock, so not
    /// from a real audio callback), but a second drain of the same manager
    /// started from inside `visit` drains nothing and returns 0; the next
    /// block takes whatever arrived meanwhile. Audio thread only.
    pub fn cycle_start_read_all_inputs(
        &mut self,
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
        &mut self,
        nframes: usize,
        buffer_start: Instant,
        visit: impl FnMut(PortId, MidiEvent),
    ) -> usize {
        let limit = self.limit;
        self.read_limited(nframes, buffer_start, limit, visit)
    }

    /// The drain, taking at most `limit` events (and no more than
    /// [`capacity`](Self::capacity)); what it leaves stays in the rings.
    fn read_limited(
        &mut self,
        nframes: usize,
        buffer_start: Instant,
        limit: usize,
        mut visit: impl FnMut(PortId, MidiEvent),
    ) -> usize {
        let shared = &self.inputs.shared;
        if shared.draining.swap(true, Ordering::Relaxed) {
            // Another drain of this manager is in progress (this one's
            // visitor started it, or another node runs on another worker):
            // the ports' rings are its until it ends.
            return 0;
        }
        let _draining = Draining(&shared.draining);

        // One borrow of the port set for the drain, so a hotplug publishing a
        // new set mid-read leaves this one intact; the borrow cannot free it
        // when it drops, whatever was published meanwhile. Events are copied
        // into the scratch by value (`MidiEvent: Copy`), so nothing borrows
        // from the snapshot past it. The borrow is held to the drain's end:
        // a port removed while the drain runs is freed by the control side
        // afterwards (`collect`), never by this borrow's drop.
        let input_ports = self.ports.lend();
        read_inputs(
            &mut self.events,
            limit.min(self.limit),
            &input_ports,
            shared.sample_rate(),
            nframes,
            buffer_start,
        );
        for &(port, event) in self.events.iter() {
            visit(port, event);
        }
        self.events.len()
    }
}

impl tutti_midi_types::MidiIn for HardwareMidiDrain {
    /// Drains every connected hardware input for this block into `buffer`.
    ///
    /// `block_size` drives the timestamp → `frame_offset` conversion (it is the
    /// block's `nframes`). RT-safe: a bounded copy out of the drain's own
    /// scratch, with no allocation.
    ///
    /// Takes no more events than `buffer` holds: the rest stay in the port
    /// rings for the next block, so a small `buffer` defers events and drops
    /// none.
    fn poll_block(&mut self, block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        let mut written = 0usize;
        self.read_limited(block_size, Instant::now(), buffer.len(), |_port, event| {
            buffer[written] = event;
            written += 1;
        });
        written
    }

    /// As [`HardwareMidiDrain::reserve`].
    fn reserve(&mut self, events: usize) {
        HardwareMidiDrain::reserve(self, events);
    }
}

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let input_ports = self.published_slots();

        f.debug_struct("HardwareMidiInputs")
            .field(
                "num_input_ports",
                &input_ports.iter().filter(|p| p.is_some()).count(),
            )
            .field("fifo_size", &self.fifo_size)
            .finish()
    }
}

impl core::fmt::Debug for HardwareMidiDrain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiDrain")
            .field("inputs", &self.inputs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
