//! A plugin node's parameter changes: one slot per parameter, sized at bind,
//! and **changes only**.
//!
//! # Capacity
//!
//! The plugin's parameter list is read once, when the client is loaded
//! (`PluginClient::new`, which blocks on the plugin-server anyway), and
//! [`ParamDelivery::new`] sizes everything from it on the control thread:
//! one slot per parameter, plus [`SPARE_SLOTS`] for addresses the list did
//! not name (a plugin that grows a parameter later, or a list the server
//! could not give). A chunk's [`ParameterChanges`] has room for a queue per
//! slot, so every parameter can move in the same chunk; this replaces the 16
//! queues a chunk held before, past which a parameter's points were dropped
//! with nothing counted. The bridge's buffers are reserved with the same
//! room (`PluginBridge::reserve_param_changes`).
//!
//! **Points.** Each queue has room for [`points_for`] the chunk: one point
//! per 32 frames and 4 more ([`ParameterQueue::chunk_room`]), sized in
//! [`prepare`](ParamDelivery::prepare) once the chunk length is known
//! (`ParameterChanges::reserved`, here and in the bridge's buffers). The
//! queues are kept, emptied, from chunk to chunk, so their point storage is
//! never freed or rebuilt on the audio thread. A source denser than that
//! has the queue's last point replaced (the chunk still ends on the latest
//! value), counted ([`ParamDeliveryStats::points_merged`]).
//!
//! An address past every slot is the one bound left. Its points are refused
//! and counted ([`ParamDeliveryStats::points_dropped`]), never silently.
//!
//! # Changes only
//!
//! Each slot remembers the last value the node **delivered** for its
//! parameter (handed to the bridge with its chunk). A point whose value
//! equals it, or equals the parameter's last point already in the chunk, is
//! not sent. A plugin is told a value once; what the host repeats every
//! block (an automation node samples every lane at every stride point, a
//! lane re-sends its segment each block) does not reach it again.
//!
//! **The rule for edits made in the plugin's own editor.** The comparison is
//! against what the node sent, never against the plugin's current value. So:
//!
//! - an unchanged host value is never re-sent, and cannot overwrite a value
//!   the user set in the plugin's editor (or a preset the plugin loaded);
//! - a host value that moves (automation running, a modulation moving) is
//!   sent, and replaces whatever the plugin holds, as a DAW's automation in
//!   read mode does.
//!
//! The plugin's own reports (`ParameterChanged`) deliberately do not update
//! the slot. If they did, an editor edit to 0.8 would make the host's
//! unchanged 0.5 look like a change, and the node would overwrite the edit.
//!
//! # The record's lifetime
//!
//! The record means **what the host last delivered to this plugin
//! instance**, so nothing clears it:
//!
//! - **a reset** (a stop or a seek) keeps it: clearing it would re-send
//!   every held host value on every stop and clobber the editor's edits;
//! - **a re-prepare** keeps it (the chunk in flight is owed, not lost);
//! - **a state or preset load** keeps it: the loaded values stay until the
//!   host's value actually moves;
//! - **a fork** starts from it: the live node's record is read just before
//!   the live state is saved, and the fork's delivery is seeded with it
//!   ([`seed`](ParamDelivery::seed), `fork.rs`), so a fork loaded with that
//!   state sends what the live one would, and an export renders what live
//!   playback plays (as a DAW's does). A value delivered between the read
//!   and the save is in the state and is sent to the fork once more,
//!   harmlessly.
//!
//! Design doc 014 §9, requirement 4. (Doc 014's PR 4, which makes an editor
//! edit write the lane's knob, may revisit this.)
//!
//! # Delivered late, never lost
//!
//! A value counts as delivered only once its chunk is handed over
//! ([`handed_off`](ParamDelivery::handed_off)). When the bridge has no free
//! buffer for it (the plugin-server is that far behind) or refuses the
//! block, each parameter's latest value is **owed**, sent on the first frame
//! of the next chunk, and the chunk counted
//! ([`ParamDeliveryStats::chunks_deferred`]). Without this a dropped change
//! would never be re-sent, because the host's value no longer changes.
//!
//! Nothing here allocates on the audio thread: the slots and the staged
//! list are reserved in [`new`](ParamDelivery::new), the chunk's queues and
//! their points in [`prepare`](ParamDelivery::prepare), and a learned
//! address is inserted within that capacity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use super::param_automation_source::MAX_POINTS;
use crate::protocol::{Normalized, ParamAddress, ParameterChanges, ParameterQueue};

