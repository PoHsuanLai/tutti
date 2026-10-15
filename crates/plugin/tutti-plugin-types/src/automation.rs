//! Parameter automation primitives passed into a plugin's process call.

use crate::{Normalized, ParamAddress};

/// One automation sample: the value at a specific sample offset within
/// the current process block.
#[derive(Debug, Clone, Copy)]
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

/// The automation points for one parameter within one process block.
///
/// A queue built with [`new`](Self::new) holds no storage, so its first point
/// allocates. A producer on the audio thread reserves the room ahead of time
/// ([`with_capacity`](Self::with_capacity), or a whole set through
/// [`ParameterChanges::reserved`]), clears rather than rebuilds between
/// blocks, and checks [`has_room`](Self::has_room) before each push (for a
/// queue in a reserved set, [`ParameterChanges::make_room`], which draws on
/// the set's shared point pool when the queue's own room is spent).
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
    /// Points in ascending, non-negative `sample_offset` order.
    ///
    /// In-process the caller maintains the order (all producers append
    /// monotonically). Across the wire there *is* no caller, so
    /// [`normalize`](ParameterQueue::normalize) re-establishes the invariant on
    /// deserialize — see the `Deserialize` impl below for why that isn't
    /// derived.
    pub points: Vec<ParameterPoint>,
}

impl ParameterQueue {
    /// The least point room [`chunk_room`](Self::chunk_room) gives a queue,
    /// however short the chunk.
    pub const MIN_POINTS: usize = 10;

    /// The spacing, in frames, a chunk's point room is sized for: a
    /// parameter can carry a point every this many frames of a chunk.
    pub const POINT_SPACING: usize = 32;

    /// Points a queue has room for beyond one per
    /// [`POINT_SPACING`](Self::POINT_SPACING): a segment's corners (its
    /// start and its arrival) and the chunk's last frame.
    pub const CORNER_POINTS: usize = 4;

    /// The points one parameter can carry in a chunk of `chunk` frames: one
    /// per [`POINT_SPACING`](Self::POINT_SPACING) frames and
    /// [`CORNER_POINTS`](Self::CORNER_POINTS) more, and never fewer than
    /// [`MIN_POINTS`](Self::MIN_POINTS). This is each queue's **logical**
    /// room: the host's plugin node reserves its chunk with it
    /// ([`ParameterChanges::reserved`]), and a format host that hands the
    /// points on reserves for it at activation with its largest block, so
    /// neither side allocates for a chunk's points. The storage behind it is
    /// shared ([`ParameterChanges::point_budget`]): every queue holds
    /// [`ParameterChanges::FLOOR_POINTS`] of its own, and any
    /// [`ParameterChanges::POOLED_QUEUES`] of them at once can reach this
    /// room.
    ///
    /// This is not yet design doc 014 §9 requirement 6
    /// (`ceil(chunk / CONTROL_STEP) + 1 + C` with `C = 256`, and past the
    /// room grid points dropped before corners); a full queue has its last
    /// point replaced instead. Doc 014's PR 2 brings both.
    pub fn chunk_room(chunk: usize) -> usize {
        (chunk.div_ceil(Self::POINT_SPACING) + Self::CORNER_POINTS).max(Self::MIN_POINTS)
    }

    /// Builds an empty queue addressed to `param_id`, with no point room:
    /// building it does not allocate, and its first point does.
    pub fn new(param_id: ParamAddress) -> Self {
        Self {
            param_id,
            points: Vec::new(),
        }
    }

    /// Builds an empty queue addressed to `param_id` with room for `points`
    /// points before it allocates again. Allocates; call it off the audio
    /// thread.
    pub fn with_capacity(param_id: ParamAddress, points: usize) -> Self {
        Self {
            param_id,
            points: Vec::with_capacity(points),
        }
    }

    /// Whether one more point fits in the queue's storage, so that pushing it
    /// does not allocate.
    pub fn has_room(&self) -> bool {
        self.points.len() < self.points.capacity()
    }

