//! A plugin node's parameter changes: one slot per parameter, sized at bind,
//! and **changes only**.
//!
//! Both plugin nodes deliver through it: the out-of-process node, whose
//! chunk is the pipeline's and is handed to the bridge, and the in-process
//! VST2 node (`format::vst2_in_process`), whose chunk is the block and is
//! handed off by setting each queue's last point on the plugin before it
//! processes the block. What follows says "the bridge"; for the VST2 node
//! read "the plugin instance", and "no free buffer" as "the instance was
//! locked".
//!
//! # Capacity
//!
//! The plugin's parameter list is read once, when the client is loaded
//! (`PluginClient::new`, which blocks on the plugin-server anyway), and
//! [`ParamDelivery::new`] sizes everything from it on the control thread:
//! one slot per parameter, plus [`ParameterChanges::SPARE_QUEUES`] for addresses the list did
//! not name (a plugin that grows a parameter later, or a list the server
//! could not give). A chunk's [`ParameterChanges`] has room for a queue per
//! slot, so every parameter can move in the same chunk; this replaces the 16
//! queues a chunk held before, past which a parameter's points were dropped
//! with nothing counted. The bridge's buffers are reserved with the same
//! room (`PluginBridge::reserve_param_changes`).
//!
//! **Points.** Each queue has room for [`points_for`] the chunk: one point
//! per 32 frames (a control lane's grid), one for the chunk's last frame,
//! and 256 more (a lane's corners in one block):
//! [`ParameterQueue::chunk_room`], design doc 014 §9 requirement 6. It is
//! sized in [`prepare`](ParamDelivery::prepare) once the chunk length is
//! known (`ParameterChanges::reserved_bridged`, here and in the bridge's
//! buffers). That room is logical: each queue owns
//! `ParameterChanges::FLOOR_POINTS` points, and a queue that needs more
//! takes a block of the full room from the chunk's shared pool
//! (`ParameterChanges::make_room`), so the memory follows the points a
//! chunk carries (any `ParameterChanges::POOLED_QUEUES` parameters at full
//! density at once), not the parameter count. The queues and the pool are
//! kept, emptied, from chunk to chunk, so their point storage is never
//! freed or rebuilt on the audio thread.
//!
//! A queue is **full** when it is at its room, or at its floor with the
//! pool spent (more than `POOLED_QUEUES` parameters past their floor in one
//! chunk). Then **grid points go before corners**: a point is a grid point
//! when the play clock's count on its frame is a multiple of 32 (the caller
//! says so; the plugin node reads its own `Cx::play_at`). A grid point that
//! finds its queue full is dropped; any other point evicts the queue's
//! earliest grid point. Both are counted
//! ([`ParamDeliveryStats::grid_points_dropped`]). Only a queue full of
//! corners keeps its latest value in place of its latest point (the one
//! full-queue policy, `PointBuf::replace_latest`), counted
//! ([`ParamDeliveryStats::points_merged`]); the chunk still ends on the
//! latest value, because a parameter's last point in a chunk is a corner
//! (the chunk's last frame, an arrival or a ramp).
//!
//! An address past every slot is the one bound left. Its points are refused
//! and counted ([`ParamDeliveryStats::points_dropped`]), never silently.
//!
//! **A closed list** ([`closed`](ParamDelivery::closed), the VST2 node's)
//! has no spare slots: a VST2 plugin's parameters are the indices below the
//! count it declared at load, and the host refuses any other index, so a
//! spare slot learning one would only stage a point the plugin never takes.
//! Such a point is refused here and counted dropped instead.
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
//!   ([`seed`](ParamDelivery::seed), `fork.rs` and
//!   `vst2_in_process/fork.rs`), so a fork loaded with that state sends
//!   what the live one would, and an export renders what live playback
//!   plays (as a DAW's does). A value delivered between the read and the
//!   save is in the state and is sent to the fork once more, harmlessly.
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

/// A slot with no queue in the chunk being filled.
const NO_QUEUE: u32 = u32::MAX;

/// A slot with no entry in the [`DeliveredRecord`] (a learned address).
const NO_MIRROR: u32 = u32::MAX;

