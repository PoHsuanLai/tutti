//! Parameter automation primitives passed into a plugin's process call.

use crate::{Normalized, ParamAddress, PointBuf, PointPool};

/// One automation sample: the value at a specific sample offset within
/// the current process block.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterPoint {
    /// Frame offset from the start of the current process block.
    ///
    /// Non-negative and ascending across a [`ParameterQueue`]'s `points`; a
    /// negative value is a sample index before the start of the buffer, and
    /// VST3's `getPoint` hands it to the plugin verbatim.
    pub sample_offset: i32,
    /// The automated value, on the host's `0..=1` scale.
    ///
    /// **[`Normalized`] clamps silently.** Anything outside `0..=1` saturates at
    /// the nearest bound with no error and no log line, so a producer that
    /// encodes data on some other scale here reads back as a run of `1.0` that
    /// looks like a legitimate parameter sweep. Values must already be
    /// normalized before they reach this field.
    ///
    /// The formats disagree about what a parameter value *is*: VST2 and VST3
    /// take normalized values, CLAP and AU take plain ones in the parameter's
    /// declared range (see [`PluginParams::get_parameter`]). A loader for
    /// either of the latter denormalizes before the value reaches the plugin —
    /// AU against the range it cached at load, CLAP against its `ranges` map.
    ///
    /// [`PluginParams::get_parameter`]: crate::PluginParams::get_parameter
    pub value: Normalized,
}

/// A point at `sample_offset` for `value`, **silently clamped onto `0..=1`**
/// (see [`ParameterQueue::add_point`]).
fn point(sample_offset: i32, value: f64) -> ParameterPoint {
    ParameterPoint {
        sample_offset,
        value: Normalized::new(value),
    }
}

/// The automation points for one parameter within one process block.
///
/// **Its point storage is fixed when it is built** ([`PointBuf`]): built
/// [`with_capacity`](Self::with_capacity) off the audio thread, cleared
/// rather than rebuilt between blocks, and never grown. A point past the
/// capacity is not stored as a new point: [`add_point`](Self::add_point)
/// keeps the queue's latest value instead and says so. A queue in a set
/// built [`ParameterChanges::reserved`] reaches its full room through the
/// set ([`ParameterChanges::make_room`], [`ParameterChanges::push_point`]),
/// which lends it a larger buffer from the set's shared point pool when its
/// own is spent.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ParameterQueue {
    /// Which parameter these points drive.
    ///
    /// A [`ParamAddress`] rather than a bare `u32` for the same reason the
    /// direct path takes one: the number alone does not say whether it is an
    /// opaque plugin-chosen handle (VST3/CLAP/AU) or a VST2 positional index,
    /// and the two do not even share a range. The address survives the wire, so
    /// a receiving loader reads the model off the value rather than assuming
    /// its own format's — which is correct only while a session hosts exactly
    /// one format, something nothing states and nothing checks.
    pub param_id: ParamAddress,
    /// Points in ascending, non-negative `sample_offset` order, in storage of
    /// a fixed capacity.
    ///
    /// In-process the caller maintains the order (all producers append
    /// monotonically). Across the wire there *is* no caller, so
    /// [`normalize`](ParameterQueue::normalize) re-establishes the invariant on
    /// deserialize — see the `Deserialize` impl below for why that isn't
    /// derived.
    pub points: PointBuf,
}

impl ParameterQueue {
    /// The spacing, in frames, a chunk's point room is sized for: a
    /// control lane's grid step (`tutti-nodes`' `CONTROL_STEP`), so a
    /// parameter can carry a point every this many frames of a chunk.
    pub const POINT_SPACING: usize = 32;

    /// Points a queue has room for beyond one per
    /// [`POINT_SPACING`](Self::POINT_SPACING) and the chunk's last frame:
    /// the corners a control lane can send in one block (`tutti-nodes`'
    /// `LANE_EVENT_CAPACITY`).
    pub const CORNER_POINTS: usize = 256;

    /// The points one parameter can carry in a chunk of `chunk` frames:
    /// one per [`POINT_SPACING`](Self::POINT_SPACING) frames, one for the
    /// chunk's last frame, and [`CORNER_POINTS`](Self::CORNER_POINTS) more
    /// (design doc 014 §9 requirement 6). This is each queue's **logical**
    /// room: the host's plugin node reserves its chunk with it
    /// ([`ParameterChanges::reserved_bridged`]), and a format host that hands
    /// the points on reserves for it at activation with its largest block, so
    /// neither side allocates for a chunk's points. The storage behind it is
    /// shared ([`ParameterChanges::point_budget`]): every queue holds
    /// [`ParameterChanges::FLOOR_POINTS`] of its own, and any
    /// [`ParameterChanges::POOLED_QUEUES`] of them at once can reach this
    /// room. Past a queue's room, or past its floor with the pool spent, the
    /// plugin node drops grid points before corners and counts them; a queue
    /// of corners keeps its latest value ([`PointBuf::replace_latest`]).
    ///
    /// ```
    /// use tutti_plugin_types::ParameterQueue;
    /// assert_eq!(ParameterQueue::chunk_room(4096), 128 + 1 + 256);
    /// assert_eq!(ParameterQueue::chunk_room(512), 16 + 1 + 256);
    /// assert_eq!(ParameterQueue::chunk_room(100), 4 + 1 + 256);
    /// ```
    pub fn chunk_room(chunk: usize) -> usize {
        chunk.div_ceil(Self::POINT_SPACING) + 1 + Self::CORNER_POINTS
    }

    /// Builds an empty queue addressed to `param_id` with room for exactly
    /// `points` points. Allocates (unless `points` is 0); call it off the
    /// audio thread.
    pub fn with_capacity(param_id: ParamAddress, points: usize) -> Self {
        Self {
            param_id,
            points: PointBuf::with_capacity(points),
        }
    }

    /// Builds a queue addressed to `param_id` holding exactly `points`, its
    /// capacity their count: for a builder sized by its data, off the audio
    /// thread.
    pub fn from_points(param_id: ParamAddress, points: Vec<ParameterPoint>) -> Self {
        Self {
            param_id,
            points: PointBuf::from_points(points),
        }
    }

    /// Appends one automation point, **silently clamping `value` onto `0..=1`**.
    /// Never grows the queue: when it is full the point is handled by the one
    /// full-queue policy ([`PointBuf::push_or_replace_last`]), which keeps the
    /// queue's latest value, and `false` is returned for the caller to count.
    ///
    /// The clamp is the whole point of the door and it reports nothing: a value
    /// of `40.0` is stored as `1.0`, and NaN becomes `0.0`. A caller passing
    /// anything but a normalized value gets a queue full of saturated points
    /// that reads back as a plausible parameter sweep, so normalize before
    /// calling — this method will not tell you that you did not.
    ///
    /// Takes a bare `f64` rather than a [`Normalized`] because the producer is
    /// `Curve::value_at` (in `tutti-mod`), a **public trait a user implements**,
    /// whose return carries no finiteness contract — so the value arriving here
    /// is untrusted by construction and the clamp belongs at this door rather
    /// than at every call site. An envelope dividing by a zero-length segment
    /// returns `f32::NAN`; unguarded, that NaN reaches a VST3 filter cutoff as a
    /// NaN coefficient, then a NaN IIR state, and every subsequent sample on the
    /// channel is NaN until the plugin is re-instantiated — a whole-channel
    /// outage from one bad envelope segment.
    pub fn add_point(&mut self, sample_offset: i32, value: f64) -> bool {
        self.points
            .push_or_replace_last(point(sample_offset, value))
    }

