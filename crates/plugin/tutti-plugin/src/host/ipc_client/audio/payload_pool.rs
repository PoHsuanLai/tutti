//! RT-safe `ProcessPayload` recycling.
//!
//! A small bounded queue of pre-built payloads. The audio thread
//! [`acquire`](PayloadPool::acquire)s one per `Process`; the bridge thread
//! [`recycle`](PayloadPool::recycle)s it after marshalling the wire message.
//! When the queue is empty (cold start, brief stalls) `acquire` falls back
//! to a fresh `Box`; in steady state the pool keeps the audio path
//! allocation-free.
//!
//! A named primitive whose method names document the RT contract instead
//! of leaving it implicit in `unwrap_or_else(|| Box::new(...))`.

use super::messages::ProcessPayload;
use crate::protocol::ParameterChanges;
use crossbeam::queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// One payload per command the audio thread can have queued.
///
/// This is the SAME constraint as the command-queue depth, not an independent
/// tunable. A payload is recycled only once the bridge thread dequeues its
/// command, so the audio thread can hold up to `COMMAND_QUEUE_SIZE` of them at
/// once — which is what happens whenever the bridge thread falls behind, the
/// very case this pipeline exists to survive. Sizing the pool below that depth
/// does not bound memory: the payloads still exist, they are just allocated on
/// the audio thread instead, one ~9 KiB `Box::new` per block, exactly when the
/// plugin is already struggling.
///
/// It was 4 against a queue of 128, so blocks 5..=128 each allocated.
const POOL_CAPACITY: usize = super::channels::COMMAND_QUEUE_SIZE;

/// Parameter-change buffers in circulation between the audio thread and the
/// bridge thread: how many chunks carrying parameter changes can be on their
/// way to the plugin-server at once.
///
/// **Why a pool of its own, and why this small.** A chunk's parameter changes
/// are a [`ParameterChanges`] with room for a queue per parameter of the
/// plugin, reserved at prepare ([`PayloadPool::reserve_params`]). Carrying one
/// in each of the [`POOL_CAPACITY`] payloads would reserve that per payload:
/// 128 copies of a 2 000-parameter table, almost all of it idle, because a
/// payload carries changes only when a parameter moved. So the buffers
/// circulate separately: the audio thread swaps a full one out for a free one
/// when it submits a chunk with changes, and the bridge thread hands it back
/// once the chunk is on the wire.
///
/// Running out means this many chunks with changes are queued and the
/// bridge thread has sent none of them: the plugin-server is that far
/// behind. The plugin node then keeps the chunk's changes and sends each
/// parameter's latest value on the next chunk's first frame (late, never
/// lost), and counts the chunk
/// ([`ParamDeliveryStats::chunks_deferred`](crate::handles::ParamDeliveryStats)).
///
/// **What it costs.** Each buffer holds `queues` parameter queues (the
/// plugin's parameter count plus the node's 64 spare slots) and stores their
/// points in the one layout, built `ParameterChanges::reserved_bridged`: each
/// queue owns `ParameterChanges::FLOOR_POINTS + 1` (3) points of 16 bytes,
/// and a shared pool holds `ParameterChanges::POOLED_QUEUES` (256) blocks of
/// a queue's full room and one more, `ParameterQueue::chunk_room(chunk) + 1`
/// points; the one more point of each is kept for the carried value the
/// bridge prepends (`ParameterChanges::prepend_carried`), so nothing grows.
/// The room is design doc 014 §9 requirement 6's, `ceil(chunk / 32) + 1 +
/// 256`: 273 at a 512-frame chunk, 385 at 4096. A queue is 64 bytes besides
/// (its slot in the queue list and among the spares). With the node's own
/// buffer that is `(PARAM_BUFFERS + 1) × (queues × 112 + 256 × 16 × (room +
/// 1))` bytes per plugin (`ParameterChanges::point_budget` points, plus
/// about 14 KB a buffer of pool bookkeeping and a byte a queue): about
/// 16.31 MB for 2 000 parameters at a 4096-frame chunk (12.18 MB at 512),
/// and about 30.81 MB for a VST3 listing 16 384 (26.68 MB at 512). The pool
/// term does not grow with the parameter count; with every queue at the
/// full room the same figures would be about 118 MB and 939 MB.
///
/// The plugin-server reserves for the same points again, per instance, at
/// activation with its block size, for what such a buffer can arrive with
/// after the bridge prepends a skipped block's carried value to its queues
/// (`ParameterChanges::point_budget` of the plugin's parameters with a
/// room of `ParameterQueue::chunk_room(block)`: three points a parameter
/// and 256 blocks of the room and one):
/// - **CLAP** (`tutti-clap-host`'s `lifecycle.rs`): the input event list's
///   parameter room, that many events at 64 bytes each plus an 8-byte sort
///   key each: about 7.55 MB for 2 000 parameters at 4096 frames and
///   10.65 MB for 16 384 (5.48 MB and 8.59 MB at 512); and the output side
///   (the event list's parameter room, the same budget, and the reserved
///   output queues) about 8.51 MB and 12.89 MB (6.22 MB and 10.59 MB).
/// - **VST3** (`tutti-vst3-host`'s `instance.rs`, at most 16 384
///   parameters and the 64 spare queues): an input list storing its points
///   the same way, about 1.68 MB of points for 2 000 parameters and
///   2.37 MB for 16 384 at 4096 frames (1.22 MB and 1.91 MB at 512).
///
/// Design doc 014 §9 and §12 give the budget's derivation.
pub(super) const PARAM_BUFFERS: usize = 8;