/// Slots beyond the plugin's parameter list, for addresses it did not name:
/// learned on the audio thread, within the reserved capacity.
pub(crate) const SPARE_SLOTS: usize = 64;

/// A slot with no queue in the chunk being filled.
const NO_QUEUE: u32 = u32::MAX;

/// A slot with no entry in the [`DeliveredRecord`] (a learned address).
const NO_MIRROR: u32 = u32::MAX;

/// The points one parameter can carry in a chunk of `chunk` frames: one per
/// `ParameterQueue::POINT_SPACING` (32) frames and
/// `ParameterQueue::CORNER_POINTS` (4) more, and never fewer
/// than a queue holds inline ([`ParameterQueue::chunk_room`], which the
/// format hosts reserve for too).
///
/// Not yet design doc 014 §9 requirement 6, which asks for
/// `ceil(chunk / CONTROL_STEP) + 1 + C` points (`C = 256`, a lane's corners
/// in one block) and, past the room, grid points dropped before corners;
/// here a full queue has its last point replaced instead
/// ([`ParamDeliveryStats::points_merged`]). Doc 014's PR 2 changes both.
pub(crate) fn points_for(chunk: usize) -> usize {
    ParameterQueue::chunk_room(chunk).max(MAX_POINTS)
}

/// What the node's parameter delivery has done since the plugin was loaded.
/// Read with [`PluginControls::param_delivery`](super::PluginControls::param_delivery).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParamDeliveryStats {
    /// Parameter points handed to the plugin-server: changes only, so an
    /// automation lane holding still adds nothing here.
    pub points_sent: u64,
    /// Chunks whose parameter changes could not go with them (every
    /// parameter-change buffer was in flight, or the block was refused):
    /// their values were sent on the next chunk's first frame instead.
    pub chunks_deferred: u64,
    /// Points for a parameter the node has no slot for (an address the
    /// plugin's list did not name, once the spare slots are taken): dropped.
    pub points_dropped: u64,
    /// Points that replaced the last point of a parameter's queue because
    /// the queue was full: a chunk carries up to one point per 32 frames per
    /// parameter, plus a few for corners, and a source sending denser than
    /// that loses the points between (the chunk still ends on the latest
    /// value).
    pub points_merged: u64,
}

/// The counters behind [`ParamDeliveryStats`], shared between the node and
/// its controls, and the [`DeliveredRecord`]. Written on the audio thread,
/// read anywhere.
#[derive(Debug, Default)]
pub(crate) struct DeliveryCounters {
    sent: AtomicU64,
    deferred: AtomicU64,
    dropped: AtomicU64,
    merged: AtomicU64,
    /// Set once, at bind ([`ParamDelivery::new`]).
    record: OnceLock<Arc<DeliveredRecord>>,
}

/// What the node last delivered for each parameter the plugin listed at
/// load, readable off the audio thread: what a fork is seeded with
/// (`fork.rs`), so a fresh instance carrying the live one's state does not
/// receive a value the live one was never re-sent (module docs, "The
/// record's lifetime"). One `f64`'s bits per listed parameter, NaN for
/// never delivered. Addresses learned into spare slots are not mirrored.
///
/// **Ordering.** A value is stored with `Release` when its chunk is handed
/// over, which is after the chunk's `Process` command was pushed to the
/// bridge; [`snapshot`](Self::snapshot) loads with `Acquire`. So a
/// snapshot that sees a value also sees that push, and a `SaveState`
/// pushed after the snapshot is queued behind the command that delivered
/// it: every value the record shows is in the state saved after it.
/// (`Relaxed` would not give the push's visibility formally.)
#[derive(Debug)]
pub(crate) struct DeliveredRecord {
    /// Sorted, the same order as the node's listed slots.
    addresses: Box<[ParamAddress]>,
    values: Box<[AtomicU64]>,
}

impl DeliveredRecord {
    fn new(addresses: &[ParamAddress]) -> Self {
        Self {
            addresses: addresses.into(),
            values: addresses
                .iter()
                .map(|_| AtomicU64::new(f64::NAN.to_bits()))
                .collect(),
        }
    }