    /// Returns `true` when no point has been appended for this block.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Returns the number of automation points in this block.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Drops every point, keeping `param_id` and the point storage so the
    /// queue can be reused across blocks without allocating.
    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Re-establish the ordering invariant this type's `points` doc claims:
    /// clamp negative `sample_offset`s to 0 and stable-sort ascending.
    ///
    /// A no-op for the in-process producers (already monotonic); the wire path
    /// is what needs it. Stable so that two points sharing an offset keep the
    /// producer's intent — last-writer-wins per offset, which is what every
    /// consumer here assumes.
    pub fn normalize(&mut self) {
        for p in self.points.iter_mut() {
            if p.sample_offset < 0 {
                p.sample_offset = 0;
            }
        }
        if !self
            .points
            .windows(2)
            .all(|w| w[0].sample_offset <= w[1].sample_offset)
        {
            self.points.sort_by_key(|p| p.sample_offset);
        }
    }
}

/// Deserialize through [`ParameterQueue::normalize`].
///
/// **Hand-written rather than derived**, for the same reason `TimeSignature`'s
/// is in `tutti-types`: a derived impl writes `points` straight through, and
/// deserialization is the one place with no caller to maintain the ordering the
/// field's doc promises. A peer handing over an unsorted or negatively-offset
/// queue reaches a plugin unmodified — VST3's `IParamValueQueue::getPoint` and
/// CLAP's event emitter both iterate `points` in index order and hand the offset
/// to the plugin verbatim, so an out-of-order point becomes a parameter ramp
/// that jumps backwards mid-block, and a negative offset is a sample index
/// before the start of the buffer.
///
/// The points' storage is the decoded length exactly ([`PointBuf`]'s
/// `Deserialize`, the one place it is sized from data).
///
/// `Serialize` stays derived: a value that already holds the invariant needs no
/// checking on the way out.
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ParameterQueue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            param_id: ParamAddress,
            points: PointBuf,
        }
        let raw = Raw::deserialize(d)?;
        let mut queue = ParameterQueue {
            param_id: raw.param_id,
            points: raw.points,
        };
        queue.normalize();
        Ok(queue)
    }
}

/// The per-block parameter automation passed into or returned from a plugin's
/// process call, as one [`ParameterQueue`] per parameter.
///
/// **Nothing on it grows.** A set built [`reserved`](Self::reserved) (or
/// [`reserved_bridged`](Self::reserved_bridged)) off the audio thread holds a
/// queue for every parameter and the point storage they share, and is
/// [`clear`](Self::clear)ed between blocks, which keeps all of it. A set built
/// [`new`](Self::new) (or `Default`) is the empty placeholder and has room for
/// nothing; one decoded from the wire has exactly the points it carried. Past
/// the room a point is not stored as a new one: the queue keeps its latest
/// value ([`PointBuf::push_or_replace_last`], the one policy), or an address
/// past every queue is dropped, and the producer counts it
/// ([`add_change`](Self::add_change) in
/// [`points_overflowed`](Self::points_overflowed)). The queue list itself is
/// a `Vec` whose capacity is the parameter count, the way the VST3 SDK's host
/// reserves one queue per parameter (`ParameterChanges::setMaxParameters`);
/// [`open_queue`](Self::open_queue) refuses past it rather than growing it.
///
/// **Spare queues.** A reserved set holds its queues as spares:
/// [`open_queue`](Self::open_queue) takes one, and [`clear`](Self::clear)
/// puts every queue back (emptied, its storage kept), so a set reused block
/// after block never allocates or frees a queue's points. The spares are not
/// part of the value: they are not serialized, and a deserialized set has
/// none.
///
/// **The point pool.** The points a set can carry follow the points a chunk
/// carries, not the parameter count. For a chunk room of `room` points each
/// spare queue owns [`FLOOR_POINTS`](Self::FLOOR_POINTS) `+ 1` points, and
/// the set keeps a shared [`PointPool`] of
/// [`POOLED_QUEUES`](Self::POOLED_QUEUES) blocks of `room + 1`. A queue whose
/// own points are spent borrows a block ([`make_room`](Self::make_room)), its
/// points moved into it, and [`clear`](Self::clear) gives every block back.
/// So every parameter can move in every chunk, and any `POOLED_QUEUES` of
/// them can carry their full room at once. A set for which the pool would
/// not at least halve the storage gives every queue `room + 1` instead
/// ([`point_layout`](Self::point_layout)).
///
/// **The carried value.** The one point past `room` in every queue's own
/// storage and in every block is for the value the IPC bridge prepends from a
/// skipped block ([`prepend_carried`](Self::prepend_carried)). A set built
/// [`reserved_bridged`](Self::reserved_bridged) (the host's chunk) keeps it
/// back from its producers, so the carried value always takes a real slot; a
/// set built [`reserved`](Self::reserved) (what receives a bridged chunk,
/// and every other set) lets its producers use it, so it holds a bridged
/// chunk whole.
///
/// Every reader (the wire, CLAP's time-sorted list, VST3's per-parameter
/// queues) sees the same per-parameter, time-ordered points as before. On the
/// wire the queues are a sequence (a length, then the queues, each its
/// address and its points' sequence), as they were when they were stored
/// inline, so the IPC format is unchanged; the spares, the pool and the
/// spare capacity are not on it.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterChanges {
    /// One queue per parameter.
    pub queues: Vec<ParameterQueue>,
    /// Emptied queues kept for reuse, their point storage intact.
    #[cfg_attr(feature = "serde", serde(skip))]
    spare: Vec<ParameterQueue>,
    /// The shared point pool, lent to `queues` by index.
    #[cfg_attr(feature = "serde", serde(skip))]
    pool: PointPool,
    /// Points of every queue's storage (and of every block) kept for
    /// [`prepend_carried`](Self::prepend_carried): 1 for a set built
    /// [`reserved_bridged`](Self::reserved_bridged), otherwise 0.
    #[cfg_attr(feature = "serde", serde(skip))]
    carry_slot: usize,
    /// See [`point_room`](Self::point_room).
    #[cfg_attr(feature = "serde", serde(skip))]
    room: usize,
    /// See [`points_overflowed`](Self::points_overflowed).
    #[cfg_attr(feature = "serde", serde(skip))]
    overflowed: u64,
}

impl ParameterChanges {
    /// The points every queue of a [`reserved`](Self::reserved) set can take
    /// of its own, whether or not it draws on the pool, besides the carried
    /// value's: a parameter can always take a step (or a value owed from an
    /// earlier chunk and a new one) in any chunk, so every listed parameter
    /// can move in every chunk (design doc 014 §9, requirement 1).
    pub const FLOOR_POINTS: usize = 2;

    /// The queues of a [`reserved`](Self::reserved) set that can carry their
    /// full room at once: the blocks in its shared pool. 256 parameters each
    /// moving at full density (a point every
    /// [`ParameterQueue::POINT_SPACING`] frames) in one chunk is past what
    /// ordinary automation of many parameters reaches. A constant rather
    /// than a setting: the plugin-server reserves its own lists with the same
    /// budget, and nothing on the wire carries it.
    pub const POOLED_QUEUES: usize = 256;