/// The free list's capacity: twice the buffers in circulation, so a buffer
/// the audio thread hands back always fits (it never drops one, which would
/// free on that thread), while the bridge thread drops what comes back once
/// [`PARAM_BUFFERS`] are free.
const PARAM_SLOTS: usize = 2 * PARAM_BUFFERS;

/// A reservation's queues and point room as one word, each saturated at
/// `u32::MAX` (past any plugin's parameter count or a chunk's room).
fn shape(queues: usize, points: usize) -> u64 {
    let half = |n: usize| u64::from(u32::try_from(n).unwrap_or(u32::MAX));
    half(queues) << 32 | half(points)
}

/// Bounded pool of recycled `ProcessPayload` boxes, and the parameter-change
/// buffers that travel in them. Cheap to clone (one Arc each).
#[derive(Clone)]
pub(super) struct PayloadPool {
    recycle: Arc<ArrayQueue<Box<ProcessPayload>>>,
    /// Free parameter-change buffers, each of the reservation in
    /// `params_shape`. Empty until [`reserve_params`](Self::reserve_params).
    params: Arc<ArrayQueue<ParameterChanges>>,
    /// The queues and the point room the buffers were reserved with, one
    /// word ([`shape`]) so the bridge thread never reads one reservation's
    /// queues with another's room: a buffer coming back of any other shape
    /// (one reserved before a re-reserve) is dropped rather than recycled,
    /// on the bridge thread. 0 before a reservation.
    params_shape: Arc<AtomicU64>,
}