    /// Each listed parameter the node has delivered, with its value.
    fn snapshot(&self) -> Vec<(ParamAddress, f64)> {
        self.addresses
            .iter()
            .zip(self.values.iter())
            .filter_map(|(&a, v)| {
                let v = f64::from_bits(v.load(Ordering::Acquire));
                (!v.is_nan()).then_some((a, v))
            })
            .collect()
    }
}

impl DeliveryCounters {
    /// What the node has delivered so far, per listed parameter: empty
    /// before bind. Control thread: allocates.
    pub(crate) fn delivered(&self) -> Vec<(ParamAddress, f64)> {
        self.record.get().map(|r| r.snapshot()).unwrap_or_default()
    }

    pub(crate) fn snapshot(&self) -> ParamDeliveryStats {
        ParamDeliveryStats {
            points_sent: self.sent.load(Ordering::Relaxed),
            chunks_deferred: self.deferred.load(Ordering::Relaxed),
            points_dropped: self.dropped.load(Ordering::Relaxed),
            points_merged: self.merged.load(Ordering::Relaxed),
        }
    }
}

/// One parameter's delivery state.
#[derive(Clone, Copy, Debug)]
struct Slot {
    address: ParamAddress,
    /// The last value handed over with a chunk, normalized; `None` until one is.
    sent: Option<f64>,
    /// The last value placed in the chunk being filled (or, while `owed`,
    /// the value owed).
    staged: f64,
    /// Its queue's index in the chunk being filled, or [`NO_QUEUE`].
    queue: u32,
    /// A value whose chunk did not go: placed on the next chunk's first frame.
    owed: bool,
    /// Its index in the [`DeliveredRecord`], or [`NO_MIRROR`].
    mirror: u32,
}

impl Slot {
    fn new(address: ParamAddress) -> Self {
        Self {
            address,
            sent: None,
            staged: 0.0,
            queue: NO_QUEUE,
            owed: false,
            mirror: NO_MIRROR,
        }
    }
}

/// A plugin node's per-parameter delivery. See the module docs.
pub(crate) struct ParamDelivery {
    /// Sorted by address; capacity reserved in [`new`](Self::new).
    slots: Vec<Slot>,
    /// The slots with a queue in `changes`, in queue order.
    staged: Vec<u32>,
    /// The chunk being filled: room for a queue per slot, each with room for
    /// the chunk's point budget ([`prepare`](Self::prepare)).
    changes: ParameterChanges,
    /// How many slots are `owed`.
    owed: usize,
    /// Points added to the chunk being filled.
    chunk_points: u64,
    /// Each queue's point room ([`prepare`](Self::prepare)).
    points: usize,
    counters: Arc<DeliveryCounters>,
    /// The mirror of `sent` for the listed slots.
    record: Arc<DeliveredRecord>,
}

impl std::fmt::Debug for ParamDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParamDelivery")
            .field("slots", &self.slots.len())
            .field("room", &self.room())
            .finish_non_exhaustive()
    }
}

impl ParamDelivery {
    /// Slots for `params` (the plugin's parameter list) and [`SPARE_SLOTS`]
    /// more, and a chunk with room for a queue in each. Control thread:
    /// allocates.
    pub(crate) fn new(
        params: impl IntoIterator<Item = ParamAddress>,
        counters: Arc<DeliveryCounters>,
    ) -> Self {
        let mut addresses: Vec<ParamAddress> = params.into_iter().collect();
        addresses.sort_unstable();
        addresses.dedup();
        let room = addresses.len() + SPARE_SLOTS;
        let record = Arc::new(DeliveredRecord::new(&addresses));
        // Once per controls: a client binds once. A second `new` on the same
        // counters (tests) keeps the first record for the controls' readers
        // and mirrors into its own.
        let _ = counters.record.set(Arc::clone(&record));
        let mut slots = Vec::with_capacity(room);
        slots.extend(addresses.into_iter().enumerate().map(|(i, a)| Slot {
            mirror: u32::try_from(i).unwrap_or(NO_MIRROR),
            ..Slot::new(a)
        }));
        Self {
            slots,
            staged: Vec::with_capacity(room),
            changes: ParameterChanges::reserved(room, MAX_POINTS),
            owed: 0,
            chunk_points: 0,
            points: MAX_POINTS,
            counters,
            record,
        }
    }