    /// The queues a host's chunk holds past the plugin's parameter list,
    /// for addresses the list did not name (the plugin node learns them on
    /// the audio thread, within this room). A plugin-side reservation for
    /// the chunk adds them to the parameter count.
    pub const SPARE_QUEUES: usize = 64;

    /// How a set for `queues` queues of a chunk room of `room` points stores
    /// them: each queue's own storage, and the number of blocks (each of
    /// `room + 1`) in the shared pool. Pooled, at
    /// [`FLOOR_POINTS`](Self::FLOOR_POINTS) `+ 1` a queue, only when that
    /// holds at most half the points of every queue at `room + 1`: just past
    /// the break-even the pool would save little while letting only
    /// `POOLED_QUEUES` queues reach their full room. Otherwise
    /// `(room + 1, 0)`. A format host that keeps its own per-parameter
    /// queues (VST3's) lays them out the same way.
    pub fn point_layout(queues: usize, room: usize) -> (usize, usize) {
        let full = room.saturating_add(1);
        let floor = (Self::FLOOR_POINTS + 1).min(full);
        let blocks = queues.min(Self::POOLED_QUEUES);
        let pooled = queues
            .saturating_mul(floor)
            .saturating_add(blocks.saturating_mul(full));
        if pooled.saturating_mul(2) <= queues.saturating_mul(full) {
            (floor, blocks)
        } else {
            (full, 0)
        }
    }

    /// The point storage [`reserved`](Self::reserved) gives a set for
    /// `queues` queues of a chunk room of `room`, in the
    /// [`point_layout`](Self::point_layout): `queues × (FLOOR_POINTS + 1) +
    /// POOLED_QUEUES × (room + 1)`, or `queues × (room + 1)` when that is
    /// less (few queues). What the host's chunk holds, and what a format
    /// host that hands a chunk's points on reserves for them, so a chunk the
    /// host's pool bounded reaches the plugin whole.
    pub fn point_budget(queues: usize, room: usize) -> usize {
        let (floor, blocks) = Self::point_layout(queues, room);
        queues
            .saturating_mul(floor)
            .saturating_add(blocks.saturating_mul(room.saturating_add(1)))
    }

    /// Points this set could not store as new points since it was built:
    /// through [`add_change`](Self::add_change), past the queue's room with
    /// the pool spent (the queue kept its latest value) or for an address
    /// past every queue (dropped); through
    /// [`prepend_carried`](Self::prepend_carried), a carried value with no
    /// slot (dropped). Not on the wire, and [`clear`](Self::clear) keeps it.
    /// A producer that writes through [`push_point`](Self::push_point) or
    /// [`make_room`](Self::make_room) counts with its own counter.
    pub fn points_overflowed(&self) -> u64 {
        self.overflowed
    }

    /// The empty set: no queues and room for none, so building one does not
    /// allocate, and [`add_change`](Self::add_change) on it stores nothing
    /// (counted). What a block with no changes carries; a producer builds
    /// [`reserved`](Self::reserved).
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an empty set with room for `queues` parameter queues for a
    /// chunk room of `room` points, in the
    /// [`point_layout`](Self::point_layout): every queue and the shared point
    /// pool are built here ([`point_budget`](Self::point_budget)), so
    /// [`open_queue`](Self::open_queue), [`make_room`](Self::make_room) and
    /// the pushes after it never allocate. A queue can reach `room + 1`
    /// points. Allocates; call it off the audio thread.
    pub fn reserved(queues: usize, room: usize) -> Self {
        let (floor, blocks) = Self::point_layout(queues, room);
        // A placeholder address: `open_queue` readdresses each queue.
        let placeholder = ParamAddress::Index(0);
        Self {
            queues: Vec::with_capacity(queues),
            spare: (0..queues)
                .map(|_| ParameterQueue::with_capacity(placeholder, floor))
                .collect(),
            pool: PointPool::new(blocks, room.saturating_add(1), queues),
            carry_slot: 0,
            room,
            overflowed: 0,
        }
    }

    /// The host's chunk: [`reserved`](Self::reserved), with the point past
    /// `room` of every queue and every block kept for the bridge's
    /// [`prepend_carried`](Self::prepend_carried). Its producers
    /// ([`make_room`](Self::make_room), [`push_point`](Self::push_point),
    /// [`add_change`](Self::add_change)) stop at `room`, so the carried value
    /// always takes a real slot and nothing grows. Allocates; call it off
    /// the audio thread.
    pub fn reserved_bridged(queues: usize, room: usize) -> Self {
        Self {
            carry_slot: 1,
            ..Self::reserved(queues, room)
        }
    }

    /// Makes room for one more point in `self.queues[queue]` without
    /// allocating: `true` when the queue has room of its own, or when it
    /// borrowed a block from the shared pool (its points moved into the
    /// block, its own storage kept until [`clear`](Self::clear) gives the
    /// block back). `false` when the queue is at its full room or the pool is
    /// spent: the producer then keeps the queue's latest value
    /// ([`PointBuf::replace_latest`], which [`push_point`](Self::push_point)
    /// does), or drops the point, and counts it. A set built
    /// [`reserved_bridged`](Self::reserved_bridged) counts its kept carry
    /// slot as no room. Allocation-free.
    pub fn make_room(&mut self, queue: usize) -> bool {
        let Some(q) = self.queues.get_mut(queue) else {
            return false;
        };
        q.points.len() + self.carry_slot < q.points.capacity()
            || self.pool.lend(queue, &mut q.points)
    }

    /// Adds a point to `self.queues[queue]` within the set's room
    /// ([`make_room`](Self::make_room)), **silently clamping `value` onto
    /// `0..=1`** ([`ParameterQueue::add_point`]): `true` when it was stored
    /// as a new point; `false` when the queue's room is spent, and the queue
    /// kept its latest value ([`PointBuf::replace_latest`]) or the index is
    /// past the queues. Counts nothing: the producer counts a `false` with
    /// its own counter. Allocation-free.
    pub fn push_point(&mut self, queue: usize, sample_offset: i32, value: f64) -> bool {
        let room = self.make_room(queue);
        let Some(q) = self.queues.get_mut(queue) else {
            return false;
        };
        let p = point(sample_offset, value);
        if room {
            q.points.try_push(p).is_ok()
        } else {
            q.points.replace_latest(p);
            false
        }
    }

    /// Puts `carried` (a skipped block's last value for this parameter, on
    /// frame 0) ahead of `self.queues[queue]`'s points, in the slot a set
    /// built [`reserved_bridged`](Self::reserved_bridged) kept for it. With
    /// no slot (a set built otherwise, full) it is dropped: the queue's own
    /// points are later, so its latest value is already there. With an index
    /// past the queues (no queue for the parameter) it is dropped too, and
    /// there it is the parameter's latest value; the IPC bridge never asks
    /// that, since its buffers hold a queue for every slot of the delivery.
    /// Counted in [`points_overflowed`](Self::points_overflowed) and `false`
    /// returned. Allocation-free.
    pub fn prepend_carried(&mut self, queue: usize, carried: ParameterPoint) -> bool {
        let stored = self
            .queues
            .get_mut(queue)
            .is_some_and(|q| q.points.try_push_front(carried).is_ok());
        self.overflowed += u64::from(!stored);
        stored
    }

