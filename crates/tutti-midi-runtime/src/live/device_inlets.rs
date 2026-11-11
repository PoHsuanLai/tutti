//! [`DeviceInlets`]: one event inlet per hardware MIDI device, and the
//! producer end ([`DeviceInput`]) the device's thread stamps and pushes through
//! (doc 017 stage E5).
//!
//! A device's thread learns when an event arrived from the OS, as an
//! [`Instant`]. The graph wants a frame. The thread places each arrival itself,
//! so the audio thread converts nothing: it holds a [`ClockMap`] fed the
//! engine's block-start readings ([`BlockClockReader`], one per render call:
//! the OS time the block began and the graph frame it began at) and pushes the
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
//! is the map's: within the bound doc 017 section 5 states
//! ([`ClockMap`]). An event is late, and counted so
//! ([`InletCounts::late`](tutti_graph::InletCounts::late)), only when the
//! thread's push comes more than the lead after its arrival.
//!
//! # Slots
//!
//! The node has a fixed number of event inputs, so a device takes one of
//! `slots` ([`DeviceInlets::open`]), and gives it back
//! ([`close`](DeviceInlets::close)). The graph is edited by its owner, on the
//! control thread; a device connects from any thread. The two meet in a list
//! of changes ([`take_changes`](DeviceInlets::take_changes)): the owner adds
//! the inlet's node to the graph and wires it to the slot's input, or takes it
//! out.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tutti_graph::{BlockClockReader, EventInletNode, EventInletWriter, InletCounts};
use tutti_rt::{ArcLend, Reader};
use tutti_types::{At, Moment};
use tutti_ump::Ump;

use crate::clock_map::{ClockMap, ClockReading, Observed};

/// Which of a [`DeviceInlets`]' slots a device holds: its index, and which
/// device to take the slot it is, so a stale slot (its device closed, the slot
/// reused) names nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceSlot {
    index: usize,
    generation: u64,
}

impl DeviceSlot {
    /// The slot's index: the event input of the ingest node it feeds.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }
}

/// Every slot is taken: [`DeviceInlets::open`] has none to give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoFreeSlot {
    /// How many slots there are.
    pub slots: usize,
}

impl std::fmt::Display for NoFreeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all {} MIDI device slots are taken", self.slots)
    }
}

impl std::error::Error for NoFreeSlot {}

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
    /// The device closed `slot`: take its inlet out.
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
    changes: Vec<InletChange>,
}

/// The hardware devices' event inlets, shared between the threads that connect
/// devices and the owner of the graph. See the module docs.
pub struct DeviceInlets {
    state: Mutex<State>,
    capacity: usize,
    /// Nanoseconds: what each device's thread adds to an arrival's frame.
    lead: Arc<AtomicU64>,
    /// The engine's block clock, lent to each device's thread. `None` until
    /// the owner has an engine ([`set_clock`](Self::set_clock)).
    clock: Arc<ArcLend<Option<BlockClockReader>>>,
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
    /// `slots` device slots (at least one), each device's inlet holding
    /// `capacity` events, and `lead` the time each arrival is placed after its
    /// mapped frame (see the module docs).
    ///
    /// # Panics
    ///
    /// If `capacity` is 0 (an inlet holds at least an event).
    #[must_use]
    pub fn new(slots: usize, capacity: usize, lead: Duration) -> Self {
        assert!(capacity > 0, "an inlet holds at least one event");
        Self {
            state: Mutex::new(State {
                slots: vec![None; slots.max(1)],
                ..State::default()
            }),
            capacity,
            lead: Arc::new(AtomicU64::new(nanos(lead))),
            clock: Arc::new(ArcLend::new(None)),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock guards nothing inconsistent: every change is whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many device slots there are.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.state().slots.len()
    }

    /// Sets the engine's block clock the devices' threads place arrivals
    /// with (`None`: there is none, and events are pushed for the next
    /// block). Takes effect on each thread's next event. Control side.
    pub fn set_clock(&self, clock: Option<BlockClockReader>) {
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
        let (node, writer) = EventInletNode::pair(self.capacity);
        state
            .changes
            .push(InletChange::Attach { slot: index, node });
        Ok(DeviceInput {
            slot: DeviceSlot { index, generation },
            counts: writer.counts(),
            writer,
            clock: self.clock.reader(),
            lead: Arc::clone(&self.lead),
            seen: None,
            map: None,
        })
    }

    /// Frees `slot` for the next device and queues the inlet's removal.
    /// `false`, changing nothing, for a slot not held (already closed,
    /// whether or not another device took it since). The device's thread may
    /// still push: its events go to a ring nothing reads. An inlet still
    /// waiting in [`take_changes`](Self::take_changes) is dropped with its
    /// attach.
    pub fn close(&self, slot: DeviceSlot) -> bool {
        let mut state = self.state();
        match state.slots.get_mut(slot.index) {
            Some(held) if *held == Some(slot.generation) => {
                *held = None;
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
                    None => state.changes.push(InletChange::Detach { slot: slot.index }),
                }
                true
            }
            _ => false,
        }
    }

    /// The changes since the last call, in order, for the graph's owner to
    /// apply. Control side.
    pub fn take_changes(&self) -> Vec<InletChange> {
        std::mem::take(&mut self.state().changes)
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// One device's end of its inlet: stamp each arrival with a frame and push it.
/// Owned by the device's thread: `Send`, not `Clone`, and
/// [`push`](Self::push) takes `&mut self`. Nothing here blocks, locks or
/// allocates.
pub struct DeviceInput {
    slot: DeviceSlot,
    writer: EventInletWriter,
    counts: InletCounts,
    clock: Reader<Option<BlockClockReader>>,
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

impl DeviceInput {
    /// The slot this device holds: what [`DeviceInlets::close`] takes.
    #[must_use]
    pub fn slot(&self) -> DeviceSlot {
        self.slot
    }

    /// The inlet's counts, readable from any thread: events the ring had no
    /// room for ([`lost`](InletCounts::lost)), the waiting list had no room
    /// for, the port refused, and events that came late.
    #[must_use]
    pub fn counts(&self) -> InletCounts {
        self.counts.clone()
    }

    /// Pushes `ump`, which the OS delivered at `arrival`, for the frame the
    /// block clock puts that time on, plus the lead; or for the next block
    /// while no reading exists (no engine yet, or `arrival` before frame 0).
    /// `false` when the ring was full: the event is dropped and counted in
    /// [`lost`](InletCounts::lost). Lock-free and allocation-free, so safe to
    /// call from a driver callback.
    #[inline]
    pub fn push(&mut self, ump: Ump, arrival: Instant) -> bool {
        let sent = match self.moment_of(arrival) {
            Some(at) => self.writer.push_at(at, ump),
            None => self.writer.push(At::NextBlock, ump),
        };
        sent.is_ok()
    }

    /// The moment `arrival` plus the lead is, by the latest block reading.
    fn moment_of(&mut self, arrival: Instant) -> Option<Moment> {
        let reading = {
            let clock = self.clock.lend();
            clock.as_ref()?.read()?
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
                (map.rate() != at.rate() && !map.set_rate(at.rate()))
                    || map.observe(reading) == Observed::Rejected
            }
        };
        if rebuild {
            self.map = ClockMap::new(at.rate(), reading);
        }
    }
}
