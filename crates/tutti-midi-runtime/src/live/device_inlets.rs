//! [`DeviceInlets`]: one event inlet per hardware MIDI device, and the
//! producer end ([`DeviceInput`]) the device's thread stamps and pushes through.
//!
//! A device's thread learns when an event arrived from the OS, as an
//! [`Instant`]. The graph wants a frame. The thread places each arrival itself,
//! so the audio thread converts nothing: it holds a [`ClockMap`] fed the
//! engine's block-start readings (an [`ArrivalClock`]: one per render call, the
//! OS time the block began and the graph frame it began at) and pushes the
//! event for the [`Moment`] the map names, through the device's own
//! [`EventInletWriter`]. The inlet holds the
//! event until its block and emits it on its frame, and a
//! [`MidiIngestNode`](crate::MidiIngestNode) reads one inlet per slot.
//!
//! # The lead
//!
//! An event that arrived between two blocks maps to a frame before the next
//! block's start: the block that would play it has already begun by the time
//! the thread pushes it, so it would land late, at offset 0, and the spacing
//! of a run of events would collapse. The thread therefore places each arrival
//! a fixed **lead** after its mapped frame ([`DeviceInlets::new`]: a block's
//! length is the natural value). An event then sounds exactly the lead after
//! it arrived, whatever the block boundaries, and the spacing between events
//! is the map's: within the bound [`ClockMap`] states. An event is late, and counted so
//! (`late` in [`InletCounts::losses`]), only when the
//! thread's push comes more than the lead after its arrival.
//!
//! # Slots
//!
//! The node has a fixed number of event inputs, so a device takes one of
//! `slots` ([`DeviceInlets::open`]), and gives it back by dropping its
//! [`DeviceInput`]: the slot is the device's for as long as the input lives.
//! The graph is edited by its owner, on the control thread; a device connects
//! from any thread. The two meet in a list of changes
//! ([`take_changes`](DeviceInlets::take_changes)): the owner adds the inlet's
//! node to the graph and wires it to the slot's input, or takes it out.
//!
//! A slot's **epoch** counts its comings and goings: each open and each release
//! adds one, so it is odd while a device holds the slot and even while none
//! does. A [`MidiIngestNode`](crate::MidiIngestNode) built
//! [`following`](crate::MidiIngestNode::following) these inlets compares each
//! slot's epoch every block. A change ends what the device before it started:
//! the notes it held and the pedals and bends it left are released downstream,
//! and its half-sent RPN and MPE state are dropped, so none is carried into the
//! next device, and a slot whose device went (even) hears nothing until another
//! takes it.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use tutti_graph::{EventCapacity, EventInletNode, EventInletWriter, InletCounts, Stamp};
use tutti_rt::{ArcLend, Reader};
use tutti_types::Moment;
use tutti_ump::Ump;

use crate::clock_map::{ClockMap, ClockReading, Observed};

/// The engine's block clock as a device's thread reads it: the latest render
/// call's start as an [`Instant`] and the graph [`Moment`] it began at, `None`
/// before the first call. A function, not an engine type: this crate depends on
/// no engine, and whoever owns both ends (the session) builds it, converting
/// the engine's OS nanoseconds onto the `Instant`s the backends stamp with.
/// Called from each device's thread on every push: lock-free and
/// allocation-free.
pub type ArrivalClock = Arc<dyn Fn() -> Option<(Instant, Moment)> + Send + Sync>;

/// Which of a [`DeviceInlets`]' slots a device holds, and so the one identity
/// of an input port: its index, and which device to take the slot it is, so a
/// stale slot (its device gone, the slot reused) names nothing.
///
/// Ephemeral: it names a connection of this run, so it has no serde (a saved
/// device is a `DeviceId`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceSlot {
    index: usize,
    generation: u64,
}

impl DeviceSlot {
    /// The slot's index: the event input of the ingest node it feeds. Unique
    /// among the slots held at one time, and reused after a device lets go
    /// of it; compare `DeviceSlot`s, not indices, to tell devices apart.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }

    /// Which device to take this slot it is: unique for the inlets' life,
    /// increasing in the order devices opened.
    #[must_use]
    pub fn generation(self) -> u64 {
        self.generation
    }
}

/// Every slot is taken: [`DeviceInlets::open`] has none to give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("all {} MIDI device slots are taken", .slots)]
pub struct NoFreeSlot {
    /// How many slots there are.
    pub slots: usize,
}

/// What the graph's owner does about a device: the node to add and wire, or
/// the slot to unwire. Taken in order ([`DeviceInlets::take_changes`]).
#[derive(Debug)]
pub enum InletChange {
    /// A device took `slot`: add `node` to the graph and feed the slot's input
    /// from its one event output.
    Attach {
        /// The slot's index.
        slot: usize,
        /// The inlet the device pushes into.
        node: EventInletNode,
    },
    /// The device let go of `slot`: take its inlet out.
    Detach {
        /// The slot's index.
        slot: usize,
    },
}