    /// Opens an empty queue for `param_id` at the end of `queues`: a spare
    /// one when there is one (with its reserved point room), otherwise one
    /// with no point room (which does not allocate). Returns `false`,
    /// opening nothing, when `queues` has no room left
    /// ([`has_room`](Self::has_room)). Allocation-free.
    pub fn open_queue(&mut self, param_id: ParamAddress) -> bool {
        if !self.has_room() {
            return false;
        }
        let mut queue = self
            .spare
            .pop()
            .unwrap_or_else(|| ParameterQueue::with_capacity(param_id, 0));
        queue.param_id = param_id;
        self.queues.push(queue);
        true
    }

    /// The chunk room this set was reserved for
    /// ([`reserved`](Self::reserved)'s `room`; 0 for a set built otherwise):
    /// with [`capacity`](Self::capacity), what fixes its
    /// [`point_layout`](Self::point_layout). A recycler that hands sets to a
    /// producer laid out for one room checks it, since a set of another
    /// room stores its points in another layout.
    pub fn point_room(&self) -> usize {
        self.room
    }

    /// Every point this set has storage for, in its queues, its spares and
    /// its pool: what it costs in points, whatever it holds now.
    pub fn point_storage(&self) -> usize {
        let queues: usize = self
            .queues
            .iter()
            .chain(&self.spare)
            .map(|q| q.points.capacity())
            .sum();
        queues + self.pool.storage()
    }

    /// The number of queues this can hold.
    pub fn capacity(&self) -> usize {
        self.queues.capacity()
    }

    /// Whether one more queue fits in the reserved capacity.
    pub fn has_room(&self) -> bool {
        self.queues.len() < self.queues.capacity()
    }

    /// Appends a point to the queue for `param_id`, opening that queue (a
    /// spare one, as [`open_queue`](Self::open_queue) takes) if it does not
    /// exist yet, within the set's room ([`push_point`](Self::push_point)).
    /// **Never grows the set**: past the queue's room with the pool spent the
    /// queue keeps its latest value, and an address past every queue
    /// ([`has_room`](Self::has_room) false) is dropped. Either is counted in
    /// [`points_overflowed`](Self::points_overflowed), and `false` is
    /// returned. So a real-time producer may call it on a set it
    /// [`clear`](Self::clear)s between blocks; a set built
    /// [`new`](Self::new) stores nothing.
    ///
    /// **It costs O(queues) per call**: the queue is found by a linear scan
    /// of the open queues. No allocation, but not cheap at many thousands of
    /// parameters; a producer that knows its queue's index uses
    /// [`push_point`](Self::push_point).
    ///
    /// **`value` is silently clamped onto `0..=1`** (as
    /// [`ParameterQueue::add_point`] does) — out-of-range input saturates with
    /// no error, so pass an already-normalized value.
    ///
    /// Queues are matched by full [`ParamAddress`] equality, so an opaque handle
    /// and a positional index that happen to be the same number address two
    /// different queues.
    pub fn add_change(&mut self, param_id: ParamAddress, sample_offset: i32, value: f64) -> bool {
        let i = match self.queues.iter().position(|q| q.param_id == param_id) {
            Some(i) => i,
            None if self.open_queue(param_id) => self.queues.len() - 1,
            None => {
                self.overflowed += 1;
                return false;
            }
        };
        let stored = self.push_point(i, sample_offset, value);
        self.overflowed += u64::from(!stored);
        stored
    }

    /// Pushes a prebuilt queue, returning `self` for chaining. Off the audio
    /// thread: it grows `queues` past its capacity.
    ///
    /// Appends unconditionally: it does not merge into an existing queue for the
    /// same address, so a duplicate address leaves two queues that
    /// [`get_queue`](Self::get_queue) resolves to the first of.
    pub fn add_queue(&mut self, queue: ParameterQueue) -> &mut Self {
        self.queues.push(queue);
        self
    }

    /// Returns `true` when no queue holds a point — including the case where
    /// queues exist but are all empty.
    pub fn is_empty(&self) -> bool {
        self.queues.is_empty() || self.queues.iter().all(|q| q.is_empty())
    }

    /// Returns the number of queues, which is the number of distinct parameter
    /// addresses touched — not the total point count.
    pub fn len(&self) -> usize {
        self.queues.len()
    }

    /// Empties the set, keeping the reserved storage for reuse across
    /// blocks: each pool block goes back from the queue that borrowed it (by
    /// index; the queue gets its own storage back), and each queue goes back
    /// to the spares, emptied with its point storage kept. Allocation-free,
    /// and frees nothing for a set built [`reserved`](Self::reserved) whose
    /// queues all came from [`open_queue`](Self::open_queue). Queues pushed
    /// past the spares' room ([`add_queue`](Self::add_queue), off the audio
    /// thread) are dropped first, so the spares keep the reserved queues.
    pub fn clear(&mut self) {
        // From exactly the queues that borrowed them, by index, each swapped
        // for its own storage, so the pool never holds more blocks than it was
        // reserved with. A holder removed from `queues` by hand takes its
        // block with it.
        let queues = &mut self.queues;
        self.pool.give_back(|i, own| match queues.get_mut(i) {
            Some(q) => {
                core::mem::swap(&mut q.points, own);
                true
            }
            None => false,
        });
        self.queues
            .truncate(self.spare.capacity() - self.spare.len());
        for mut queue in self.queues.drain(..) {
            queue.points.clear();
            self.spare.push(queue);
        }
    }

    /// Returns the queue addressed by `param_id`, if one has been created.
    pub fn get_queue(&self, param_id: ParamAddress) -> Option<&ParameterQueue> {
        self.queues.iter().find(|q| q.param_id == param_id)
    }

    /// Returns the queue addressed by `param_id` for mutation, if one has been
    /// created.
    pub fn get_queue_mut(&mut self, param_id: ParamAddress) -> Option<&mut ParameterQueue> {
        self.queues.iter_mut().find(|q| q.param_id == param_id)
    }
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::*;
    use crate::ParamId;

    fn round_trip(q: &ParameterQueue) -> ParameterQueue {
        let bytes = bincode::serialize(q).expect("serialize");
        bincode::deserialize(&bytes).expect("deserialize")
    }

    /// A peer can hand over points in any order; the wire path has no caller to
    /// maintain the `points` doc's ascending invariant, so deserialize must.
    #[test]
    fn deserialize_sorts_out_of_order_points() {
        let mut q = ParameterQueue::with_capacity(ParamAddress::Opaque(ParamId::new(7)), 3);
        q.add_point(32, 0.5);
        q.add_point(0, 0.0);
        q.add_point(16, 0.25);

        let got = round_trip(&q);

        assert_eq!(got.param_id, ParamAddress::Opaque(ParamId::new(7)));
        let offsets: Vec<i32> = got.points.iter().map(|p| p.sample_offset).collect();
        assert_eq!(offsets, vec![0, 16, 32]);
        // Values travel with their offsets, not independently.
        assert_eq!(got.points[0].value.get(), 0.0);
        assert_eq!(got.points[1].value.get(), 0.25);
        assert_eq!(got.points[2].value.get(), 0.5);
    }

    /// A negative offset is a sample index before the start of the buffer;
    /// VST3's `getPoint` hands it to the plugin verbatim.
    #[test]
    fn deserialize_clamps_negative_offsets() {
        let mut q = ParameterQueue::with_capacity(ParamAddress::Opaque(ParamId::new(1)), 2);
        q.add_point(-100, 0.75);
        q.add_point(8, 0.25);

        let got = round_trip(&q);

        assert_eq!(got.points[0].sample_offset, 0);
        assert_eq!(got.points[0].value.get(), 0.75);
        assert_eq!(got.points[1].sample_offset, 8);
    }