    /// Take `delivered` (a live instance's [`DeliveredRecord`], read before
    /// its state was saved) as already delivered to this instance: a fork
    /// loaded with that state then sends a parameter only once the host's
    /// value differs from it, as the live one does. Control thread, before
    /// the node runs.
    pub(crate) fn seed(&mut self, delivered: &[(ParamAddress, f64)]) {
        for &(address, value) in delivered {
            if let Some(i) = self.slot(address) {
                let slot = &mut self.slots[i];
                slot.sent = Some(value);
                slot.staged = value;
                if let Some(m) = self.record.values.get(slot.mirror as usize) {
                    m.store(value.to_bits(), Ordering::Release);
                }
            }
        }
    }

    /// Size the chunk's point room for chunks of `chunk` frames
    /// ([`points_for`]), dropping the chunk being filled (its values are
    /// owed to the next). Returns the points per queue, for the bridge's
    /// buffers. Control thread: allocates.
    pub(crate) fn prepare(&mut self, chunk: usize) -> usize {
        self.abandon();
        self.points = points_for(chunk);
        self.changes = ParameterChanges::reserved(self.room(), self.points);
        self.points
    }

    /// Each queue's point room: what the bridge's buffers are reserved with.
    #[cfg(test)]
    pub(crate) fn points(&self) -> usize {
        self.points
    }

    /// The parameter queues a chunk has room for: what the bridge's buffers
    /// are reserved with.
    pub(crate) fn room(&self) -> usize {
        self.slots.capacity()
    }

    /// The slot for `address`, learning it into a spare slot when it is new
    /// and one is free. Allocation-free: a learned slot is inserted within
    /// the reserved capacity.
    fn slot(&mut self, address: ParamAddress) -> Option<usize> {
        match self.slots.binary_search_by(|s| s.address.cmp(&address)) {
            Ok(i) => Some(i),
            Err(i) if self.slots.len() < self.slots.capacity() => {
                self.slots.insert(i, Slot::new(address));
                // The slots after it moved up one.
                for s in &mut self.staged {
                    if *s as usize >= i {
                        *s += 1;
                    }
                }
                Some(i)
            }
            Err(_) => None,
        }
    }