#[derive(Default)]
struct State {
    /// The generation of the device holding each slot, `None` when free.
    slots: Vec<Option<u64>>,
    next_generation: u64,
    changes: VecDeque<InletChange>,
}

/// The hardware devices' event inlets, shared between the threads that connect
/// devices and the owner of the graph. See the module docs.
pub struct DeviceInlets {
    /// What a [`DeviceInput`] gives its slot back to when it drops.
    this: Weak<DeviceInlets>,
    state: Mutex<State>,
    /// Each slot's epoch: one more for every open and every release (0 before
    /// any), so odd while a device holds the slot; for an ingest node to see
    /// a device come or go by.
    epochs: Arc<[AtomicU64]>,
    /// How many slots there are: never zero.
    count: NonZeroUsize,
    capacity: EventCapacity,
    /// Nanoseconds: what each device's thread adds to an arrival's frame.
    lead: Arc<AtomicU64>,
    /// The engine's block clock, lent to each device's thread. `None` until
    /// the owner has an engine ([`set_clock`](Self::set_clock)).
    clock: Arc<ArcLend<Option<ArrivalClock>>>,
}

impl std::fmt::Debug for DeviceInlets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInlets")
            .field("slots", &self.slots())
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl DeviceInlets {
    /// `slots` device slots, each device's inlet holding
    /// `capacity` events, and `lead` the time each arrival is placed after its
    /// mapped frame (see the module docs).
    #[must_use]
    pub fn new(slots: NonZeroUsize, capacity: EventCapacity, lead: Duration) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: Weak::clone(this),
            state: Mutex::new(State {
                slots: vec![None; slots.get()],
                ..State::default()
            }),
            count: slots,
            epochs: (0..slots.get()).map(|_| AtomicU64::new(0)).collect(),
            capacity,
            lead: Arc::new(AtomicU64::new(nanos(lead))),
            clock: Arc::new(ArcLend::new(None)),
        })
    }

    /// The slots' epochs, for a node that follows them.
    pub(crate) fn epochs(&self) -> Arc<[AtomicU64]> {
        Arc::clone(&self.epochs)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock guards nothing inconsistent: every change is whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many device slots there are.
    #[must_use]
    pub fn slots(&self) -> NonZeroUsize {
        self.count
    }

    /// Sets the engine's block clock the devices' threads place arrivals
    /// with (`None`: there is none, and events are pushed for the next
    /// block). Takes effect on each thread's next event. Control side.
    pub fn set_clock(&self, clock: Option<ArrivalClock>) {
        self.clock.publish(clock);
    }

    /// Sets the lead (see the module docs); each device's thread reads it on
    /// its next event.
    pub fn set_lead(&self, lead: Duration) {
        self.lead.store(nanos(lead), Ordering::Relaxed);
    }

    /// Frees the clocks the devices' threads have let go of; control side, from
    /// the owner's tick (a session's `Reclaim` phase). Returns how many are
    /// still held.
    pub fn reclaim(&self) -> usize {
        self.clock.reclaim()
    }

    /// Gives a device a slot and the end its thread pushes through; the
    /// owner of the graph finds the inlet to add in
    /// [`take_changes`](Self::take_changes). Control side: allocates the
    /// inlet's ring and waiting list.
    ///
    /// The slot is the device's until the returned [`DeviceInput`] drops.
    ///
    /// # Errors
    ///
    /// [`NoFreeSlot`] when every slot is held.
    pub fn open(&self) -> Result<DeviceInput, NoFreeSlot> {
        let mut state = self.state();
        let slots = state.slots.len();
        let index = state
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(NoFreeSlot { slots })?;
        let generation = state.next_generation;
        state.next_generation += 1;
        state.slots[index] = Some(generation);
        // Before the node can see the inlet: a block that reads the new epoch
        // restarts the slot's state, and the attach follows. Release pairs
        // with the node's acquire load.
        self.epochs[index].fetch_add(1, Ordering::Release);
        let (node, writer) = EventInletNode::pair(self.capacity);
        state
            .changes
            .push_back(InletChange::Attach { slot: index, node });
        Ok(DeviceInput {
            slot: DeviceSlot { index, generation },
            counts: writer.counts(),
            writer,
            inlets: Weak::clone(&self.this),
            clock: self.clock.reader(),
            lead: Arc::clone(&self.lead),
            seen: None,
            map: None,
        })
    }

    /// Frees `slot` for the next device and queues the inlet's removal; what
    /// a [`DeviceInput`]'s drop does. A slot not held (a device that took it
    /// since) is left alone. An inlet still waiting in
    /// [`take_changes`](Self::take_changes) is dropped with its attach.
    fn release(&self, slot: DeviceSlot) {
        let mut state = self.state();
        match state.slots.get_mut(slot.index) {
            Some(held) if *held == Some(slot.generation) => {
                *held = None;
                // The device is gone: a node that follows the epoch releases
                // what it started, though its inlet is still attached until the
                // owner takes the detach.
                self.epochs[slot.index].fetch_add(1, Ordering::Release);
                // An inlet the graph's owner has not taken yet never reached
                // the graph: its attach and this detach cancel, so a device
                // that comes and goes with no owner draining leaves nothing.
                let pending = state.changes.iter().position(
                    |c| matches!(c, InletChange::Attach { slot: s, .. } if *s == slot.index),
                );
                match pending {
                    Some(at) => {
                        state.changes.remove(at);
                    }
                    None => state
                        .changes
                        .push_back(InletChange::Detach { slot: slot.index }),
                }
            }
            _ => {}
        }
    }

    /// The changes since the last call, in order, for the graph's owner to
    /// apply: an iterator that drains the list as it goes, so what the owner
    /// does not take stays queued for its next call. **No lock is held while
    /// the caller runs**: each step takes the next change under the lock and
    /// lets it go before returning it, so applying a change may open or drop
    /// a [`DeviceInput`] without deadlock (and a change queued meanwhile is
    /// yielded by the same iterator). Control side.
    pub fn take_changes(&self) -> Changes<'_> {
        Changes { inlets: self }
    }
}