    /// An already-ordered queue is untouched — the common case pays no reorder.
    #[test]
    fn deserialize_leaves_ordered_points_alone() {
        let mut q = ParameterQueue::with_capacity(ParamAddress::Opaque(ParamId::new(3)), 9);
        for i in 0..9 {
            q.add_point(i * 8, i as f64 / 8.0);
        }

        let got = round_trip(&q);

        let offsets: Vec<i32> = got.points.iter().map(|p| p.sample_offset).collect();
        assert_eq!(offsets, (0..9).map(|i| i * 8).collect::<Vec<_>>());
    }

    /// The two addressing models stay distinct across the wire, and a queue
    /// keyed by one is not found by the other.
    ///
    /// This is what the field's type buys: `ParamId::new(3)` and `Index(3)` are
    /// the same number and must not be the same address. Fused into one number,
    /// the receiving loader can only recover the model from *which loader it
    /// is* — correct solely because a session hosts one format.
    #[test]
    fn the_addressing_model_survives_the_wire() {
        let opaque = ParamAddress::Opaque(ParamId::new(3));
        let index = ParamAddress::Index(3);
        assert_ne!(opaque, index, "same number, different address");

        let mut changes = ParameterChanges::reserved(2, 1);
        changes.add_change(opaque, 0, 0.25);
        changes.add_change(index, 0, 0.75);
        // Two queues, not one merged by a coinciding number.
        assert_eq!(changes.len(), 2);

        let bytes = bincode::serialize(&changes).expect("serialize");
        let got: ParameterChanges = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(
            got.get_queue(opaque).expect("opaque").points[0].value.get(),
            0.25
        );
        assert_eq!(
            got.get_queue(index).expect("index").points[0].value.get(),
            0.75
        );

        // An opaque id above `i32::MAX` is routine (these are often name
        // hashes) and is exactly what a bare number could not carry: read as
        // an index it is negative.
        let high = ParamAddress::Opaque(ParamId::new(0xF000_000A));
        let mut one = ParameterChanges::reserved(1, 1);
        one.add_change(high, 0, 1.0);
        let back: ParameterChanges =
            bincode::deserialize(&bincode::serialize(&one).expect("serialize")).expect("de");
        assert_eq!(back.get_queue(high).expect("high id survives").len(), 1);
        assert_eq!(back.queues[0].param_id.index(), None);
    }

    /// `ParameterChanges` derives its `Deserialize`, so the per-queue impl has
    /// to fire through the collection too — that is the shape the IPC protocol
    /// actually sends.
    #[test]
    fn nested_in_parameter_changes() {
        let mut changes = ParameterChanges::reserved(2, 2);
        changes.add_change(ParamAddress::Opaque(ParamId::new(1)), 32, 1.0);
        changes.add_change(ParamAddress::Opaque(ParamId::new(1)), -4, 0.0);
        changes.add_change(ParamAddress::Opaque(ParamId::new(2)), 0, 0.5);

        let bytes = bincode::serialize(&changes).expect("serialize");
        let got: ParameterChanges = bincode::deserialize(&bytes).expect("deserialize");

        let q = got
            .get_queue(ParamAddress::Opaque(ParamId::new(1)))
            .expect("queue 1");
        assert_eq!(q.points[0].sample_offset, 0);
        assert_eq!(q.points[1].sample_offset, 32);
    }

    /// **The wire format is unchanged**: a set of more than sixteen queues
    /// (past the old inline sixteen) of more than ten points (past the old
    /// inline ten) encodes byte for byte as the sequences the inline lists
    /// were (a length, then the items), and comes back whole. The spares of a
    /// reserved set are not on the wire.
    ///
    /// The expected bytes are the layout itself, written out, rather than a
    /// second container's encoding: every serde sequence encodes so, which is
    /// the property an older peer depends on.
    ///
    /// Mutation (run): `spare` serialized (no `serde(skip)`) → the bytes gain
    /// the spares → fails.
    #[test]
    fn more_than_sixteen_queues_cross_the_wire_unchanged() {
        fn seq(len: usize, items: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
            let mut out = (len as u64).to_le_bytes().to_vec();
            for item in items {
                out.extend(item);
            }
            out
        }
        let mut changes = ParameterChanges::reserved(200, 40);
        for id in 0..200i32 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
            let q = changes.queues.last_mut().expect("opened");
            for k in 0..12 {
                q.add_point(k, 0.25);
            }
            q.add_point(id, 0.5);
        }
        let expected = seq(
            changes.queues.len(),
            changes.queues.iter().map(|q| {
                let mut b = bincode::serialize(&q.param_id).expect("serialize");
                b.extend(seq(
                    q.points.len(),
                    q.points
                        .iter()
                        .map(|p| bincode::serialize(p).expect("serialize")),
                ));
                b
            }),
        );
        let bytes = bincode::serialize(&changes).expect("serialize");
        assert_eq!(bytes, expected);
        let back: ParameterChanges = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.queues.len(), 200);
        for (id, q) in back.queues.iter().enumerate() {
            assert_eq!(q.param_id, ParamAddress::Index(id as i32));
            assert_eq!(q.points.len(), 13);
        }
    }

    /// **A pooled set is the same value on the wire**: queues filled through
    /// the shared point pool, points for different parameters arriving
    /// interleaved in time, encode as the sequence of queues (a length, then
    /// each queue's address and points), byte for byte the layout an
    /// unpooled set has, and decode to the same per-parameter, time-ordered
    /// points. The pool and the floors are not on the wire.
    ///
    /// Mutation (run): the pool serialized (no `serde(skip)` on `pool`) →
    /// the bytes gain the spent pool's blocks → fails.
    #[test]
    fn a_pooled_set_crosses_the_wire_as_its_queues() {
        fn seq(len: usize, items: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
            let mut out = (len as u64).to_le_bytes().to_vec();
            for item in items {
                out.extend(item);
            }
            out
        }
        let mut changes = ParameterChanges::reserved(1_000, 40);
        assert!(
            ParameterChanges::point_layout(1_000, 40).1 > 0,
            "1000 queues of 40 points are pooled"
        );
        // Interleaved: frame by frame, every parameter, as a producer
        // sampling many lanes sends them; 200 parameters past their floor,
        // within the pool.
        for k in 0..13 {
            for id in 0..200i32 {
                changes.add_change(ParamAddress::Index(id), k * 4 + id % 4, 0.25);
            }
        }
        let expected = seq(
            200,
            (0..200i32).map(|id| {
                let mut b = bincode::serialize(&ParamAddress::Index(id)).expect("serialize");
                b.extend(seq(
                    13,
                    (0..13).map(|k| {
                        bincode::serialize(&ParameterPoint {
                            sample_offset: k * 4 + id % 4,
                            value: crate::Normalized::new(0.25),
                        })
                        .expect("serialize")
                    }),
                ));
                b
            }),
        );
        let bytes = bincode::serialize(&changes).expect("serialize");
        assert_eq!(bytes, expected);
    }
}

#[cfg(test)]
mod reserved_tests {
    use super::*;