/// The points one parameter can carry in a chunk of `chunk` frames:
/// [`ParameterQueue::chunk_room`] (`ceil(chunk / 32) + 1 + 256`, design doc
/// 014 §9 requirement 6), which the format hosts reserve for too.
pub(crate) fn points_for(chunk: usize) -> usize {
    ParameterQueue::chunk_room(chunk)
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
    /// plugin's list did not name, once the spare slots are taken; on an
    /// in-process VST2 node, any index past the count it declared): dropped.
    pub points_dropped: u64,
    /// Points that replaced the last point of a parameter's queue because
    /// the queue was full of corners: a chunk carries a point per 32 frames
    /// per parameter, its last frame and 256 corners, and a source with more
    /// corners than that loses the points between (the chunk still ends on
    /// the latest value). A queue past its own few points draws on the
    /// chunk's shared point pool (`ParameterChanges::POOLED_QUEUES`
    /// parameters at full room); with the pool spent it is full at its
    /// floor, and counted here the same way once it holds no grid point.
    pub points_merged: u64,
    /// Grid points (on a frame whose play count is a multiple of 32) dropped
    /// from a full queue, to make room for a corner or because the grid
    /// point itself found no room (design doc 014 §9 requirement 6).
    pub grid_points_dropped: u64,
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
    grid_dropped: AtomicU64,
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
            grid_points_dropped: self.grid_dropped.load(Ordering::Relaxed),
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

/// Which points of each queue in the chunk being filled are grid points
/// (design doc 014 §9 requirement 6), a bit a point, stored the way the
/// chunk stores its points: each queue's own bits for its floor, and a
/// block's bits for a queue that took one of the pool's blocks. So the bits
/// follow the points a chunk carries, as the points do, not the parameter
/// count times the room: about 12 bytes a queue and
/// `POOLED_QUEUES × ceil((room + 1) / 64)` words besides (about 40 KB for
/// 2 064 queues at a 4096-frame chunk). Reserved in
/// [`ParamDelivery::prepare`]; allocation-free after.
#[derive(Debug)]
struct GridBits {
    words: Vec<u64>,
    /// Each queue's bits: its first word in `words`, and how many.
    span: Vec<(u32, u32)>,
    /// Words of a queue's own bits, and of a block's.
    own: usize,
    block: usize,
    /// The blocks' bits, and the next one not yet taken this chunk.
    blocks: usize,
    next: usize,
}

impl GridBits {
    /// Bits for `queues` queues of a chunk room of `room`, laid out as
    /// `ParameterChanges::reserved_bridged(queues, room)` lays out its
    /// points. Allocates.
    fn new(queues: usize, room: usize) -> Self {
        let (floor, blocks) = ParameterChanges::point_layout(queues, room);
        let own = floor.div_ceil(64);
        let block = room.saturating_add(1).div_ceil(64);
        Self {
            words: vec![0; queues * own + blocks * block],
            span: vec![(0, 0); queues],
            own,
            block,
            blocks,
            next: 0,
        }
    }

    /// Queue `q` was opened, empty: its own bits, cleared. The first queue
    /// opened starts a chunk, and takes the blocks' bits back.
    fn open(&mut self, q: usize) {
        if q == 0 {
            self.next = 0;
        }
        let Some(span) = self.span.get_mut(q) else {
            return;
        };
        let start = q * self.own;
        *span = (
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(self.own).unwrap_or(0),
        );
        if let Some(w) = self.words.get_mut(start..start + self.own) {
            w.fill(0);
        }
    }

    /// Queue `q`'s points have storage for `capacity` now: when that
    /// outgrew its bits (it took a pool block), its bits move to a block's.
    /// Past the blocks' bits (a buffer of another layout swapped in), the
    /// bits stay as they are, and a point past them reads as a corner.
    fn fits(&mut self, q: usize, capacity: usize) {
        let Some(&(start, words)) = self.span.get(q) else {
            return;
        };
        let (start, words) = (start as usize, words as usize);
        let need = capacity.div_ceil(64);
        if need <= words || self.next >= self.blocks || need > self.block {
            return;
        }
        let to = self.span.len() * self.own + self.next * self.block;
        self.next += 1;
        self.words.copy_within(start..start + words, to);
        self.words[to + words..to + self.block].fill(0);
        self.span[q] = (
            u32::try_from(to).unwrap_or(u32::MAX),
            u32::try_from(self.block).unwrap_or(0),
        );
    }

    /// Queue `q`'s bits, if it has any.
    fn of(&self, q: usize) -> &[u64] {
        match self.span.get(q) {
            Some(&(start, words)) => {
                let start = start as usize;
                self.words.get(start..start + words as usize).unwrap_or(&[])
            }
            None => &[],
        }
    }

    fn of_mut(&mut self, q: usize) -> &mut [u64] {
        match self.span.get(q) {
            Some(&(start, words)) => {
                let start = start as usize;
                self.words
                    .get_mut(start..start + words as usize)
                    .unwrap_or(&mut [])
            }
            None => &mut [],
        }
    }

    /// Marks point `k` of queue `q` a grid point or a corner.
    fn set(&mut self, q: usize, k: usize, grid: bool) {
        if let Some(w) = self.of_mut(q).get_mut(k / 64) {
            *w = (*w & !(1 << (k % 64))) | (u64::from(grid) << (k % 64));
        }
    }

    /// The earliest grid point among queue `q`'s first `len` points.
    fn first(&self, q: usize, len: usize) -> Option<usize> {
        let bits = self.of(q);
        (0..len).find(|&k| bits.get(k / 64).is_some_and(|w| w >> (k % 64) & 1 == 1))
    }

    /// Point `k` of queue `q`'s `len` was removed: the bits after it move
    /// down one, as the points did.
    fn remove(&mut self, q: usize, k: usize, len: usize) {
        let bits = self.of_mut(q);
        let get = |bits: &[u64], j: usize| bits.get(j / 64).is_some_and(|w| w >> (j % 64) & 1 == 1);
        for j in k..len.saturating_sub(1) {
            let next = get(bits, j + 1);
            if let Some(w) = bits.get_mut(j / 64) {
                *w = (*w & !(1 << (j % 64))) | (u64::from(next) << (j % 64));
            }
        }
        if let Some(w) = bits.get_mut(len.saturating_sub(1) / 64) {
            *w &= !(1 << (len.saturating_sub(1) % 64));
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
    /// The slots there is room for: the listed ones and the spares.
    room: usize,
    /// Points added to the chunk being filled.
    chunk_points: u64,
    /// Each queue's point room ([`prepare`](Self::prepare)).
    points: usize,
    /// Which points of each queue in the chunk being filled are grid
    /// points.
    grid: GridBits,
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
    /// Slots for `params` (the plugin's parameter list) and [`ParameterChanges::SPARE_QUEUES`]
    /// more, and a chunk with room for a queue in each. Control thread:
    /// allocates.
    pub(crate) fn new(
        params: impl IntoIterator<Item = ParamAddress>,
        counters: Arc<DeliveryCounters>,
    ) -> Self {
        Self::with_spares(params, counters, ParameterChanges::SPARE_QUEUES)
    }

    /// Slots for `params` only: an address the list does not name is
    /// refused and counted dropped, never learned (module docs, "A closed
    /// list"). Control thread: allocates.
    #[cfg(feature = "vst2")]
    pub(crate) fn closed(
        params: impl IntoIterator<Item = ParamAddress>,
        counters: Arc<DeliveryCounters>,
    ) -> Self {
        Self::with_spares(params, counters, 0)
    }

    fn with_spares(
        params: impl IntoIterator<Item = ParamAddress>,
        counters: Arc<DeliveryCounters>,
        spares: usize,
    ) -> Self {
        let mut addresses: Vec<ParamAddress> = params.into_iter().collect();
        addresses.sort_unstable();
        addresses.dedup();
        let room = addresses.len() + spares;
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
            changes: ParameterChanges::reserved_bridged(room, MAX_POINTS),
            owed: 0,
            room,
            chunk_points: 0,
            points: MAX_POINTS,
            grid: GridBits::new(room, MAX_POINTS),
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
        self.changes = ParameterChanges::reserved_bridged(self.room(), self.points);
        self.grid = GridBits::new(self.room(), self.points);
        self.points
    }

    /// The counters and record this delivery writes, for the node's
    /// controls.
    #[cfg(feature = "vst2")]
    pub(crate) fn counters(&self) -> Arc<DeliveryCounters> {
        Arc::clone(&self.counters)
    }

    /// Each queue's point room: what the bridge's buffers are reserved with.
    #[cfg(test)]
    pub(crate) fn points(&self) -> usize {
        self.points
    }

    /// The parameter queues a chunk has room for: what the bridge's buffers
    /// are reserved with.
    pub(crate) fn room(&self) -> usize {
        self.room
    }

    /// The chunk being filled, to read: the VST2 node sets it on the plugin
    /// itself.
    #[cfg(feature = "vst2")]
    pub(crate) fn chunk(&self) -> &ParameterChanges {
        &self.changes
    }

    /// `n` points of a chunk already handed off that the plugin refused:
    /// counted dropped. Allocation-free.
    #[cfg(feature = "vst2")]
    pub(crate) fn refused(&self, n: u64) {
        if n > 0 {
            self.counters.dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The slot for `address`, learning it into a spare slot when it is new
    /// and one is free. Allocation-free: a learned slot is inserted within
    /// the reserved capacity.
    fn slot(&mut self, address: ParamAddress) -> Option<usize> {
        match self.slots.binary_search_by(|s| s.address.cmp(&address)) {
            Ok(i) => Some(i),
            Err(i) if self.slots.len() < self.room => {
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
    /// A corner: when the queue is full (its room, or its floor with the
    /// chunk's shared pool spent) it evicts a grid point
    /// ([`grid_point`](Self::grid_point)), and only a queue of corners has
    /// its latest point replaced, counted as merged: the latest value wins,
    /// so a chunk spanning several blocks ends exact.
    /// `false` when `address` has no slot (counted). Allocation-free.
    pub(crate) fn point(&mut self, address: ParamAddress, offset: i32, value: f32) -> bool {
        self.point_kind(address, offset, value, false)
    }

    /// [`point`](Self::point) for a grid point (a frame whose play count is
    /// a multiple of 32): dropped first when its queue is full, and counted.
    pub(crate) fn grid_point(&mut self, address: ParamAddress, offset: i32, value: f32) -> bool {
        self.point_kind(address, offset, value, true)
    }

    fn point_kind(&mut self, address: ParamAddress, offset: i32, value: f32, grid: bool) -> bool {
        let Some(i) = self.slot(address) else {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.place(i, offset, Normalized::new(f64::from(value)).get(), grid);
        true
    }

    /// `value` (already normalized) for slot `i` at chunk frame `offset`,
    /// a grid point or not.
    fn place(&mut self, i: usize, offset: i32, value: f64, grid: bool) {
        let q = match self.slots[i].queue {
            NO_QUEUE => {
                let slot = &self.slots[i];
                if slot.sent == Some(value) {
                    // Unchanged: the plugin has it from this node already.
                    return;
                }
                let queue = u32::try_from(self.changes.queues.len()).unwrap_or(NO_QUEUE);
                if !self.changes.open_queue(slot.address) {
                    // Impossible by construction: a queue per slot, and the
                    // chunk has room for every slot. Refused rather than
                    // grown.
                    self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                self.slots[i].queue = queue;
                self.staged.push(u32::try_from(i).unwrap_or(u32::MAX));
                self.grid.open(queue as usize);
                queue as usize
            }
            q => {
                let q = q as usize;
                if self.changes.queues[q]
                    .points
                    .last()
                    .is_some_and(|p| p.value.get() == value)
                {
                    // The chunk already ends on it.
                    return;
                }
                q
            }
        };
        // Room of its own, or a block from the chunk's shared pool
        // (`make_room`); never grown, which would allocate.
        if !self.changes.make_room(q) {
            // Full: at its room, or at its floor with the pool spent. Grid
            // points go first.
            if grid {
                self.counters.grid_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let len = self.changes.queues[q].points.len();
            match self.grid.first(q, len) {
                Some(k) => {
                    self.changes.queues[q].points.remove(k);
                    self.grid.remove(q, k, len);
                    self.counters.grid_dropped.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    // A queue of corners: the latest value replaces the
                    // latest point (`push_point`'s full-queue policy).
                    self.counters.merged.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            self.chunk_points += 1;
        }
        self.grid.fits(q, self.changes.queues[q].points.capacity());
        // Room now (its own, a block, or a grid point's), or a queue of
        // corners keeping its latest value (counted above).
        if self.changes.push_point(q, offset, value) {
            let k = self.changes.queues[q].points.len() - 1;
            self.grid.set(q, k, grid);
        }
        self.slots[i].staged = value;
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
                self.place(i, 0, value, false);
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
        let mut fresh = ParameterChanges::reserved_bridged(d.room(), d.points());
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
    /// `ParameterChanges::reserved_bridged(16, MAX_POINTS)` in `new`) → 184 points
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
        for id in 1000..1000 + ParameterChanges::SPARE_QUEUES as u32 {
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
    /// **A closed list learns nothing**: an index the VST2 plugin did not
    /// declare is refused and counted dropped, while a declared one goes.
    ///
    /// Mutation (run): `closed` built with `ParameterChanges::SPARE_QUEUES` spares, as `new`
    /// is → the undeclared index is learned and staged, nothing dropped →
    /// fails.
    #[cfg(feature = "vst2")]
    #[test]
    fn a_closed_list_refuses_an_undeclared_index() {
        let counters = Arc::new(DeliveryCounters::default());
        let mut d = ParamDelivery::closed((0..4).map(ParamAddress::Index), Arc::clone(&counters));
        d.begin();
        assert!(!d.point(ParamAddress::Index(4), 0, 0.5));
        assert!(!d.point(ParamAddress::Index(200), 0, 0.5));
        assert!(d.point(ParamAddress::Index(3), 0, 0.5));
        assert_eq!(chunk(&d), vec![(ParamAddress::Index(3), vec![(0, 0.5)])]);
        assert_eq!(counters.snapshot().points_dropped, 2);
    }

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
    /// each of 200 parameters takes 321 points (2048 / 32 + 1 + 256, doc
    /// 014 §9 requirement 6) in one chunk,
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
        let budget = CHUNK / ParameterQueue::POINT_SPACING + 1 + ParameterQueue::CORNER_POINTS;
        let (mut d, counters) = delivery(200);
        assert_eq!(d.prepare(CHUNK), budget);
        let mut spare = ParameterChanges::reserved_bridged(d.room(), d.points());
        let chunk = |d: &mut ParamDelivery, spare: &mut ParameterChanges| {
            d.begin();
            for k in 0..budget {
                for id in 0..200 {
                    // Exact eighths, distinct from point to point and
                    // from the last chunk's last point (321 points: a cycle
                    // of 8 would end each chunk where the next starts, and
                    // changes-only would skip that point).
                    let v = ((k + id as usize) % 7) as f32 / 8.0;
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

    /// **Past a queue's room, grid points go before corners** (doc 014 §9
    /// requirement 6): a full queue drops a grid point offered to it, and a
    /// corner evicts the queue's earliest grid point, each counted; only a
    /// queue of corners has its last point replaced (merged).
    ///
    /// Mutations (run), each fails: a grid point treated as a corner (the
    /// `if grid` drop removed) → it evicts a grid point and ends the queue;
    /// a corner never evicting (always the merge) → the first grid point
    /// stays and the count is merged, not dropped; the evicted point the
    /// last grid point rather than the first → the queue's first offset is
    /// not 1; the grid bits not shifted down past an evicted point → the
    /// bits no longer name the queue's grid points, and the next corner
    /// evicts a corner.
    #[test]
    fn grid_points_are_dropped_before_corners() {
        let (mut d, counters) = delivery(1);
        let room = d.prepare(64);
        assert_eq!(room, 2 + 1 + 256);
        let grids = 100;
        // Alternating values, so changes-only keeps every point.
        let v = |k: usize| if k.is_multiple_of(2) { 0.25 } else { 0.75 };
        d.begin();
        for k in 0..room {
            if k < grids {
                d.grid_point(op(0), k as i32, v(k));
            } else {
                d.point(op(0), k as i32, v(k));
            }
        }
        let offsets = |d: &ParamDelivery| -> Vec<i32> {
            d.changes.queues[0]
                .points
                .iter()
                .map(|p| p.sample_offset)
                .collect()
        };
        assert_eq!(offsets(&d).len(), room);
        // A grid point at full: dropped.
        d.grid_point(op(0), 1000, v(room));
        assert_eq!(offsets(&d).len(), room);
        assert_eq!(offsets(&d).last(), Some(&(room as i32 - 1)));
        assert_eq!(counters.snapshot().grid_points_dropped, 1);
        // A corner at full: the earliest grid point (offset 0) goes.
        d.point(op(0), 2000, v(room));
        let o = offsets(&d);
        assert_eq!((o[0], o.last().copied()), (1, Some(2000)));
        assert_eq!(counters.snapshot().grid_points_dropped, 2);
        assert_eq!(counters.snapshot().points_merged, 0);
        // Every other grid point goes before a corner is merged.
        for k in 1..grids {
            d.point(op(0), 2000 + k as i32, v(room + k));
        }
        assert_eq!(counters.snapshot().grid_points_dropped, 1 + grids as u64);
        assert_eq!(offsets(&d)[0], grids as i32, "only corners left");
        d.point(op(0), 3000, v(room + grids));
        assert_eq!(counters.snapshot().points_merged, 1);
        assert_eq!(offsets(&d).len(), room);
    }

    /// **The chunk's point storage is the pool budget, not parameters ×
    /// room**: a delivery for 2 000 parameters (and its 64 spare slots)
    /// prepared for 512- and 4096-frame chunks holds
    /// `ParameterChanges::point_budget` points (a floor of
    /// `FLOOR_POINTS + 1` a slot and a shared pool of `POOLED_QUEUES`
    /// blocks of `room + 1`, the extra point kept for the bridge's carried
    /// value), under a third of a full carried room per slot. (Reworded with
    /// `PointBuf`: the chunk was `point_budget`, under a quarter of `room`,
    /// and the carried value grew a queue on the bridge thread.)
    ///
    /// Mutation (run): `prepare` reserving the old layout (every slot a
    /// full room: `ParameterChanges::point_layout` always `(full, 0)`) →
    /// the storage is `slots × (room + 1)` → fails.
    #[test]
    fn the_chunk_holds_the_pool_budget_not_params_times_room() {
        let (mut d, _) = delivery(2_000);
        for chunk in [512usize, 4096] {
            let room = d.prepare(chunk);
            assert_eq!(room, points_for(chunk));
            let slots = 2_000 + ParameterChanges::SPARE_QUEUES;
            let budget = slots * (ParameterChanges::FLOOR_POINTS + 1)
                + ParameterChanges::POOLED_QUEUES * (room + 1);
            assert_eq!(ParameterChanges::point_budget(slots, room), budget);
            assert_eq!(d.changes.point_storage(), budget, "at {chunk}");
            assert!(3 * budget < slots * (room + 1), "at {chunk}");
        }
    }

    /// **Up to the pool budget every point goes, without allocating; past
    /// it each point is merged into its parameter's last, counted.** 1 000
    /// parameters prepared for a 1024-frame chunk (a room of 289; 36 before
    /// doc 014 PR 2b): the first `POOLED_QUEUES` carry their full room,
    /// every other one its floor (a
    /// step and a second value), points for different parameters arriving
    /// interleaved, chunk after chunk inside the no-alloc gate, with nothing
    /// merged. Then one more parameter needs a third point with the pool
    /// spent: it replaces that parameter's last point (the chunk ends on the
    /// latest value), counted once.
    ///
    /// Mutations (run), each fails: `push_point` without `make_room` (the
    /// queue's own room only, `has_room`) → the moving parameters merge
    /// past their floor; the merge not counted → `points_merged` is 0 after
    /// the overflow; the host's chunk built without its carry slot
    /// (`reserved`) → the floor queues take a third point, and the
    /// moving parameters' per-chunk counts are off.
    #[test]
    fn up_to_the_pool_budget_nothing_merges_and_past_it_merges_counted() {
        const PARAMS: u32 = 1_000;
        const CHUNK: usize = 1024;
        let (mut d, counters) = delivery(PARAMS);
        let room = d.prepare(CHUNK);
        let moving = ParameterChanges::POOLED_QUEUES as u32;
        let mut spare = ParameterChanges::reserved_bridged(d.room(), d.points());
        let chunk = |d: &mut ParamDelivery, spare: &mut ParameterChanges, phase: usize| {
            d.begin();
            for k in 0..room {
                for id in 0..PARAMS {
                    if k >= ParameterChanges::FLOOR_POINTS && id >= moving {
                        continue;
                    }
                    // Distinct from point to point, and a chunk's first
                    // from the last chunk's last (else it is unchanged and
                    // not sent).
                    let v = ((k + id as usize + 4 * phase) % 8) as f32 / 8.0;
                    d.point(op(id), (k * 28) as i32, v);
                }
            }
            for (i, q) in d.changes.queues.iter().enumerate() {
                let want = if (i as u32) < moving {
                    room
                } else {
                    ParameterChanges::FLOOR_POINTS
                };
                assert_eq!(q.points.len(), want);
                assert!(q
                    .points
                    .windows(2)
                    .all(|w| w[0].sample_offset < w[1].sample_offset));
            }
            std::mem::swap(d.changes(), spare);
            d.handed_off(true);
            spare.clear();
        };
        chunk(&mut d, &mut spare, 0);
        assert_no_alloc::assert_no_alloc(|| {
            for phase in 1..4 {
                chunk(&mut d, &mut spare, phase);
            }
        });
        assert_eq!(counters.snapshot().points_merged, 0);

        // The pool spent, one more parameter takes a third point.
        // Odd sixteenths: none was sent before, so every point goes.
        d.begin();
        for k in 0..3 {
            for id in 0..=moving {
                d.point(op(id), k, (2 * k + 1) as f32 / 16.0);
            }
        }
        let last = d.changes.get_queue(op(moving)).expect("queued");
        assert_eq!(last.points.len(), ParameterChanges::FLOOR_POINTS);
        assert_eq!(last.points.last().map(|p| p.value.get()), Some(5.0 / 16.0));
        assert_eq!(last.points.last().map(|p| p.sample_offset), Some(2));
        assert_eq!(counters.snapshot().points_merged, 1);
    }

    /// One chunk of [`more_lanes_than_the_pool`]'s shape for `lanes` lanes
    /// of a delivery prepared for 512-frame chunks: in time order, frame by
    /// frame and lane by lane, a grid point on every 32nd frame, a corner 7
    /// frames after each, and a corner on the chunk's last frame (a lane's
    /// shape: its grid, its breakpoints, its chunk edge). Each lane's
    /// values step through sevenths of a cycle shifted by `phase`, so every
    /// point is a change (from its predecessor and from the last chunk's
    /// last). Writes each lane's latest value into `latest` (one a lane).
    fn lane_chunk(d: &mut ParamDelivery, latest: &mut [f32], phase: usize) {
        let lanes = latest.len() as u32;
        let mut k = 0;
        d.begin();
        for f in 0..512i32 {
            let grid = f % 32 == 0;
            if !(grid || f % 32 == 7 || f == 511) {
                continue;
            }
            for id in 0..lanes {
                let v = ((k + id as usize + 3 * phase) % 7 + 1) as f32 / 8.0;
                if grid {
                    d.grid_point(op(id), f, v);
                } else {
                    d.point(op(id), f, v);
                }
                latest[id as usize] = v;
            }
            k += 1;
        }
    }

    /// **More moving lanes than the pool has blocks: each parameter still
    /// ends the chunk on its latest value, grid points go before corners,
    /// every drop is counted, and nothing allocates** (design doc 014 §9
    /// requirement 6, past the pool). 300 lanes of a 600-parameter plugin
    /// move in one 512-frame chunk, each with 16 grid points, 16 corners and
    /// the chunk's last frame (33 points, inside the room of 273). The
    /// first 256 to need a third point take the pool's 256 blocks and keep
    /// all 33. The other 44 stay at their floor (two points, the third kept
    /// for the bridge's carried value): each drops the 15 grid points
    /// offered to its full queue, its first corner past the floor evicts
    /// its one grid point (frame 0), and its later corners, with no grid
    /// point left to evict, keep the latest value in the latest point's
    /// place: 16 grid drops and 15 merges a lane, and the queue ends
    /// `[7, 511]`, a corner and the latest value. Chunk after chunk inside
    /// the no-alloc gate, each handed off as the bridge does.
    ///
    /// Mutations (run), each fails: `place` without `make_room` (the
    /// queue's own storage only, `len + 1 < capacity`) → every lane is
    /// held at its floor and the first 256 lose their points; a grid point
    /// at a full queue handled like a corner (the `if grid` drop removed)
    /// → it evicts a grid point or takes the latest point's place, and the
    /// grid-drop count is not 16 a lane; a corner never evicting a
    /// grid point (the `first` match always `None`) → the spent lanes end
    /// `[0, 511]`, a grid point kept over a corner; the merge not counted
    /// → `points_merged` is 0.
    #[test]
    fn more_lanes_than_the_pool_end_on_their_latest_values_counted() {
        const PARAMS: u32 = 600;
        const LANES: u32 = 300;
        let (mut d, counters) = delivery(PARAMS);
        let room = d.prepare(512);
        assert_eq!(room, 273);
        let pooled = ParameterChanges::POOLED_QUEUES;
        assert_eq!(
            ParameterChanges::point_layout(d.room(), room),
            (ParameterChanges::FLOOR_POINTS + 1, pooled),
            "the delivery is pooled"
        );
        let spent = LANES as usize - pooled;
        let mut spare = ParameterChanges::reserved_bridged(d.room(), d.points());
        let check = |d: &mut ParamDelivery, latest: &[f32]| {
            for (id, &v) in latest.iter().enumerate() {
                let q = &d.changes.queues[id];
                assert_eq!(q.param_id, op(id as u32));
                let last = q.points.last().expect("a point");
                assert_eq!(
                    (last.sample_offset, last.value.get()),
                    (511, f64::from(v)),
                    "lane {id} ends on its latest value"
                );
                let offsets: Vec<i32> = q.points.iter().map(|p| p.sample_offset).collect();
                if id < pooled {
                    assert_eq!(offsets.len(), 33, "lane {id} holds a block");
                } else {
                    assert_eq!(offsets, [7, 511], "lane {id}: corners before grid points");
                }
            }
        };
        let mut latest = vec![0.0; LANES as usize];
        lane_chunk(&mut d, &mut latest, 0);
        check(&mut d, &latest);
        assert_eq!(counters.snapshot().grid_points_dropped, spent as u64 * 16);
        assert_eq!(counters.snapshot().points_merged, spent as u64 * 15);
        std::mem::swap(d.changes(), &mut spare);
        d.handed_off(true);
        spare.clear();
        for phase in 1..4 {
            assert_no_alloc::assert_no_alloc(|| lane_chunk(&mut d, &mut latest, phase));
            check(&mut d, &latest);
            assert_no_alloc::assert_no_alloc(|| {
                std::mem::swap(d.changes(), &mut spare);
                d.handed_off(true);
                spare.clear();
            });
        }
        let stats = counters.snapshot();
        assert_eq!(stats.grid_points_dropped, 4 * spent as u64 * 16);
        assert_eq!(stats.points_merged, 4 * spent as u64 * 15);
        assert_eq!(stats.points_dropped, 0);
    }

    /// **A queue that took a block keeps its grid marks**: its floor's
    /// marks move with its points into the block (`GridBits::fits`), so at
    /// its room a corner still evicts its *earliest* grid point, the one
    /// it took on its floor. A pooled delivery (600 parameters at a
    /// 512-frame chunk, room 273): one parameter takes a grid point on
    /// frame 0, a corner, then corners to its room (borrowing a block on
    /// the way); a corner past the room evicts the frame-0 grid point.
    ///
    /// Mutation (run): `fits` moving to a cleared block without copying the
    /// floor's marks → the frame-0 point reads as a corner, nothing is
    /// evicted, the corner is merged → fails.
    #[test]
    fn a_queue_keeps_its_grid_marks_when_it_takes_a_block() {
        let (mut d, counters) = delivery(600);
        let room = d.prepare(512);
        let v = |k: usize| if k.is_multiple_of(2) { 0.25 } else { 0.75 };
        d.begin();
        d.grid_point(op(0), 0, v(0));
        for k in 1..room {
            d.point(op(0), k as i32, v(k));
        }
        let points = |d: &ParamDelivery| d.changes.queues[0].points.len();
        assert_eq!(points(&d), room);
        d.point(op(0), 1000, v(room));
        assert_eq!(points(&d), room);
        assert_eq!(d.changes.queues[0].points[0].sample_offset, 1);
        let stats = counters.snapshot();
        assert_eq!((stats.grid_points_dropped, stats.points_merged), (1, 0));
    }

    /// **The pool-spent chunk reaches a CLAP plugin whole and in time
    /// order**, without allocating: the chunk of
    /// [`more_lanes_than_the_pool_end_on_their_latest_values_counted`],
    /// as the bridge hands it on, is flattened into a CLAP input list
    /// reserved as the plugin-server reserves it at activation
    /// (`ParameterChanges::point_budget` of the plugin's parameters at the
    /// chunk's room) inside the no-alloc gate. Every point is handed on,
    /// in time order, and every lane's last event is its latest value on
    /// the chunk's last frame.
    ///
    /// Mutation (run): the list reserved at the floor alone (`3 × params`,
    /// as if the pool's blocks were not counted) → the 256 full lanes'
    /// points past it are refused and counted → fails.
    #[cfg(feature = "clap")]
    #[test]
    fn a_pool_spent_chunk_reaches_a_clap_plugin_whole() {
        const PARAMS: u32 = 600;
        const LANES: u32 = 300;
        let (mut d, _) = delivery(PARAMS);
        let room = d.prepare(512);
        let mut latest = vec![0.0; LANES as usize];
        lane_chunk(&mut d, &mut latest, 0);
        let budget = ParameterChanges::point_budget(PARAMS as usize, room);
        let mut list = tutti_clap_host::InputEventList::new();
        list.reserve(budget);
        list.set_param_point_room(budget);
        let carried: usize = d.changes.queues.iter().map(|q| q.points.len()).sum();
        assert_no_alloc::assert_no_alloc(|| {
            list.add_param_changes(&d.changes, &[], false);
            list.clamp_times(512);
            list.sort_by_time();
        });
        assert_eq!(list.param_points_dropped(), 0);
        assert_eq!(list.events().len(), carried);
        let mut last = vec![None; LANES as usize];
        let mut time = 0;
        for e in list.events() {
            let tutti_clap_host::ClapEvent::ParamValue(p) = e else {
                panic!("only parameter events");
            };
            assert!(p.header.time >= time, "time order");
            time = p.header.time;
            last[p.param_id as usize] = Some((p.header.time, p.value));
        }
        for (id, &v) in latest.iter().enumerate() {
            assert_eq!(last[id], Some((511, f64::from(v))), "lane {id}");
        }
    }
}
