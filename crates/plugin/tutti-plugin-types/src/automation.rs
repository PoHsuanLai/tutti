//! Parameter automation primitives passed into a plugin's process call.

use smallvec::SmallVec;

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
/// Ten points live inline, so a queue built with [`new`](Self::new) holds a
/// short block's automation without allocating. A producer that needs more
/// reserves them ahead of time ([`with_capacity`](Self::with_capacity), or a
/// whole set through [`ParameterChanges::reserved`]) and checks
/// [`has_room`](Self::has_room) before each push on the audio thread.
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
    pub points: SmallVec<[ParameterPoint; 10]>,
}

impl ParameterQueue {
    /// Builds an empty queue addressed to `param_id`.
    pub fn new(param_id: ParamAddress) -> Self {
        Self {
            param_id,
            points: SmallVec::new(),
        }
    }

    /// Builds an empty queue addressed to `param_id` with room for `points`
    /// points before it allocates again. Allocates past the ten inline ones;
    /// call it off the audio thread.
    pub fn with_capacity(param_id: ParamAddress, points: usize) -> Self {
        Self {
            param_id,
            points: SmallVec::with_capacity(points),
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

    /// Drops every point, keeping `param_id` and the inline capacity so the
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
            points: SmallVec<[ParameterPoint; 10]>,
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
/// that many spare queues, each with room for a given number of points.
/// [`open_queue`](Self::open_queue) takes a spare one, and
/// [`clear`](Self::clear) puts every queue back (emptied, its storage kept),
/// so a set reused block after block never allocates or frees a queue's
/// points. The spares are not part of the value: they are not serialized,
/// and a deserialized set has none.
///
/// On the wire a `Vec` and the `SmallVec` this used to be serialize the same
/// way (a length, then the queues), so the IPC format is unchanged.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterChanges {
    /// One queue per parameter.
    pub queues: Vec<ParameterQueue>,
    /// Emptied queues kept for reuse, their point storage intact.
    #[cfg_attr(feature = "serde", serde(skip))]
    spare: Vec<ParameterQueue>,
}

impl ParameterChanges {
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
            spare: Vec::new(),
        }
    }

    /// Builds an empty set of changes with room for `queues` parameter
    /// queues of `points` points each: every queue is built here, so
    /// [`open_queue`](Self::open_queue) and pushes within a queue's room
    /// never allocate. Allocates; call it off the audio thread.
    pub fn reserved(queues: usize, points: usize) -> Self {
        // A placeholder address: `open_queue` readdresses each queue.
        let placeholder = ParamAddress::Index(0);
        Self {
            queues: Vec::with_capacity(queues),
            spare: (0..queues)
                .map(|_| ParameterQueue::with_capacity(placeholder, points))
                .collect(),
        }
    }

    /// Opens an empty queue for `param_id` at the end of `queues`: a spare
    /// one when there is one (with its reserved point room), otherwise a new
    /// one with the inline room. Returns `false`, opening nothing, when
    /// `queues` has no room left ([`has_room`](Self::has_room)).
    /// Allocation-free.
    pub fn open_queue(&mut self, param_id: ParamAddress) -> bool {
        if !self.has_room() {
            return false;
        }
        let queue = match self.spare.pop() {
            Some(mut q) => {
                q.param_id = param_id;
                q
            }
            None => ParameterQueue::new(param_id),
        };
        self.queues.push(queue);
        true
    }

    /// The points each queue [`open_queue`](Self::open_queue) hands out can
    /// hold before a push allocates: the smallest room among the spares, or
    /// the inline room when there are none.
    pub fn point_capacity(&self) -> usize {
        self.spare
            .iter()
            .map(|q| q.points.capacity())
            .min()
            .unwrap_or_else(|| {
                ParameterQueue::new(ParamAddress::Index(0))
                    .points
                    .capacity()
            })
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
    /// does not exist yet.
    ///
    /// **`value` is silently clamped onto `0..=1`** by
    /// [`ParameterQueue::add_point`] — out-of-range input saturates with no
    /// error, so pass an already-normalized value.
    ///
    /// Queues are matched by full [`ParamAddress`] equality, so an opaque handle
    /// and a positional index that happen to be the same number address two
    /// different queues.
    pub fn add_change(&mut self, param_id: ParamAddress, sample_offset: i32, value: f64) {
        if let Some(queue) = self.queues.iter_mut().find(|q| q.param_id == param_id) {
            queue.add_point(sample_offset, value);
        } else {
            let mut queue = ParameterQueue::new(param_id);
            queue.add_point(sample_offset, value);
            self.queues.push(queue);
        }
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
    /// storage kept, while the spares have room for it. Allocation-free, and
    /// frees nothing for a set built [`reserved`](Self::reserved) whose
    /// queues all came from [`open_queue`](Self::open_queue).
    pub fn clear(&mut self) {
        while let Some(mut queue) = self.queues.pop() {
            if self.spare.len() < self.spare.capacity() {
                queue.points.clear();
                self.spare.push(queue);
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
    /// (past the old inline sixteen) encodes byte for byte as the
    /// `SmallVec<[_; 16]>` it used to be, and comes back whole. The spares
    /// of a reserved set are not on the wire.
    ///
    /// Mutation (run): `spare` serialized (no `serde(skip)`) → the bytes gain
    /// the spares → fails.
    #[test]
    fn more_than_sixteen_queues_cross_the_wire_unchanged() {
        #[derive(serde::Serialize)]
        struct Old {
            queues: SmallVec<[ParameterQueue; 16]>,
        }
        let mut changes = ParameterChanges::reserved(200, 40);
        for id in 0..200i32 {
            assert!(changes.open_queue(ParamAddress::Index(id)));
            let q = changes.queues.last_mut().expect("opened");
            q.add_point(0, 0.25);
            q.add_point(id, 0.5);
        }
        let old = Old {
            queues: changes.queues.iter().cloned().collect(),
        };
        let bytes = bincode::serialize(&changes).expect("serialize");
        assert_eq!(bytes, bincode::serialize(&old).expect("serialize"));
        let back: ParameterChanges = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.queues.len(), 200);
        for (id, q) in back.queues.iter().enumerate() {
            assert_eq!(q.param_id, ParamAddress::Index(id as i32));
            assert_eq!(q.points.len(), 2);
        }
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
    /// queues have the inline room only; `open_queue` ignoring the spares
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
}