    /// **A reserved set hands out queues with the reserved point room, and
    /// takes them back on `clear` with that room intact**, block after
    /// block: what keeps a producer filling forty points a queue off the
    /// allocator. The heap pointer of each queue's points is the witness
    /// that the storage is the same allocation, never freed and rebuilt.
    ///
    /// Mutations (run), each fails: `clear` dropping the queues instead of
    /// keeping them (the spare-room check `false`) → the second block's
    /// queues have no room; `open_queue` ignoring the spares
    /// (always `ParameterQueue::new`) → the same.
    #[test]
    fn a_reserved_set_keeps_its_point_storage_across_blocks() {
        let mut changes = ParameterChanges::reserved(3, 40);
        assert!(changes.spare.iter().all(|q| q.points.capacity() == 41));
        let mut first = Vec::new();
        for block in 0..3 {
            for id in 0..3i32 {
                assert!(changes.open_queue(ParamAddress::Index(id)));
                let q = changes.queues.last_mut().expect("opened");
                assert_eq!(q.param_id, ParamAddress::Index(id));
                assert!(q.is_empty(), "a reused queue starts empty");
                for k in 0..41 {
                    assert!(q.points.has_room(), "block {block} queue {id} point {k}");
                    q.add_point(k, 0.5);
                }
                assert!(!q.points.has_room());
            }
            let ptrs: Vec<*const ParameterPoint> =
                changes.queues.iter().map(|q| q.points.as_ptr()).collect();
            if block == 0 {
                first = ptrs;
            } else {
                let mut a = first.clone();
                let mut b = ptrs;
                a.sort();
                b.sort();
                assert_eq!(a, b, "the same storage, reused");
            }
            changes.clear();
            assert!(changes.queues.is_empty());
        }
        // Past the reserved queues, nothing opens.
        for id in 0..3 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
        }
        assert!(!changes.open_queue(ParamAddress::Index(3)));
    }

    /// **A queue pushed past the spares does not cost a reserved one its
    /// place.** Two reserved queues opened and one unreserved queue pushed
    /// after them (as the bridge's carry merge once did): `clear` takes them
    /// back last-first, so the unreserved queue filled the spares' last free
    /// place and a reserved queue was dropped, and every later chunk had a
    /// queue with no room among its 40-point ones.
    ///
    /// Mutation (run): `clear` without its `truncate` (every queue pushed
    /// back to the spares, the unreserved one last, so opened first) → the
    /// first queue opened has no room → fails.
    #[test]
    fn clear_keeps_the_reserved_queues_over_an_unreserved_one() {
        let mut changes = ParameterChanges::reserved(4, 40);
        assert!(changes.open_queue(ParamAddress::Index(0)));
        assert!(changes.open_queue(ParamAddress::Index(1)));
        changes
            .queues
            .push(ParameterQueue::with_capacity(ParamAddress::Index(2), 0));
        changes.clear();
        assert!(changes.spare.iter().all(|q| q.points.capacity() == 41));
        for id in 0..4 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
            assert!(
                changes
                    .queues
                    .last()
                    .is_some_and(|q| q.points.capacity() >= 41),
                "queue {id} has the reserved room"
            );
        }
    }

    /// **`add_change` takes a spare queue too**, so a producer that adds
    /// changes point by point (a host draining a plugin's output list) into a
    /// reserved set it clears each block reuses the same point storage and
    /// never allocates. The heap pointers are the witness, as above.
    ///
    /// Mutation (run): `add_change` building `ParameterQueue::new` instead of
    /// taking a spare → the queues have no reserved room and new storage
    /// each block → fails.
    #[test]
    fn add_change_reuses_the_spare_queues() {
        let mut changes = ParameterChanges::reserved(2, 16);
        let mut first = Vec::new();
        for block in 0..3 {
            for id in 0..2i32 {
                for k in 0..16 {
                    changes.add_change(ParamAddress::Index(id), k, 0.5);
                }
            }
            assert_eq!(changes.queues.len(), 2);
            assert!(
                changes.queues.iter().all(|q| q.points.capacity() == 17),
                "block {block}: each queue is a reserved one"
            );
            let mut ptrs: Vec<*const ParameterPoint> =
                changes.queues.iter().map(|q| q.points.as_ptr()).collect();
            ptrs.sort();
            if block == 0 {
                first = ptrs;
            } else {
                assert_eq!(first, ptrs, "the same storage, reused");
            }
            changes.clear();
        }
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    /// **A reserved set's point storage is its budget, not parameters ×
    /// room**: for 2 000 and 16 384 parameters at 512- and 4096-frame chunks,
    /// `queues × (FLOOR_POINTS + 1) + POOLED_QUEUES × (room + 1)` (the
    /// carried layout: one point a queue and a block past the room), under a
    /// third of every queue at the room. And a set for few queues gives each
    /// `room + 1`, as does one where the pool would save less than half (300
    /// queues of 40 points: 11 396 pooled against 12 300), while one where
    /// it saves more is pooled (1 000 queues of 40). (Reworded with the one
    /// layout: the plain layout's floor was `FLOOR_POINTS` and its blocks
    /// `room`, under a quarter.)
    ///
    /// Mutations (run), each fails: `point_layout` always `(full, 0)`
    /// (every queue at full room, the old layout) → the storage is `queues ×
    /// (room + 1)`; `point_layout` pooling at any saving (`pooled < full`, as
    /// it first did) → 300 queues of 40 are pooled.
    #[test]
    fn a_reserved_set_holds_its_budget_not_params_times_room() {
        for params in [2_000usize, 16_384] {
            for chunk in [512usize, 4096] {
                let room = ParameterQueue::chunk_room(chunk);
                let changes = ParameterChanges::reserved(params, room);
                let budget = params * (ParameterChanges::FLOOR_POINTS + 1)
                    + ParameterChanges::POOLED_QUEUES * (room + 1);
                assert_eq!(ParameterChanges::point_budget(params, room), budget);
                assert_eq!(changes.point_storage(), budget, "{params} at {chunk}");
                assert!(
                    3 * changes.point_storage() < params * room,
                    "{params} at {chunk}: {} points",
                    changes.point_storage()
                );
                assert!(changes
                    .pool
                    .free_blocks()
                    .iter()
                    .all(|b| b.capacity() == room + 1));
            }
        }
        let few = ParameterChanges::reserved(10, 40);
        assert_eq!(few.point_storage(), 410);
        assert_eq!(ParameterChanges::point_budget(10, 40), 410);
        assert_eq!(ParameterChanges::point_layout(300, 40), (41, 0));
        assert_eq!(ParameterChanges::reserved(300, 40).point_storage(), 12_300);
        assert_eq!(
            ParameterChanges::point_layout(1_000, 40),
            (
                ParameterChanges::FLOOR_POINTS + 1,
                ParameterChanges::POOLED_QUEUES
            )
        );
    }

    /// **Any `POOLED_QUEUES` queues reach their full room, every queue keeps
    /// its floor, and one past is refused, not grown**, block after block:
    /// the blocks go back to the pool on `clear` (the same storage, by heap
    /// pointer), so the next block, with a different `POOLED_QUEUES` queues
    /// moving, has them all again.
    ///
    /// Mutations (run), each fails: `clear` not giving the blocks back (the
    /// return skipped) → the second block's moving queues find the pool
    /// spent; `make_room` growing past an empty pool (`true` with no block)
    /// → the 257th moving queue takes a third point.
    #[test]
    fn pooled_queues_reach_full_room_and_one_past_is_refused() {
        const QUEUES: usize = 1_000;
        const ROOM: usize = 40;
        let mut changes = ParameterChanges::reserved(QUEUES, ROOM);
        let (floor, _) = ParameterChanges::point_layout(QUEUES, ROOM);
        let storage = changes.point_storage();
        let mut first: Vec<*const ParameterPoint> = Vec::new();
        for block in 0..3 {
            for id in 0..QUEUES {
                assert!(changes.open_queue(ParamAddress::Index(id as i32)));
            }
            // Every queue takes its floor.
            for i in 0..QUEUES {
                for k in 0..floor {
                    assert!(changes.make_room(i), "block {block} floor {i}");
                    changes.queues[i].add_point(k as i32, 0.5);
                }
            }
            // This block's pooled queues fill to their room; the next is
            // refused.
            let moving = block * ParameterChanges::POOLED_QUEUES
                ..(block + 1) * ParameterChanges::POOLED_QUEUES;
            for i in moving.clone() {
                for k in floor..=ROOM {
                    assert!(changes.make_room(i), "block {block} queue {i} point {k}");
                    changes.queues[i].add_point(k as i32, 0.5);
                }
                assert!(!changes.make_room(i), "full room reached");
            }
            assert!(!changes.make_room(moving.end));
            let mut ptrs: Vec<*const ParameterPoint> = changes.queues[moving]
                .iter()
                .map(|q| q.points.as_ptr())
                .collect();
            ptrs.sort();
            if block == 0 {
                first = ptrs;
            } else {
                assert_eq!(first, ptrs, "the same blocks, reused");
            }
            assert_eq!(changes.point_storage(), storage, "nothing grew");
            changes.clear();
        }
    }

    /// **`add_change` on a reserved set never grows it; past the room it
    /// counts.** Warmed with one pattern (the first `POOLED_QUEUES`
    /// parameters at full room), then measured with another (the next
    /// `POOLED_QUEUES` at full room, so none of them finds a block a warm-up
    /// left behind, and one more parameter given three points with the pool
    /// spent, and an address past every queue): the storage is the
    /// reservation's, by count and by heap pointer, the parameter past the
    /// pool ends on its latest value, and two points are counted. The empty
    /// set ([`ParameterChanges::new`]) stores nothing and counts each point.
    /// (Reworded with `PointBuf`: the empty set grew, uncounted, before.)
    ///
    /// Mutations (run), each fails: `push_point` pushing whatever
    /// `make_room` says (`try_push` on no room, so the point is dropped
    /// rather than kept as the latest) → the parameter past the pool ends on
    /// its second point; the replacement not counted → `points_overflowed`
    /// is 1; an address past the queues opened anyway (`open_queue` without
    /// its room check) → a 1 001st queue, and nothing counted for it.
    #[test]
    fn add_change_on_a_reserved_set_never_grows_and_counts() {
        const QUEUES: usize = 1_000;
        const ROOM: usize = 20;
        let pooled = ParameterChanges::POOLED_QUEUES;
        let (floor, _) = ParameterChanges::point_layout(QUEUES, ROOM);
        let fill = |changes: &mut ParameterChanges, moving: core::ops::Range<usize>| {
            for k in 0..ROOM {
                for id in 0..QUEUES {
                    if k < floor || moving.contains(&id) {
                        assert!(changes.add_change(ParamAddress::Index(id as i32), k as i32, 0.5));
                    }
                }
            }
        };
        let mut changes = ParameterChanges::reserved(QUEUES, ROOM);
        let storage = changes.point_storage();
        fill(&mut changes, 0..pooled);
        changes.clear();
        let ptrs = |c: &ParameterChanges| {
            let mut v: Vec<usize> = c.spare.iter().map(|q| q.points.as_ptr() as usize).collect();
            v.extend(c.pool.free_blocks().iter().map(|b| b.as_ptr() as usize));
            v.sort_unstable();
            v
        };
        let warm = ptrs(&changes);

        fill(&mut changes, pooled..2 * pooled);
        let past = ParamAddress::Index((2 * pooled) as i32);
        assert!(!changes.add_change(past, 7, 0.875), "the pool is spent");
        let q = changes.get_queue(past).expect("queued");
        assert_eq!(q.points.len(), floor);
        assert_eq!(
            q.points.last().map(|p| (p.sample_offset, p.value.get())),
            Some((7, 0.875))
        );
        assert!(!changes.add_change(ParamAddress::Index(QUEUES as i32), 0, 0.5));
        assert_eq!(changes.queues.len(), QUEUES);
        assert_eq!(changes.points_overflowed(), 2);
        assert_eq!(changes.point_storage(), storage);
        assert_eq!(changes.capacity(), QUEUES);
        changes.clear();
        assert_eq!(ptrs(&changes), warm, "the same storage");

        let mut empty = ParameterChanges::new();
        for k in 0..ROOM as i32 {
            assert!(!empty.add_change(ParamAddress::Index(0), k, 0.5));
        }
        assert!(empty.queues.is_empty());
        assert_eq!(empty.points_overflowed(), ROOM as u64);
    }

    /// **`clear` gives back exactly the blocks it lent, from the queues that
    /// took them.** A queue with storage past a block's room that holds no
    /// block (one built by hand and put in the set) keeps it: after `clear`
    /// the pool is the same blocks it was reserved with, by heap pointer,
    /// and the larger storage stays with the queue, in the spares.
    /// (Reworded with `PointBuf`: the large queue was a floor grown by the
    /// bridge's prepend, which can no longer grow.)
    ///
    /// Mutation (run): `clear` deciding which queues hold a block by their
    /// capacity (`capacity >= block_room`, as it first did) → the large
    /// queue's storage goes into the pool in place of a block → fails.
    #[test]
    fn clear_gives_back_exactly_the_blocks_it_lent() {
        const QUEUES: usize = 1_000;
        const ROOM: usize = 20;
        let mut changes = ParameterChanges::reserved(QUEUES, ROOM);
        let mut blocks: Vec<usize> = changes
            .pool
            .free_blocks()
            .iter()
            .map(|b| b.as_ptr() as usize)
            .collect();
        blocks.sort_unstable();
        for id in 0..QUEUES {
            assert!(changes.open_queue(ParamAddress::Index(id as i32)));
        }
        // The last queue has twice a block's room and no block (popped first
        // by `clear`, ahead of every holder).
        let mut large =
            ParameterQueue::with_capacity(ParamAddress::Index(QUEUES as i32 - 1), 2 * ROOM);
        for k in 0..2 * ROOM {
            assert!(large.add_point(k as i32, 0.5));
        }
        let grown = large.points.as_ptr() as usize;
        changes.queues[QUEUES - 1] = large;
        // The first `POOLED_QUEUES` queues take every block.
        for i in 0..ParameterChanges::POOLED_QUEUES {
            for k in 0..ROOM {
                assert!(changes.make_room(i));
                changes.queues[i].add_point(k as i32, 0.5);
            }
        }
        changes.clear();
        let mut back: Vec<usize> = changes
            .pool
            .free_blocks()
            .iter()
            .map(|b| b.as_ptr() as usize)
            .collect();
        back.sort_unstable();
        assert_eq!(back, blocks);
        assert!(changes
            .pool
            .free_blocks()
            .iter()
            .all(|b| b.is_empty() && b.capacity() == ROOM + 1));
        assert!(changes
            .spare
            .iter()
            .any(|q| q.points.as_ptr() as usize == grown));
        assert_eq!(changes.pool.lent(), 0);
    }

    /// **A queue that held a block starts empty when it is opened again.**
    /// Its points moved into the block, and the floor storage it gets back
    /// on `clear` must not still hold them, or the next chunk would send
    /// last chunk's points ahead of its own.
    ///
    /// Mutation (run): `clear` not emptying the queues it puts back (no
    /// `queue.points.clear()`, so a floor swapped back keeps the points the
    /// queue held before it took its block) → the reopened queues hold stale
    /// points → fails.
    #[test]
    fn a_queue_that_held_a_block_starts_empty_when_reopened() {
        let mut changes = ParameterChanges::reserved(1_000, 30);
        for id in 0..ParameterChanges::POOLED_QUEUES {
            assert!(changes.open_queue(ParamAddress::Index(id as i32)));
            for k in 0..10 {
                assert!(changes.make_room(id));
                changes.queues[id].add_point(k, 0.5);
            }
        }
        changes.clear();
        for id in 0..1_000 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
        }
        assert!(changes.queues.iter().all(ParameterQueue::is_empty));
        assert!(changes.is_empty());
    }
}