    /// Whether `address` has a slot (learning it if there is room); counts
    /// a refusal as one dropped point.
    pub(crate) fn knows(&mut self, address: ParamAddress) -> bool {
        let known = self.slot(address).is_some();
        if !known {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
        known
    }

    /// A point for `address` at chunk frame `offset`, unless the value is
    /// the one the plugin already has from this node (see the module docs).
    /// A full queue (its point budget, [`points_for`] the chunk) has its
    /// last point replaced, counted as merged: the latest value wins, so a
    /// chunk spanning several blocks ends exact.
    /// `false` when `address` has no slot (counted). Allocation-free.
    pub(crate) fn point(&mut self, address: ParamAddress, offset: i32, value: f32) -> bool {
        let Some(i) = self.slot(address) else {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.place(i, offset, Normalized::new(f64::from(value)).get());
        true
    }

    /// `value` (already normalized) for slot `i` at chunk frame `offset`.
    fn place(&mut self, i: usize, offset: i32, value: f64) {
        let slot = &mut self.slots[i];
        if slot.queue == NO_QUEUE {
            if slot.sent == Some(value) {
                // Unchanged: the plugin has it from this node already.
                return;
            }
            let queue = u32::try_from(self.changes.queues.len()).unwrap_or(NO_QUEUE);
            if !self.changes.open_queue(slot.address) {
                // Impossible by construction: a queue per slot, and the
                // chunk has room for every slot. Refused rather than grown.
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            slot.queue = queue;
            self.staged.push(u32::try_from(i).unwrap_or(u32::MAX));
        } else if self.changes.queues[slot.queue as usize]
            .points
            .last()
            .is_some_and(|p| p.value.get() == value)
        {
            // The chunk already ends on it.
            return;
        }
        let queue = &mut self.changes.queues[slot.queue as usize];
        if !queue.has_room() {
            // Full: the latest value replaces the last point, never grows
            // the queue (which would allocate).
            queue.points.pop();
            self.counters.merged.fetch_add(1, Ordering::Relaxed);
        } else {
            self.chunk_points += 1;
        }
        queue.add_point(offset, value);
        slot.staged = value;
    }

    /// A chunk begins: what earlier chunks owe goes on its first frame.
    pub(crate) fn begin(&mut self) {
        if self.owed == 0 {
            return;
        }
        for i in 0..self.slots.len() {
            if self.slots[i].owed {
                self.slots[i].owed = false;
                let value = self.slots[i].staged;
                self.place(i, 0, value);
            }
        }
        self.owed = 0;
    }

    /// The chunk's changes, for the bridge to take: it swaps in a free buffer
    /// of the same room when they go.
    pub(crate) fn changes(&mut self) -> &mut ParameterChanges {
        &mut self.changes
    }

    /// The chunk was submitted: its changes went with it (`delivered`), and
    /// each parameter's value is now the plugin's; or they did not, and
    /// each one's latest value is owed to the next chunk (counted as a
    /// deferred chunk). Allocation-free.
    pub(crate) fn handed_off(&mut self, delivered: bool) {
        if delivered {
            self.counters
                .sent
                .fetch_add(self.chunk_points, Ordering::Relaxed);
        } else if !self.staged.is_empty() {
            self.counters.deferred.fetch_add(1, Ordering::Relaxed);
        }
        self.settle(delivered);
    }

    /// The chunk being filled will never be submitted (a reset or a
    /// re-prepare drops it): its values are owed to the next chunk.
    pub(crate) fn abandon(&mut self) {
        self.settle(false);
    }

    fn settle(&mut self, delivered: bool) {
        for &i in &self.staged {
            let slot = &mut self.slots[i as usize];
            slot.queue = NO_QUEUE;
            if delivered {
                slot.sent = Some(slot.staged);
                if let Some(m) = self.record.values.get(slot.mirror as usize) {
                    m.store(slot.staged.to_bits(), Ordering::Release);
                }
            } else if !slot.owed {
                slot.owed = true;
                self.owed += 1;
            }
        }
        self.staged.clear();
        self.chunk_points = 0;
        // After a hand-off this is the free buffer swapped in, already
        // empty; otherwise it still holds the chunk. Keeps its capacity.
        self.changes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ParamId;

    fn op(id: u32) -> ParamAddress {
        ParamAddress::Opaque(ParamId::new(id))
    }

    fn delivery(n: u32) -> (ParamDelivery, Arc<DeliveryCounters>) {
        let counters = Arc::new(DeliveryCounters::default());
        (
            ParamDelivery::new((0..n).map(op), Arc::clone(&counters)),
            counters,
        )
    }

    /// `(address, [(offset, value)])` of the chunk being filled.
    fn chunk(d: &ParamDelivery) -> Vec<(ParamAddress, Vec<(i32, f64)>)> {
        d.changes
            .queues
            .iter()
            .map(|q| {
                (
                    q.param_id,
                    q.points
                        .iter()
                        .map(|p| (p.sample_offset, p.value.get()))
                        .collect(),
                )
            })
            .collect()
    }

    /// Hand the chunk off as the bridge does: swapped for an empty buffer of
    /// the same room.
    fn hand_off(d: &mut ParamDelivery, delivered: bool) -> ParameterChanges {
        let mut fresh = ParameterChanges::reserved(d.room(), d.points());
        if delivered {
            std::mem::swap(d.changes(), &mut fresh);
        }
        d.handed_off(delivered);
        fresh
    }

    /// **Every parameter can move in one chunk**: 200 parameters, a queue
    /// each, none dropped, and nothing past the reserved room.
    ///
    /// Mutation (run): the chunk sized at 16 queues (the old cap,
    /// `ParameterChanges::reserved(16, MAX_POINTS)` in `new`) → 184 points
    /// refused → fails.
    #[test]
    fn two_hundred_parameters_fit_one_chunk() {
        let (mut d, counters) = delivery(200);
        let room = d.changes.capacity();
        d.begin();
        for id in 0..200 {
            assert!(d.point(op(id), 0, id as f32 / 400.0));
        }
        assert_eq!(chunk(&d).len(), 200);
        assert_eq!(d.changes.capacity(), room, "the chunk never grew");
        assert_eq!(counters.snapshot().points_dropped, 0);
    }

    /// **An unchanged value is not sent again.** A value sent with one chunk
    /// is skipped in the next; a new value goes; a repeat inside a chunk is
    /// skipped. Counted in `points_sent`.
    ///
    /// Mutation (run): drop the `sent == Some(value)` check → the second
    /// chunk carries the unchanged value → fails. Mutation (run): drop the
    /// in-chunk check → the repeat is a second point → fails.
    #[test]
    fn an_unchanged_value_is_not_sent_again() {
        let (mut d, counters) = delivery(4);
        d.begin();
        d.point(op(1), 0, 0.5);
        d.point(op(1), 8, 0.5);
        assert_eq!(chunk(&d), vec![(op(1), vec![(0, 0.5)])]);
        hand_off(&mut d, true);
        assert_eq!(counters.snapshot().points_sent, 1);

        d.begin();
        d.point(op(1), 0, 0.5);
        assert!(chunk(&d).is_empty(), "unchanged: nothing sent");
        d.point(op(1), 16, 0.25);
        assert_eq!(chunk(&d), vec![(op(1), vec![(16, 0.25)])]);
        hand_off(&mut d, true);
        assert_eq!(counters.snapshot().points_sent, 2);
    }

    /// **A value is delivered only when its chunk goes.** A chunk the bridge
    /// could not take owes each parameter's latest value to the next chunk's
    /// first frame, counted once; a value it did take is not owed.
    ///
    /// Mutation (run): commit `sent` whatever the hand-off says
    /// (`settle(true)` in `handed_off`) → the owed value is never placed →
    /// fails. Mutation (run): `begin` not placing owed values → fails.
    #[test]
    fn a_chunk_that_did_not_go_is_owed_to_the_next() {
        let (mut d, counters) = delivery(4);
        d.begin();
        d.point(op(2), 5, 0.125);
        d.point(op(2), 9, 0.375);
        hand_off(&mut d, false);
        assert_eq!(counters.snapshot().chunks_deferred, 1);
        assert_eq!(counters.snapshot().points_sent, 0);

        d.begin();
        assert_eq!(chunk(&d), vec![(op(2), vec![(0, 0.375)])]);
        // The host still sending 0.375 adds nothing: it is in the chunk.
        d.point(op(2), 4, 0.375);
        assert_eq!(chunk(&d), vec![(op(2), vec![(0, 0.375)])]);
        hand_off(&mut d, true);

        d.begin();
        d.point(op(2), 0, 0.375);
        assert!(chunk(&d).is_empty(), "delivered once, not again");
    }

    /// **The record is what the host last delivered to the instance**: a
    /// reset and a re-prepare keep it, so a held value is not re-sent after
    /// either; it is published for a fork, and a fork seeded with it sends
    /// that value only once the host's value moves.
    ///
    /// Mutations (run), each fails: `abandon` clearing the record (every
    /// slot's `sent = None`) → the held 0.5 is re-sent after the reset;
    /// `settle` not mirroring the delivered value → `delivered()` is empty;
    /// `seed` doing nothing → the fork re-sends 0.5.
    #[test]
    fn the_record_survives_reset_and_reprepare_and_seeds_a_fork() {
        let (mut d, counters) = delivery(4);
        d.prepare(256);
        d.begin();
        d.point(op(1), 0, 0.5);
        hand_off(&mut d, true);

        d.abandon(); // a reset
        d.prepare(512); // a re-prepare
        d.begin();
        d.point(op(1), 0, 0.5);
        assert!(chunk(&d).is_empty(), "held: not re-sent");

        let delivered = counters.delivered();
        assert_eq!(delivered, vec![(op(1), 0.5)]);

        let (mut fork, _) = delivery(4);
        fork.seed(&delivered);
        fork.prepare(512);
        fork.begin();
        fork.point(op(1), 0, 0.5);
        assert!(
            chunk(&fork).is_empty(),
            "the fork holds what live delivered"
        );
        fork.point(op(1), 8, 0.25);
        assert_eq!(chunk(&fork), vec![(op(1), vec![(8, 0.25)])]);
    }

    /// **An address the list did not name is learned into a spare slot**;
    /// past the spares it is refused and counted, never grown into.
    ///
    /// Mutation (run): grow past the capacity (no `len < capacity` check in
    /// `slot`) → the slots reallocate → fails. Mutation (run): not counting
    /// the refusal → fails.
    #[test]
    fn unknown_addresses_take_spare_slots_then_are_counted() {
        let (mut d, counters) = delivery(2);
        let room = d.slots.capacity();
        d.begin();
        for id in 1000..1000 + SPARE_SLOTS as u32 {
            assert!(d.point(op(id), 0, 0.5), "spare slot for {id}");
        }
        assert!(!d.point(op(5000), 0, 0.5));
        assert_eq!(d.slots.capacity(), room, "never grown");
        assert_eq!(counters.snapshot().points_dropped, 1);
        // A learned slot shifts the ones after it; the chunk's queues still
        // belong to their own parameters.
        hand_off(&mut d, true);
        d.begin();
        d.point(op(1), 0, 0.75);
        assert_eq!(chunk(&d), vec![(op(1), vec![(0, 0.75)])]);
    }

    /// Learning a slot below one already staged in the chunk keeps the
    /// staged slot's queue its own.
    ///
    /// Mutation (run): not shifting `staged` in `slot` → the commit marks
    /// the wrong slot sent, and 0.625 on id 50 is skipped as unchanged → fails.
    #[test]
    fn learning_a_slot_keeps_staged_queues_in_place() {
        let counters = Arc::new(DeliveryCounters::default());
        let mut d = ParamDelivery::new([op(50)], counters);
        d.begin();
        d.point(op(50), 0, 0.25);
        d.point(op(10), 0, 0.625);
        hand_off(&mut d, true);
        d.begin();
        d.point(op(50), 0, 0.625);
        d.point(op(10), 0, 0.25);
        assert_eq!(
            chunk(&d),
            vec![(op(50), vec![(0, 0.625)]), (op(10), vec![(0, 0.25)])]
        );
    }

    /// **A parameter carries a point every 32 frames of its chunk, plus
    /// corners, with nothing allocated**: prepared for a 2048-frame chunk,
    /// each of 200 parameters takes 68 points (2048 / 32 + 4) in one chunk,
    /// chunk after chunk, inside the no-alloc gate. Past the budget the last
    /// point is replaced, so the queue never grows and the chunk ends on the
    /// latest value, and each replacement is counted.
    ///
    /// Mutations (run), each fails: `points_for` ignoring the chunk (the old
    /// ten, `MAX_POINTS`) → 58 points a queue merged; pushing past the budget
    /// (no `has_room` check in `place`) → the queue allocates inside the
    /// gate → aborts; the replacement not counted → `points_merged` is 0.
    #[test]
    fn a_queue_carries_a_point_every_32_frames_of_its_chunk() {
        const CHUNK: usize = 2048;
        let budget = CHUNK / ParameterQueue::POINT_SPACING + ParameterQueue::CORNER_POINTS;
        let (mut d, counters) = delivery(200);
        assert_eq!(d.prepare(CHUNK), budget);
        let mut spare = ParameterChanges::reserved(d.room(), d.points());
        let chunk = |d: &mut ParamDelivery, spare: &mut ParameterChanges| {
            d.begin();
            for k in 0..budget {
                for id in 0..200 {
                    // Exact eighths, distinct from point to point.
                    let v = ((k + id as usize) % 8) as f32 / 8.0;
                    d.point(op(id), (k * 30) as i32, v);
                }
            }
            let q = &d.changes.queues[0];
            assert_eq!(q.points.len(), budget);
            std::mem::swap(d.changes(), spare);
            d.handed_off(true);
            spare.clear();
        };
        chunk(&mut d, &mut spare);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                chunk(&mut d, &mut spare);
            }
        });
        assert_eq!(counters.snapshot().points_merged, 0);

        // Past the budget: the last point replaced, counted.
        d.begin();
        for k in 0..budget + 3 {
            d.point(op(0), k as i32, (k % 2) as f32 / 4.0 + 0.5);
        }
        let q = &d.changes.queues[0];
        assert_eq!(q.points.len(), budget);
        assert_eq!(
            q.points.last().map(|p| p.sample_offset),
            Some((budget + 2) as i32)
        );
        assert_eq!(counters.snapshot().points_merged, 3);
    }
}