impl PayloadPool {
    /// Builds the pool **full**, on the control thread.
    ///
    /// Filling here rather than letting it warm up through `recycle` is the
    /// point: an empty pool pushes its allocations onto the audio thread, and
    /// the first blocks after a plugin loads are the least forgiving moment for
    /// one. This runs once, off the RT path, where a few hundred KiB of boxes
    /// costs nothing.
    pub(super) fn new() -> Self {
        let recycle = ArrayQueue::new(POOL_CAPACITY);
        for _ in 0..POOL_CAPACITY {
            // Cannot fail: the queue was just created with this capacity.
            let _ = recycle.push(Box::new(ProcessPayload::empty()));
        }
        Self {
            recycle: Arc::new(recycle),
            params: Arc::new(ArrayQueue::new(PARAM_SLOTS)),
            params_shape: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Fill the parameter-change buffers, each with room for `queues` queues
    /// (the plugin's parameter count, and some) of up to `points` points each
    /// (the node's per-chunk point room, stored as a shared pool,
    /// `ParameterChanges::reserved`). Control
    /// thread: allocates. Buffers reserved before are dropped here, or on
    /// the bridge thread when one comes back from flight. **Any other shape
    /// is dropped, a different point room as much as fewer queues:** the
    /// node marks its grid points (`GridBits`) in the layout of its own
    /// room, so a buffer of another room in its hands would misread them,
    /// and could hold more points than the server's lists reserve.
    pub(super) fn reserve_params(&self, queues: usize, points: usize) {
        self.params_shape
            .store(shape(queues, points), Ordering::Release);
        while self.params.pop().is_some() {}
        for _ in 0..PARAM_BUFFERS {
            // Cannot fail: drained above, and the list holds twice this many
            // (a buffer the bridge thread returns meanwhile still fits).
            let _ = self
                .params
                .push(ParameterChanges::reserved_bridged(queues, points));
        }
    }

    /// Audio thread: a free parameter-change buffer, or `None` when every one
    /// is in flight. Allocation-free.
    #[inline]
    pub(super) fn take_params(&self) -> Option<ParameterChanges> {
        self.params.pop()
    }

    /// Bridge thread: return a parameter-change buffer, cleared, to the free
    /// list, once its contents are on the wire. **This is where a buffer is
    /// freed**, never on the audio thread: one not of the current
    /// reservation's queues and point room (the empty placeholder a payload
    /// carries when its chunk had no changes, or a buffer reserved before a
    /// re-reservation), any before a reservation, and any past [`PARAM_BUFFERS`] free ones (a
    /// re-reservation refilled the list while older buffers were in flight)
    /// are dropped here.
    #[inline]
    pub(super) fn return_params(&self, mut changes: ParameterChanges) {
        let reserved = self.params_shape.load(Ordering::Acquire);
        if reserved == 0
            || shape(changes.capacity(), changes.point_room()) != reserved
            || self.params.len() >= PARAM_BUFFERS
        {
            return;
        }
        changes.clear();
        let _ = self.params.push(changes);
    }

    /// Audio thread: hand back a buffer taken with
    /// [`take_params`](Self::take_params) that its block could not use (the
    /// command queue refused the block). **Never frees**: it is kept whatever
    /// its capacity (one reserved before a re-reservation goes round once
    /// more and is dropped by the bridge thread on its way back), and the
    /// list has room for it, being twice the buffers the bridge thread keeps.
    /// Only the empty placeholder, which owns no memory, is not kept.
    /// Allocation-free.
    #[inline]
    pub(super) fn return_params_rt(&self, mut changes: ParameterChanges) {
        if changes.capacity() == 0 {
            return;
        }
        changes.clear();
        if let Err(changes) = self.params.push(changes) {
            // Unreachable. The list holds `2 * PARAM_BUFFERS`. The bridge
            // thread pushes only while fewer than `PARAM_BUFFERS` are free,
            // and `reserve_params` refills to `PARAM_BUFFERS` after draining,
            // so between them they leave the list at most `PARAM_BUFFERS`
            // plus the one push a bridge return racing a refill can add. The
            // audio thread returns only a buffer it popped from this list in
            // the same `submit`, so it never adds to that total. A full list
            // here would mean that bound is broken: the buffer is leaked
            // rather than freed on the audio thread.
            core::mem::forget(changes);
        }
    }

    /// Audio-thread: hand back a payload to fill in.
    ///
    /// The fallback allocates, which on this thread is a defect rather than a
    /// slow path — it is kept only because returning `Option` would push the
    /// same decision onto every caller, and dropping a block is worse than a
    /// rare malloc. The pool is sized and pre-filled so the fallback is
    /// unreachable in practice: it needs more than [`POOL_CAPACITY`] payloads
    /// outstanding, and the command queue cannot hold that many.
    #[inline]
    pub(super) fn acquire(&self) -> Box<ProcessPayload> {
        self.recycle
            .pop()
            .unwrap_or_else(|| Box::new(ProcessPayload::empty()))
    }

    /// Bridge-thread: return a payload after the wire message has been built.
    /// Drops silently if the pool is full.
    #[inline]
    pub(super) fn recycle(&self, payload: Box<ProcessPayload>) {
        let _ = self.recycle.push(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::node::{DeliveryCounters, ParamDelivery};
    use crate::protocol::{ParamAddress, ParamId, ParameterQueue};

    /// **The bridge's buffers hold the pool budget, not parameters × room**:
    /// for 2 000 parameters (and the node's 64 spare slots) at 512- and
    /// 4096-frame chunks, the [`PARAM_BUFFERS`] free buffers together hold
    /// `PARAM_BUFFERS × ParameterChanges::point_budget` points (a
    /// floor of `FLOOR_POINTS + 1` a queue and the shared pool's blocks of
    /// `room + 1`), under a third of a full carried room (`room + 1`) per
    /// queue. (Reworded with `PointBuf`: the buffers were `point_budget`,
    /// under a quarter of `room`, and the carried value's point grew a queue
    /// on the bridge thread; it is reserved now.)
    ///
    /// Mutation (run): `reserve_params` building each buffer with every
    /// queue at full room (`ParameterChanges::point_layout` always
    /// `(full, 0)`) → `queues × (room + 1)` a buffer → fails. (Reworded with
    /// the one layout: a second mutation, the plain layout, no longer
    /// exists.)
    #[test]
    fn the_buffers_hold_the_pool_budget_not_params_times_room() {
        let queues = 2_000 + 64;
        for chunk in [512usize, 4096] {
            let room = ParameterQueue::chunk_room(chunk);
            let pool = PayloadPool::new();
            pool.reserve_params(queues, room);
            let mut storage = 0;
            while let Some(buffer) = pool.take_params() {
                storage += buffer.point_storage();
            }
            let budget = queues * (ParameterChanges::FLOOR_POINTS + 1)
                + ParameterChanges::POOLED_QUEUES * (room + 1);
            assert_eq!(ParameterChanges::point_budget(queues, room), budget);
            assert_eq!(storage, PARAM_BUFFERS * budget, "at {chunk}");
            assert!(3 * budget < queues * (room + 1), "at {chunk}");
        }
    }

    /// **The audio thread never drops a parameter-change buffer; the bridge
    /// thread drops the surplus.** With every buffer free and one more
    /// coming back (a re-reservation refilled the list while an older one
    /// was in flight), the bridge thread's return drops it; a buffer the
    /// audio thread took and hands back is kept even then, so its memory is
    /// never freed on the audio thread. (A drop is observed as the free
    /// list's length: an assert-no-alloc gate needs the test binary's global
    /// allocator, which this unit-test binary does not install.)
    ///
    /// Mutations (run), each fails: the bridge return without the
    /// `len() >= PARAM_BUFFERS` check → the surplus is kept, the list holds
    /// one more; `return_params_rt` applying the bridge's rule (its body
    /// `self.return_params(changes)`) → the taken buffer is dropped, the
    /// list one short.
    #[test]
    fn the_bridge_drops_surplus_buffers_and_the_audio_thread_none() {
        let pool = PayloadPool::new();
        pool.reserve_params(4, 10);
        assert_eq!(pool.params.len(), PARAM_BUFFERS);

        // A surplus buffer reaches the bridge thread: dropped there.
        pool.return_params(ParameterChanges::reserved_bridged(4, 10));
        assert_eq!(
            pool.params.len(),
            PARAM_BUFFERS,
            "the bridge drops the surplus"
        );

        // The audio thread takes one; the bridge returns another meanwhile.
        let taken = pool.take_params().expect("a free buffer");
        pool.return_params(ParameterChanges::reserved_bridged(4, 10));
        assert_eq!(pool.params.len(), PARAM_BUFFERS);
        // The block is refused: the audio thread hands its buffer back.
        pool.return_params_rt(taken);
        assert_eq!(
            pool.params.len(),
            PARAM_BUFFERS + 1,
            "the audio thread's buffer is kept, never dropped"
        );
    }

    /// **A buffer of the room before a re-prepare is dropped on its way
    /// back, never handed to the node** (the sequence that broke it). A
    /// 2 000-parameter node is prepared for 4096-frame chunks and its pool
    /// reserved to match; a chunk goes, so a 4096-room buffer is in flight;
    /// the node is re-prepared for 512 frames and the pool re-reserved; the
    /// audio thread takes a buffer meanwhile, so the free list has a place
    /// when the old one comes back. Then chunk after chunk, each swapped for
    /// a free buffer as `submit` does and returned as the bridge does, with
    /// 20 lanes offering a point on every frame (512 a queue, past the room
    /// of 273). Every buffer the node fills must be of its own room: the
    /// grid marks (`GridBits`) are laid out for it, and the server's lists
    /// are reserved for it.
    ///
    /// Mutation (run), fails: `return_params` comparing the queues only
    /// (`changes.point_room()` replaced by the reservation's own room,
    /// `(reserved & 0xFFFF_FFFF) as usize`) → the 4096-room buffer is
    /// recycled and reaches the node (the free-buffer assertion fails; past
    /// it, `GridBits::fits`' debug assertion would fire, and in release a
    /// queue would hold 385 points against the server's 273, with grid
    /// points past the 64th read as corners).
    #[test]
    fn a_buffer_of_the_room_before_a_re_prepare_is_dropped_not_reused() {
        let counters = Arc::new(DeliveryCounters::default());
        let mut d = ParamDelivery::new(
            (0..2000).map(|i| ParamAddress::Opaque(ParamId::new(i))),
            counters,
        );
        let pool = PayloadPool::new();
        let lanes = 20u32;
        let load = |d: &mut ParamDelivery, phase: usize| {
            d.begin();
            for f in 0..512i32 {
                for id in 0..lanes {
                    let v = ((f as usize + id as usize + phase) % 7 + 1) as f32 / 8.0;
                    let address = ParamAddress::Opaque(ParamId::new(id));
                    if f % 32 == 0 {
                        d.grid_point(address, f, v);
                    } else {
                        d.point(address, f, v);
                    }
                }
            }
        };
        // One chunk at 4096 frames goes: its buffer is in flight.
        let points = d.prepare(4096);
        pool.reserve_params(d.room(), points);
        load(&mut d, 0);
        let mut in_flight = pool.take_params().expect("a free buffer");
        std::mem::swap(d.changes(), &mut in_flight);
        d.handed_off(true);
        assert_eq!(in_flight.point_room(), points);

        // Re-prepared for 512, and re-reserved.
        let points = d.prepare(512);
        assert_eq!(points, ParameterQueue::chunk_room(512));
        pool.reserve_params(d.room(), points);
        // The audio thread takes one, so the list has a place free...
        let taken = pool.take_params().expect("a free buffer");
        // ...when the 4096-room buffer reaches the bridge thread.
        pool.return_params(in_flight);
        pool.return_params(taken);

        for phase in 1..=3 * PARAM_BUFFERS {
            load(&mut d, phase);
            let mut sent = pool.take_params().expect("a free buffer");
            assert_eq!(sent.point_room(), points, "a free buffer of the node's room");
            std::mem::swap(d.changes(), &mut sent);
            d.handed_off(true);
            assert_eq!(sent.len(), lanes as usize);
            for q in &sent.queues {
                assert!(
                    q.points.len() <= points,
                    "chunk {phase}: {} points, room {points}",
                    q.points.len()
                );
            }
            pool.return_params(sent);
        }
    }
}