    /// Appends one automation point, **silently clamping `value` onto `0..=1`**.
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
    pub fn add_point(&mut self, sample_offset: i32, value: f64) {
        self.points.push(ParameterPoint {
            sample_offset,
            value: Normalized::new(value),
        });
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
        for p in &mut self.points {
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
/// `Serialize` stays derived: a value that already holds the invariant needs no
/// checking on the way out.
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ParameterQueue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            param_id: ParamAddress,
            points: Vec<ParameterPoint>,
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
/// **No queue cap.** `queues` is a plain `Vec`: a block can carry a queue for
/// every parameter the plugin has. What keeps it off the allocator on an
/// audio thread is capacity reserved ahead of time, the way the VST3 SDK's
/// host reserves one queue per parameter (`ParameterChanges::setMaxParameters`):
/// build it [`with_capacity`](Self::with_capacity) the plugin's parameter
/// count on the control thread, and [`clear`](Self::clear) it between blocks
/// (which keeps the capacity). A push past the reserved capacity grows the
/// `Vec`, which allocates; a real-time producer checks
/// [`has_room`](Self::has_room) first.
///
/// **Point capacity.** A set built [`reserved`](Self::reserved) also holds
/// that many spare queues. [`open_queue`](Self::open_queue) takes a spare
/// one, and [`clear`](Self::clear) puts every queue back (emptied, its
/// storage kept), so a set reused block after block never allocates or frees
/// a queue's points. The spares are not part of the value: they are not
/// serialized, and a deserialized set has none.
///
/// **The point pool.** The points a set can carry follow the points a chunk
/// carries, not the parameter count: each spare queue owns only
/// [`FLOOR_POINTS`](Self::FLOOR_POINTS) points, and the set keeps a shared
/// pool of [`POOLED_QUEUES`](Self::POOLED_QUEUES) blocks, each the full
/// per-queue room. A queue whose own points are spent takes a block from the
/// pool ([`make_room`](Self::make_room)), its points moved into it, and
/// [`clear`](Self::clear) gives every block back. So every parameter can move
/// in every chunk, and any `POOLED_QUEUES` of them can carry their full room
/// at once; past either, nothing grows, and the producer's own policy
/// applies (each format replaces the queue's last point, so the parameter
/// still ends the chunk on its latest value, and counts it).
/// [`add_change`](Self::add_change) on a reserved set does the same, counted
/// in [`points_overflowed`](Self::points_overflowed). A set for which the
/// pool would not at least halve the storage gives every queue the full room
/// instead ([`point_layout`](Self::point_layout)).
///
/// The one producer that grows a reserved set is the IPC bridge, which
/// prepends a skipped block's carried value to a queue (off the audio
/// thread); [`carried_budget`](Self::carried_budget) is what such a set can
/// arrive with.
///
/// The queues are still one `Vec` of points each, so every reader (the wire,
/// CLAP's time-sorted list, VST3's per-parameter queues) sees the same
/// per-parameter, time-ordered points as before.
///
/// On the wire the queues are a sequence (a length, then the queues), as they
/// were when they were stored inline, so the IPC format is unchanged; the
/// pool, like the spares, is not on it.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterChanges {
    /// One queue per parameter.
    pub queues: Vec<ParameterQueue>,
    /// Emptied queues kept for reuse, their point storage intact.
    #[cfg_attr(feature = "serde", serde(skip))]
    spare: Vec<ParameterQueue>,
    /// The shared point pool: empty blocks of `block_room` points each.
    #[cfg_attr(feature = "serde", serde(skip))]
    pool: Vec<Vec<ParameterPoint>>,
    /// Each queue holding a block from `pool`, by its index in `queues`,
    /// with the floor storage it gets back when the block returns. Reserved
    /// for every block, so a take never grows it.
    #[cfg_attr(feature = "serde", serde(skip))]
    floors: Vec<(usize, Vec<ParameterPoint>)>,
    /// The points a pool block holds: the per-queue room the set was
    /// reserved with, or 0 when it has no pool.
    #[cfg_attr(feature = "serde", serde(skip))]
    block_room: usize,
    /// Built [`reserved`](Self::reserved): [`add_change`](Self::add_change)
    /// is bounded by the reservation rather than growing it.
    #[cfg_attr(feature = "serde", serde(skip))]
    bounded: bool,
    /// See [`points_overflowed`](Self::points_overflowed).
    #[cfg_attr(feature = "serde", serde(skip))]
    overflowed: u64,
}

impl ParameterChanges {
    /// The points every queue of a [`reserved`](Self::reserved) set owns,
    /// whether or not it draws on the pool: a parameter can always take a
    /// step (or a value owed from an earlier chunk and a new one) in any
    /// chunk, so every listed parameter can move in every chunk (design doc
    /// 014 §9, requirement 1).
    pub const FLOOR_POINTS: usize = 2;

    /// The queues of a [`reserved`](Self::reserved) set that can carry their
    /// full room at once: the blocks in its shared pool. 256 parameters each
    /// moving at full density (a point every
    /// [`ParameterQueue::POINT_SPACING`] frames) in one chunk is past what
    /// ordinary automation of many parameters reaches. A constant rather
    /// than a setting: the plugin-server reserves its own lists with the same
    /// budget, and nothing on the wire carries it.
    pub const POOLED_QUEUES: usize = 256;

    /// How a set for `queues` queues of `points` points stores them: each
    /// queue's own room, and the number of blocks (each of `points`) in the
    /// shared pool. Pooled only when that holds at most half the points of
    /// every queue at full room: just past the break-even the pool would
    /// save little while letting only `POOLED_QUEUES` queues reach their
    /// full room. Otherwise `(points, 0)`, every queue at full room. A
    /// format host that keeps its own per-parameter queues (VST3's) lays
    /// them out the same way.
    pub fn point_layout(queues: usize, points: usize) -> (usize, usize) {
        let floor = Self::FLOOR_POINTS.min(points);
        let blocks = queues.min(Self::POOLED_QUEUES);
        let pooled = queues
            .saturating_mul(floor)
            .saturating_add(blocks.saturating_mul(points));
        if pooled.saturating_mul(2) <= queues.saturating_mul(points) {
            (floor, blocks)
        } else {
            (points, 0)
        }
    }

    /// The point storage [`reserved`](Self::reserved) gives a set for
    /// `queues` queues of `points` points each: `queues × FLOOR_POINTS +
    /// POOLED_QUEUES × points`, or `queues × points` when that is less (few
    /// queues). What a format host that hands a chunk's points on reserves
    /// for them too, so it holds what the set can carry.
    pub fn point_budget(queues: usize, points: usize) -> usize {
        let (floor, blocks) = Self::point_layout(queues, points);
        queues
            .saturating_mul(floor)
            .saturating_add(blocks.saturating_mul(points))
    }

    /// The points a set [`reserved`](Self::reserved) for `queues` queues of
    /// `points` points can arrive with on the far side of the IPC bridge:
    /// the same layout with one more point in every queue's own room and in
    /// every block, the value a skipped block carries, which the bridge
    /// prepends to any queue (a floor queue's own points, or a full block,
    /// and that one more). What the plugin-server's format hosts reserve for
    /// an input chunk, so a chunk the host's pool bounded reaches the plugin
    /// whole.
    pub fn carried_budget(queues: usize, points: usize) -> usize {
        let (_, blocks) = Self::point_layout(queues, points);
        Self::point_budget(queues, points)
            .saturating_add(queues)
            .saturating_add(blocks)
    }

    /// Points [`add_change`](Self::add_change) could not store whole in a
    /// [`reserved`](Self::reserved) set since it was built: past the queue's
    /// room with the pool spent (it replaced the queue's last point), or
    /// for an address past every reserved queue (dropped). Not on the wire,
    /// and [`clear`](Self::clear) keeps it. Zero for a set that is not
    /// reserved, which grows instead.
    pub fn points_overflowed(&self) -> u64 {
        self.overflowed
    }

    /// Builds an empty set of changes. Holds no capacity, so building one
    /// does not allocate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an empty set of changes with room for `queues` parameter queues
    /// before it allocates again. Allocates; call it off the audio thread,
    /// with the plugin's parameter count.
    pub fn with_capacity(queues: usize) -> Self {
        Self {
            queues: Vec::with_capacity(queues),
            ..Self::default()
        }
    }

    /// Builds an empty set of changes with room for `queues` parameter
    /// queues of up to `points` points each (a queue's logical room): every
    /// queue and the shared point pool are built here
    /// ([`point_budget`](Self::point_budget)), so
    /// [`open_queue`](Self::open_queue) and pushes after
    /// [`make_room`](Self::make_room) never allocate. Allocates; call it off
    /// the audio thread.
    pub fn reserved(queues: usize, points: usize) -> Self {
        // A placeholder address: `open_queue` readdresses each queue.
        let placeholder = ParamAddress::Index(0);
        let (floor, blocks) = Self::point_layout(queues, points);
        let mut pool = Vec::with_capacity(blocks);
        pool.extend((0..blocks).map(|_| Vec::with_capacity(points)));
        Self {
            queues: Vec::with_capacity(queues),
            spare: (0..queues)
                .map(|_| ParameterQueue::with_capacity(placeholder, floor))
                .collect(),
            pool,
            floors: Vec::with_capacity(blocks),
            block_room: if blocks > 0 { points } else { 0 },
            bounded: true,
            overflowed: 0,
        }
    }

    /// Makes room for one more point in `self.queues[queue]` without
    /// allocating: `true` when the queue has room of its own, or when it
    /// took a block from the shared pool (its points moved into the block,
    /// its own storage kept until [`clear`](Self::clear) gives the block
    /// back). `false` when the queue is at its full room or the pool is
    /// spent: the producer then replaces the queue's last point, or drops
    /// the point, and counts it. Allocation-free.
    pub fn make_room(&mut self, queue: usize) -> bool {
        let Some(q) = self.queues.get_mut(queue) else {
            return false;
        };
        if q.has_room() {
            return true;
        }
        if q.points.capacity() >= self.block_room {
            // At its logical room (or the set has no pool).
            return false;
        }
        let Some(mut block) = self.pool.pop() else {
            return false;
        };
        // An empty block of `block_room` points, more than the queue holds:
        // the copy never grows it.
        block.extend_from_slice(&q.points);
        // Its points are in the block now; `clear` empties the floor when it
        // goes back.
        let floor = core::mem::replace(&mut q.points, block);
        // `floors` holds one entry per block taken and was reserved for them
        // all (`clear` returns a block only with a floor to swap back), so
        // this push never grows it.
        debug_assert!(self.floors.len() < self.floors.capacity());
        self.floors.push((queue, floor));
        true
    }

    /// Opens an empty queue for `param_id` at the end of `queues`: a spare
    /// one when there is one (with its reserved point room), otherwise a new
    /// one with no point room (its first point allocates). Returns `false`,
    /// opening nothing, when `queues` has no room left
    /// ([`has_room`](Self::has_room)). Allocation-free.
    pub fn open_queue(&mut self, param_id: ParamAddress) -> bool {
        if !self.has_room() {
            return false;
        }
        let queue = self.take_queue(param_id);
        self.queues.push(queue);
        true
    }

    /// An empty queue for `param_id`: a spare one (with its reserved point
    /// room) when there is one, otherwise a new one with no point room.
    fn take_queue(&mut self, param_id: ParamAddress) -> ParameterQueue {
        match self.spare.pop() {
            Some(mut q) => {
                q.param_id = param_id;
                q
            }
            None => ParameterQueue::new(param_id),
        }
    }

    /// The points each queue [`open_queue`](Self::open_queue) hands out can
    /// reach before a push allocates, through [`make_room`](Self::make_room)
    /// while the pool has a block left: the smallest room among the spares,
    /// or none when there are no spares.
    pub fn point_capacity(&self) -> usize {
        let block = if self.pool.is_empty() {
            0
        } else {
            self.block_room
        };
        self.spare
            .iter()
            .map(|q| q.points.capacity().max(block))
            .min()
            .unwrap_or(0)
    }

    /// Every point this set has storage for, in its queues, its spares and
    /// its pool: what it costs in points, whatever it holds now.
    pub fn point_storage(&self) -> usize {
        let queues = self
            .queues
            .iter()
            .chain(&self.spare)
            .map(|q| q.points.capacity());
        let pool = self.pool.iter().map(Vec::capacity);
        let floors = self.floors.iter().map(|(_, f)| f.capacity());
        queues.chain(pool).chain(floors).sum()
    }

    /// The number of queues this can hold before a push allocates.
    pub fn capacity(&self) -> usize {
        self.queues.capacity()
    }

    /// Whether one more queue fits in the reserved capacity, so that pushing
    /// it does not allocate.
    pub fn has_room(&self) -> bool {
        self.queues.len() < self.queues.capacity()
    }

    /// Appends a point to the queue for `param_id`, creating that queue if it
    /// does not exist yet. A created queue is a spare one when the set has
    /// one (as [`open_queue`](Self::open_queue) takes), and a full queue
    /// draws on the pool ([`make_room`](Self::make_room)), so a set built
    /// [`reserved`](Self::reserved) and [`clear`](Self::clear)ed between
    /// blocks reuses its point storage.
    ///
    /// **A reserved set never grows.** On a set built
    /// [`reserved`](Self::reserved), past the queue's room with the pool
    /// spent the point replaces the queue's last one, so the parameter still
    /// ends on its latest value; an address past every reserved queue
    /// ([`has_room`](Self::has_room) false) is dropped. Either is counted in
    /// [`points_overflowed`](Self::points_overflowed), and `false` is
    /// returned. So a real-time producer may call it on a reserved set. A set
    /// that is not reserved (built [`new`](Self::new),
    /// [`with_capacity`](Self::with_capacity) or deserialized) grows instead,
    /// and always returns `true`.
    ///
    /// **`value` is silently clamped onto `0..=1`** by
    /// [`ParameterQueue::add_point`] — out-of-range input saturates with no
    /// error, so pass an already-normalized value.
    ///
    /// Queues are matched by full [`ParamAddress`] equality, so an opaque handle
    /// and a positional index that happen to be the same number address two
    /// different queues.
    pub fn add_change(&mut self, param_id: ParamAddress, sample_offset: i32, value: f64) -> bool {
        let i = match self.queues.iter().position(|q| q.param_id == param_id) {
            Some(i) => i,
            None if self.bounded && !self.has_room() => {
                self.overflowed += 1;
                return false;
            }
            None => {
                let queue = self.take_queue(param_id);
                self.queues.push(queue);
                self.queues.len() - 1
            }
        };
        let room = self.make_room(i);
        let queue = &mut self.queues[i];
        if room || !self.bounded {
            #[cfg(debug_assertions)]
            let before = queue.points.capacity();
            queue.add_point(sample_offset, value);
            // Loud where a change here would let a reserved set grow on an
            // audio thread again.
            #[cfg(debug_assertions)]
            debug_assert!(
                !self.bounded || queue.points.capacity() == before,
                "a reserved ParameterChanges grew"
            );
            return true;
        }
        // At its room with the pool spent: the latest value takes the last
        // point's place (a reserved queue's room is never zero, so it has one;
        // with none, the point is dropped).
        self.overflowed += 1;
        if queue.points.pop().is_some() {
            queue.add_point(sample_offset, value);
        }
        false
    }

    /// Pushes a prebuilt queue, returning `self` for chaining.
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

    /// Empties the set, keeping the reserved capacity for reuse across
    /// blocks: each queue goes back to the spares, emptied with its point
    /// storage kept, while the spares have room for it, and each pool block a
    /// queue took goes back to the pool (the queue gets its own storage
    /// back). Allocation-free, and frees nothing for a set built
    /// [`reserved`](Self::reserved) whose queues all came from
    /// [`open_queue`](Self::open_queue).
    ///
    /// **The spares keep the largest queues.** When they are full (a queue
    /// was pushed past them, with whatever room it had), each further queue
    /// replaces the spare with the least point room if it has more, and the
    /// smaller one is dropped. So a set that held one unreserved queue among
    /// its reserved ones keeps its reserved point room
    /// ([`point_capacity`](Self::point_capacity)), whatever order the
    /// queues were in.
    pub fn clear(&mut self) {
        // The blocks go back from exactly the queues that took them, by index
        // (a queue that grew past a block's room some other way keeps its
        // own storage), each swapped for its own floor, so the pool never
        // holds more blocks than it was reserved with, and `make_room`'s push
        // onto `floors` never grows it. A holder removed from `queues` by
        // hand takes its block with it.
        for (i, floor) in self.floors.drain(..) {
            if let Some(q) = self.queues.get_mut(i) {
                let mut block = core::mem::replace(&mut q.points, floor);
                block.clear();
                self.pool.push(block);
            }
        }
        while let Some(mut queue) = self.queues.pop() {
            queue.points.clear();
            if self.spare.len() < self.spare.capacity() {
                self.spare.push(queue);
                continue;
            }
            let smallest = self.spare.iter_mut().min_by_key(|q| q.points.capacity());
            if let Some(smallest) = smallest {
                if smallest.points.capacity() < queue.points.capacity() {
                    core::mem::swap(smallest, &mut queue);
                }
            }
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
        let mut q = ParameterQueue::new(ParamAddress::Opaque(ParamId::new(7)));
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
        let mut q = ParameterQueue::new(ParamAddress::Opaque(ParamId::new(1)));
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
        let mut q = ParameterQueue::new(ParamAddress::Opaque(ParamId::new(3)));
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

        let mut changes = ParameterChanges::new();
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
        let mut one = ParameterChanges::new();
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
        let mut changes = ParameterChanges::new();
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
            !changes.pool.is_empty(),
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
        assert_eq!(changes.point_capacity(), 40);
        let mut first = Vec::new();
        for block in 0..3 {
            for id in 0..3i32 {
                assert!(changes.open_queue(ParamAddress::Index(id)));
                let q = changes.queues.last_mut().expect("opened");
                assert_eq!(q.param_id, ParamAddress::Index(id));
                assert!(q.is_empty(), "a reused queue starts empty");
                for k in 0..40 {
                    assert!(q.has_room(), "block {block} queue {id} point {k}");
                    q.add_point(k, 0.5);
                }
                assert!(!q.has_room());
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
    /// Mutation (run): `clear` without the swap (a queue past full spares is
    /// dropped, the old behaviour) → `point_capacity()` is 0 → fails.
    #[test]
    fn clear_keeps_the_reserved_queues_over_an_unreserved_one() {
        let mut changes = ParameterChanges::reserved(4, 40);
        assert!(changes.open_queue(ParamAddress::Index(0)));
        assert!(changes.open_queue(ParamAddress::Index(1)));
        changes
            .queues
            .push(ParameterQueue::new(ParamAddress::Index(2)));
        changes.clear();
        assert_eq!(changes.point_capacity(), 40);
        for id in 0..4 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
            assert!(
                changes
                    .queues
                    .last()
                    .is_some_and(|q| q.points.capacity() >= 40),
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
                changes.queues.iter().all(|q| q.points.capacity() == 16),
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
    /// `queues × FLOOR_POINTS + POOLED_QUEUES × room`, under a quarter of
    /// every queue at full room. And a set for few queues gives each the full
    /// room, as it did before the pool, as does one where the pool would
    /// save less than half (300 queues of 40 points: 10 840 pooled against
    /// 12 000), while one where it saves more is pooled (1 000 queues of 40).
    ///
    /// Mutations (run), each fails: `point_layout` always `(points, 0)`
    /// (every queue at full room, the old layout) → the storage is `queues ×
    /// room`; `point_layout` pooling at any saving (`pooled < full`, as it
    /// first did) → 300 queues of 40 are pooled.
    #[test]
    fn a_reserved_set_holds_its_budget_not_params_times_room() {
        for params in [2_000usize, 16_384] {
            for chunk in [512usize, 4096] {
                let room = ParameterQueue::chunk_room(chunk);
                let changes = ParameterChanges::reserved(params, room);
                let budget = params * ParameterChanges::FLOOR_POINTS
                    + ParameterChanges::POOLED_QUEUES * room;
                assert_eq!(ParameterChanges::point_budget(params, room), budget);
                assert_eq!(changes.point_storage(), budget, "{params} at {chunk}");
                assert!(
                    4 * changes.point_storage() < params * room,
                    "{params} at {chunk}: {} points",
                    changes.point_storage()
                );
                assert_eq!(changes.point_capacity(), room);
            }
        }
        let few = ParameterChanges::reserved(10, 40);
        assert_eq!(few.point_storage(), 400);
        assert_eq!(ParameterChanges::point_budget(10, 40), 400);
        assert_eq!(ParameterChanges::point_layout(300, 40), (40, 0));
        assert_eq!(ParameterChanges::reserved(300, 40).point_storage(), 12_000);
        assert_eq!(
            ParameterChanges::point_layout(1_000, 40),
            (
                ParameterChanges::FLOOR_POINTS,
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
        let storage = changes.point_storage();
        let mut first: Vec<*const ParameterPoint> = Vec::new();
        for block in 0..3 {
            for id in 0..QUEUES {
                assert!(changes.open_queue(ParamAddress::Index(id as i32)));
            }
            // Every queue takes its floor.
            for i in 0..QUEUES {
                for k in 0..ParameterChanges::FLOOR_POINTS {
                    assert!(changes.make_room(i), "block {block} floor {i}");
                    changes.queues[i].add_point(k as i32, 0.5);
                }
            }
            // This block's pooled queues fill to their room; the next is
            // refused.
            let moving = block * ParameterChanges::POOLED_QUEUES
                ..(block + 1) * ParameterChanges::POOLED_QUEUES;
            for i in moving.clone() {
                for k in ParameterChanges::FLOOR_POINTS..ROOM {
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
    /// pool ends on its latest value, and two points are counted. A set
    /// that is not reserved still grows and counts nothing.
    ///
    /// Mutations (run), each fails: `add_change` pushing whatever
    /// `make_room` says (the unbounded path for every set) → the third point
    /// grows its floor (the debug assertion fires); the replacement not
    /// counted → `points_overflowed` is 1; an address past the queues opened
    /// anyway → a 1 001st queue, and nothing counted for it.
    #[test]
    fn add_change_on_a_reserved_set_never_grows_and_counts() {
        const QUEUES: usize = 1_000;
        const ROOM: usize = 20;
        let pooled = ParameterChanges::POOLED_QUEUES;
        let fill = |changes: &mut ParameterChanges, moving: core::ops::Range<usize>| {
            for k in 0..ROOM {
                for id in 0..QUEUES {
                    if k < ParameterChanges::FLOOR_POINTS || moving.contains(&id) {
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
            v.extend(c.pool.iter().map(|b| b.as_ptr() as usize));
            v.sort_unstable();
            v
        };
        let warm = ptrs(&changes);

        fill(&mut changes, pooled..2 * pooled);
        let past = ParamAddress::Index((2 * pooled) as i32);
        assert!(!changes.add_change(past, 7, 0.875), "the pool is spent");
        let q = changes.get_queue(past).expect("queued");
        assert_eq!(q.points.len(), ParameterChanges::FLOOR_POINTS);
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

        let mut open = ParameterChanges::new();
        for k in 0..ROOM as i32 {
            assert!(open.add_change(ParamAddress::Index(0), k, 0.5));
        }
        assert_eq!(open.queues[0].points.len(), ROOM);
        assert_eq!(open.points_overflowed(), 0);
    }

    /// **`clear` gives back exactly the blocks it lent, from the queues that
    /// took them.** A floor queue grown past a block's room some other way
    /// (the bridge's carried-value prepend, off the audio thread) holds no
    /// block: after `clear` the pool is the same blocks it was reserved
    /// with, by heap pointer, and the grown storage stays with the queue, in
    /// the spares.
    ///
    /// Mutation (run): `clear` deciding which queues hold a block by their
    /// capacity (`capacity >= block_room`, as it first did) → the grown floor
    /// goes into the pool in place of a block → fails.
    #[test]
    fn clear_gives_back_exactly_the_blocks_it_lent() {
        const QUEUES: usize = 1_000;
        const ROOM: usize = 20;
        let mut changes = ParameterChanges::reserved(QUEUES, ROOM);
        let mut blocks: Vec<usize> = changes.pool.iter().map(|b| b.as_ptr() as usize).collect();
        blocks.sort_unstable();
        for id in 0..QUEUES {
            assert!(changes.open_queue(ParamAddress::Index(id as i32)));
        }
        // The last queue grows past a block's room without one (popped
        // first by `clear`, ahead of every holder).
        for k in 0..2 * ROOM {
            changes.queues[QUEUES - 1].points.insert(
                0,
                ParameterPoint {
                    sample_offset: k as i32,
                    value: crate::Normalized::new(0.5),
                },
            );
        }
        let grown = changes.queues[QUEUES - 1].points.as_ptr() as usize;
        // The first `POOLED_QUEUES` queues take every block.
        for i in 0..ParameterChanges::POOLED_QUEUES {
            for k in 0..ROOM {
                assert!(changes.make_room(i));
                changes.queues[i].add_point(k as i32, 0.5);
            }
        }
        changes.clear();
        let mut back: Vec<usize> = changes.pool.iter().map(|b| b.as_ptr() as usize).collect();
        back.sort_unstable();
        assert_eq!(back, blocks);
        assert!(changes
            .pool
            .iter()
            .all(|b| b.is_empty() && b.capacity() == ROOM));
        assert!(changes
            .spare
            .iter()
            .any(|q| q.points.as_ptr() as usize == grown));
        assert!(changes.floors.is_empty());
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