/// The changes [`DeviceInlets::take_changes`] drains: one per step, oldest
/// first, until none is queued.
#[derive(Debug)]
#[must_use = "an unread change stays queued: the iterator does nothing until it is read"]
pub struct Changes<'a> {
    inlets: &'a DeviceInlets,
}

impl Iterator for Changes<'_> {
    type Item = InletChange;

    fn next(&mut self) -> Option<InletChange> {
        self.inlets.state().changes.pop_front()
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// One device's end of its inlet: stamp each arrival with a frame and push it.
/// Owned by the device's thread: `Send`, not `Clone`, and
/// [`push`](Self::push) takes `&mut self`. Nothing here blocks, locks or
/// allocates.
///
/// **Dropping it gives the slot back** and queues the inlet's removal
/// ([`DeviceInlets::take_changes`]): the slot is held exactly as long as the
/// input lives, so there is nothing to close. The drop takes the inlets' lock,
/// so it is for the device's own thread or the control side, never a driver
/// callback's hot path.
pub struct DeviceInput {
    slot: DeviceSlot,
    writer: EventInletWriter,
    counts: InletCounts,
    /// The inlets to give the slot back to; nothing if they are gone.
    inlets: Weak<DeviceInlets>,
    clock: Reader<Option<ArrivalClock>>,
    lead: Arc<AtomicU64>,
    /// The latest reading the map was fed.
    seen: Option<(Instant, Moment)>,
    map: Option<ClockMap>,
}

impl std::fmt::Debug for DeviceInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInput")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

impl Drop for DeviceInput {
    fn drop(&mut self) {
        if let Some(inlets) = self.inlets.upgrade() {
            inlets.release(self.slot);
        }
    }
}

impl DeviceInput {
    /// The slot this device holds, until it drops.
    #[must_use]
    pub fn slot(&self) -> DeviceSlot {
        self.slot
    }

    /// The inlet's counts, readable from any thread ([`InletCounts::losses`]):
    /// events the ring had no room for (`lost`), the waiting list had no room
    /// for (`evicted`), the port refused (`refused`), and events that came
    /// late.
    #[must_use]
    pub fn counts(&self) -> InletCounts {
        self.counts.clone()
    }

    /// Pushes `ump`, which the OS delivered at `arrival`, for the frame the
    /// block clock puts that time on, plus the lead; or for the next block
    /// while no reading exists (no engine yet, or `arrival` before frame 0).
    /// `false` when the ring was full: the event is dropped and counted in
    /// [`Losses::lost`](tutti_graph::Losses::lost). Lock-free and allocation-free, so safe to
    /// call from a driver callback.
    #[inline]
    pub fn push(&mut self, ump: impl Into<Ump>, arrival: Instant) -> bool {
        let ump = ump.into();
        let sent = match self.moment_of(arrival) {
            Some(at) => self.writer.push(at, ump),
            None => self.writer.push(Stamp::NextBlock, ump),
        };
        sent.is_ok()
    }

    /// The moment `arrival` plus the lead is, by the latest block reading.
    fn moment_of(&mut self, arrival: Instant) -> Option<Moment> {
        let reading = {
            let clock = self.clock.lend();
            (clock.as_ref()?)()?
        };
        if self.seen != Some(reading) {
            self.seen = Some(reading);
            self.follow(reading);
        }
        let lead = Duration::from_nanos(self.lead.load(Ordering::Relaxed));
        let at = arrival.checked_add(lead).unwrap_or(arrival);
        self.map.as_ref()?.moment_at(at)
    }

    /// Feeds the map a reading: it follows the engine's clock, or is built
    /// again where the clock was restarted (a reading the map rejects, a
    /// frame that went back) or at another rate.
    fn follow(&mut self, (os, at): (Instant, Moment)) {
        let reading = ClockReading::new(os, at.frame());
        let rebuild = match self.map.as_mut() {
            None => true,
            Some(map) => {
                if map.rate() != at.rate() {
                    map.set_rate(at.rate());
                }
                map.observe(reading) == Observed::Rejected
            }
        };
        if rebuild {
            self.map = Some(ClockMap::new(at.rate(), reading));
        }
    }
}