#[cfg(test)]
mod carried_tests {
    use super::*;

    /// **Every set holds the one budget, and the host's chunk gives its
    /// producers the room, not `room + 1`**: a set built `reserved` or
    /// `reserved_bridged` stores [`ParameterChanges::point_budget`] points (a
    /// floor of `FLOOR_POINTS + 1` and blocks of `room + 1` when pooled,
    /// every queue at `room + 1` when not); a producer fills a queue of a
    /// plain set to `room + 1` and one of a bridged set to `room`.
    ///
    /// Mutations (run), each fails: `reserved` without the extra floor
    /// point (`FLOOR_POINTS`, not `FLOOR_POINTS + 1`) → the storage is
    /// `queues` short; `make_room` ignoring the carry slot → a bridged queue
    /// takes `room + 1`.
    #[test]
    fn every_set_holds_the_one_budget_and_a_bridged_one_keeps_a_slot() {
        for (queues, room) in [(2_000usize, 20usize), (16_384, 132), (10, 40)] {
            let budget = ParameterChanges::point_budget(queues, room);
            let mut plain = ParameterChanges::reserved(queues, room);
            let mut bridged = ParameterChanges::reserved_bridged(queues, room);
            assert_eq!(plain.point_storage(), budget, "{queues} × {room}");
            assert_eq!(bridged.point_storage(), budget, "{queues} × {room}");
            for (changes, want) in [(&mut plain, room + 1), (&mut bridged, room)] {
                assert!(changes.open_queue(ParamAddress::Index(0)));
                let mut k = 0;
                while changes.push_point(0, k, 0.5) {
                    k += 1;
                }
                assert_eq!(k as usize, want, "{queues} × {room}");
            }
        }
        assert_eq!(
            ParameterChanges::point_budget(2_000, 20),
            2_000 * (ParameterChanges::FLOOR_POINTS + 1) + ParameterChanges::POOLED_QUEUES * 21
        );
        assert_eq!(ParameterChanges::point_budget(10, 40), 10 * 41);
    }

