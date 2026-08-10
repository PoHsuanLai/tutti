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

/// Ordered list of [`ParameterPoint`]s for a single parameter id within
/// one block. Inline storage keeps a full block's automation run
/// allocation-free on the RT path.
///
/// Sized for the densest producer: `ParamAutomationSource` samples one point
/// per `SAMPLE_STRIDE` (8) samples plus the final sample, so a full
/// `MAX_BUFFER_SIZE` (64) block yields `64/8 + 1 = 9` points — 10 inline
/// leaves headroom and never spills mid-block.
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

/// Collection of [`ParameterQueue`]s, one per parameter id, passed into
/// or returned from a plugin's process call.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParameterChanges {
    /// One queue per parameter; the first 16 live inline.
    pub queues: SmallVec<[ParameterQueue; 16]>,
}

impl ParameterChanges {
    /// Builds an empty set of changes.
    pub fn new() -> Self {
        Self::default()
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

    /// Drops every queue, keeping the inline capacity for reuse across blocks.
    pub fn clear(&mut self) {
        self.queues.clear();
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
}