    /// **The bridge's carried value takes a real slot in the host's chunk at
    /// its pool budget, with every queue full, and nothing allocates or
    /// grows.** 2 000 parameters at a 512-frame chunk room, built
    /// `reserved_bridged`: every queue filled to its floor, the last
    /// `POOLED_QUEUES` (so the floor queues before them had every chance to
    /// take a block) through `push_point` until `make_room` refuses, which
    /// is at the room. (Reworded: the fill stopped at the room whatever
    /// `make_room` said, so the carry-slot mutation below survived it.) Then a carried value goes ahead of every queue,
    /// inside the no-alloc gate: each lands first, on frame 0, nothing is
    /// counted, and the storage is the reservation's.
    ///
    /// Mutations (run), each fails: `reserved_bridged` without the carry slot
    /// (`carry_slot: 0`) → the producers fill the slot, and the queues have
    /// none left for the carried value, counted; `prepend_carried` appending
    /// (`try_push`) rather than inserting at the front → the carried value
    /// is not first. (Reworded with the one layout: a third mutation, the
    /// plain layout, no longer exists.)
    #[test]
    fn a_carried_value_takes_a_real_slot_at_the_pool_budget_without_allocating() {
        const PARAMS: usize = 2_000;
        let room = ParameterQueue::chunk_room(512);
        let moving = PARAMS - ParameterChanges::POOLED_QUEUES..PARAMS;
        let mut changes = ParameterChanges::reserved_bridged(PARAMS, room);
        let storage = changes.point_storage();
        for id in 0..PARAMS {
            assert!(changes.open_queue(ParamAddress::Index(id as i32)));
            let want = if moving.contains(&id) {
                room
            } else {
                ParameterChanges::FLOOR_POINTS
            };
            // A moving queue fills until `make_room` refuses: the room the
            // producers get, with the carried value's slot kept back.
            let mut k = 0;
            while (moving.contains(&id) || k < want) && changes.make_room(id) {
                assert!(changes.push_point(id, 1 + k as i32, 0.5));
                k += 1;
            }
            assert_eq!(k, want, "queue {id}");
        }
        assert_eq!(changes.points_overflowed(), 0);
        let carried = ParameterPoint {
            sample_offset: 0,
            value: Normalized::new(0.25),
        };
        let all = assert_no_alloc::assert_no_alloc(|| {
            (0..PARAMS).all(|i| changes.prepend_carried(i, carried))
        });
        assert!(all, "every carried value took a slot");
        assert_eq!(changes.points_overflowed(), 0);
        assert_eq!(changes.point_storage(), storage);
        for (id, q) in changes.queues.iter().enumerate() {
            let want = if moving.contains(&id) {
                room + 1
            } else {
                ParameterChanges::FLOOR_POINTS + 1
            };
            assert_eq!(q.points.len(), want, "queue {id}");
            assert_eq!(q.points[0].sample_offset, 0);
            assert_eq!(q.points[0].value.get(), 0.25);
        }
        changes.clear();
        assert_eq!(changes.point_storage(), storage);
    }

    /// **Past every slot, the carried value is dropped, counted**: the
    /// queue's own points are later, so its latest value is already there,
    /// and nothing of the queue is touched; an index past the queues is
    /// dropped and counted the same way. (Reworded with the one layout: a
    /// full queue fell back to the full-queue policy, which never took a
    /// carried value, always the earliest.)
    ///
    /// Mutation (run): `prepend_carried` not counting the miss → the count
    /// is 0 → fails.
    #[test]
    fn a_carried_value_with_no_slot_is_dropped_counted() {
        let mut changes = ParameterChanges::reserved(1, 2);
        for (k, v) in [(5, 0.5), (9, 0.75), (12, 1.0)] {
            assert!(changes.add_change(ParamAddress::Index(0), k, v));
        }
        let carried = ParameterPoint {
            sample_offset: 0,
            value: Normalized::new(0.25),
        };
        assert!(!changes.prepend_carried(0, carried));
        assert!(!changes.prepend_carried(1, carried));
        assert_eq!(changes.points_overflowed(), 2);
        let points: Vec<(i32, f64)> = changes.queues[0]
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();
        assert_eq!(points, vec![(5, 0.5), (9, 0.75), (12, 1.0)]);
    }
}
