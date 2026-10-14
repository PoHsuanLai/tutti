//! Curve segments ([`ParamSegment`]) on a hosted plugin's event input, as
//! the parameter points a plugin takes.
//!
//! CLAP and VST3 parameter changes (and VST2's, through the same queue) are
//! point values: the plugin holds each until the next. A ramp reaches the
//! plugin as one point, its target on its frame. A segment is a move over
//! many frames, possibly many chunks, so the node keeps the segments in
//! flight here and sends points along each until it arrives:
//!
//! - its value on its event frame;
//! - its value at every [`stride_for`]`(chunk)`-th frame of each chunk and
//!   on each chunk's last frame, as [`PluginAutomation`](super::PluginAutomation)
//!   spaces its points (so a segment puts at most [`MAX_POINTS`] points a
//!   chunk on its parameter, within the queue's room, `param_delivery::points_for`);
//! - its end value on the frame it arrives ([`ParamSegment::arrives_after`]),
//!   after which it is dropped: the plugin holds the end value exactly.
//!
//! A later ramp or segment for the same parameter interrupts it on its
//! frame, as in a graph param port.
//!
//! **Bounds, and what happens past them.**
//!
//! - **Segments in flight:** one slot per parameter address (a later segment
//!   on an address replaces its slot), as many slots as the node has
//!   parameter slots (`ParamDelivery::room`: the plugin's parameter count,
//!   read at load, and some spare), allocated by
//!   [`prepare`](SegmentPoints::prepare) on the control thread. A segment
//!   is tracked only on an address with a parameter slot, so they cannot
//!   run out; were they to, the segment is sent **as a ramp: its end value
//!   at once, on its event frame**, so the parameter never holds its start
//!   value. On the audio thread nothing allocates.
//! - **Parameters per chunk:** none beyond the plugin's own. A chunk has a
//!   queue for every parameter (`ParamDelivery`, sized at bind), so a point
//!   for a tracked segment always has a queue; a segment on an address the
//!   node has no slot for is refused at its start and counted
//!   (`ParamDeliveryStats::points_dropped`). Points pass through the node's
//!   changes-only filter like any other: a flat stretch sends nothing.
//!
//! A re-prepare or a reset drops every segment in flight.

use tutti_graph::ParamSegment;
use tutti_types::Samples;

use super::param_automation_source::stride_for;
#[cfg(doc)]
use super::param_automation_source::MAX_POINTS;
use super::param_delivery::ParamDelivery;
use crate::protocol::ParamAddress;

/// Segment slots the unit tests prepare: a figure past any they fill.
#[cfg(test)]
const MAX_SEGMENTS: usize = 1024;

/// One segment in flight.
#[derive(Clone, Copy, Debug)]
struct Track {
    address: ParamAddress,
    segment: ParamSegment,
    /// The node's absolute frame of the segment's event.
    start: u64,
    /// The first absolute frame not yet considered for a point.
    next: u64,
    /// The absolute frame it arrives on, if it does.
    end: Option<u64>,
}

/// The segments in flight on a plugin node's event input. See the module
/// docs.
#[derive(Debug, Default)]
pub(crate) struct SegmentPoints {
    /// The slots, sized by [`prepare`](Self::prepare); live ones first.
    tracks: Vec<Track>,
    /// The current chunk's length: where its last frame is, and its stride.
    chunk: usize,
}

impl SegmentPoints {
    /// Size the slots for `slots` segments and drop any in flight. Control
    /// thread: allocates (once; a later call with the same size reuses).
    pub(super) fn prepare(&mut self, slots: usize) {
        self.tracks.clear();
        self.tracks.reserve_exact(slots);
    }

    /// Drop every segment in flight (a reset). Allocation-free.
    pub(super) fn clear(&mut self) {
        self.tracks.clear();
    }

    /// A chunk of `chunk` frames begins.
    pub(super) fn begin(&mut self, chunk: usize) {
        self.chunk = chunk;
    }

    /// A ramp or segment for `address` lands: the segment it was on, if
    /// any, stops here.
    pub(super) fn interrupt(&mut self, address: ParamAddress) {
        self.tracks.retain(|t| t.address != address);
    }

    /// `segment` for `address` starts at absolute frame `frame`, chunk frame
    /// `at`: its first point, and the segment followed from the next frame
    /// on unless it has already arrived. With no slot free it is sent as a
    /// ramp: its end value, now. An address the delivery has no slot for is
    /// refused (counted there).
    pub(super) fn start(
        &mut self,
        changes: &mut ParamDelivery,
        address: ParamAddress,
        segment: ParamSegment,
        frame: u64,
        at: usize,
    ) {
        if !changes.knows(address) {
            return;
        }
        self.interrupt(address);
        let arrives = segment.arrives_after();
        if arrives == Some(Samples(0)) {
            point(changes, address, at, segment.raw_value_after(Samples(0)));
            return;
        }
        // `len < capacity`: a push that cannot allocate. Past it, the
        // fallback (module docs).
        if self.tracks.len() >= self.tracks.capacity() {
            let end = segment.raw_value_after(Samples(u32::MAX as usize));
            point(changes, address, at, end);
            return;
        }
        point(changes, address, at, segment.raw_value_after(Samples(0)));
        self.tracks.push(Track {
            address,
            segment,
            start: frame,
            next: frame + 1,
            end: arrives.map(|j| frame + j.get() as u64),
        });
    }

    /// The points the segments in flight put on absolute frames
    /// `from..to`, the first of them at chunk frame `at`.
    pub(super) fn points(&mut self, changes: &mut ParamDelivery, from: u64, to: u64, at: usize) {
        if from >= to {
            return;
        }
        let stride = stride_for(self.chunk).max(1);
        let last = self.chunk.saturating_sub(1);
        self.tracks.retain_mut(|t| {
            let mut f = t.next.max(from);
            while f < to {
                let c = at + (f - from) as usize;
                let arrived = t.end == Some(f);
                if arrived || c.is_multiple_of(stride) || c == last {
                    let j = f.saturating_sub(t.start);
                    let value = t
                        .segment
                        .raw_value_after(Samples(usize::try_from(j).unwrap_or(usize::MAX)));
                    point(changes, t.address, c, value);
                    if arrived {
                        // Done: the plugin holds its end value.
                        return false;
                    }
                }
                f += 1;
            }
            t.next = to;
            true
        });
    }
}

/// A point at chunk frame `at`. The address has a slot (`start` checked).
fn point(changes: &mut ParamDelivery, address: ParamAddress, at: usize, value: f32) {
    let offset = i32::try_from(at).unwrap_or(i32::MAX);
    changes.point(address, offset, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::node::param_delivery::DeliveryCounters;
    use crate::protocol::{ParamId, ParameterChanges};
    use std::sync::Arc;
    use tutti_graph::SegmentShape;

    const A: ParamAddress = ParamAddress::Opaque(ParamId::new(7));

    /// A delivery with a slot for each of `ids`, and the spares.
    fn delivery(ids: impl IntoIterator<Item = u32>) -> ParamDelivery {
        ParamDelivery::new(
            ids.into_iter()
                .map(|id| ParamAddress::Opaque(ParamId::new(id))),
            Arc::new(DeliveryCounters::default()),
        )
    }

    /// `(chunk frame, value)` of `address`'s queue in the chunk being filled.
    fn queue(d: &mut ParamDelivery, address: ParamAddress) -> Vec<(i32, f32)> {
        d.changes()
            .queues
            .iter()
            .find(|q| q.param_id == address)
            .map(|q| {
                q.points
                    .iter()
                    .map(|p| (p.sample_offset, p.value.get() as f32))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The chunk goes, as the bridge takes it: swapped for an empty buffer.
    fn hand_off(d: &mut ParamDelivery) -> ParameterChanges {
        let mut sent = ParameterChanges::reserved(d.room(), d.points());
        std::mem::swap(d.changes(), &mut sent);
        d.handed_off(true);
        sent
    }

    /// A segment spanning several chunks sends points along itself in each,
    /// on the stride and each chunk's last frame, and lands its end value
    /// exactly on the frame it arrives; then nothing more.
    ///
    /// Mutations (run), each fails: no point on the arrival frame (only the
    /// stride and the last frame) → the end value lands late; dropping a
    /// track after each run of points (`false` for the `true` after
    /// `t.next = to`) → the next chunk gets no points; never tracking
    /// (`end: Some(frame)`, every segment arrived at once) → fails.
    #[test]
    fn a_segment_sends_points_until_it_lands() {
        let seg = ParamSegment::foreign(7, 0.0, 1.0, Samples(150), SegmentShape::PowerIn(2.0));
        let mut sp = SegmentPoints::default();
        let mut d = delivery([7]);
        sp.prepare(MAX_SEGMENTS);
        let chunk = 64;
        let mut chunks = Vec::new();
        for k in 0..4u64 {
            d.begin();
            sp.begin(chunk);
            let base = k * chunk as u64;
            if k == 0 {
                // The event on frame 10.
                sp.points(&mut d, 0, 10, 0);
                sp.start(&mut d, A, seg, 10, 10);
                sp.points(&mut d, 10, 64, 10);
            } else {
                sp.points(&mut d, base, base + 64, 0);
            }
            chunks.push(queue(&mut d, A));
            hand_off(&mut d);
        }
        let at = |k: u64, c: i32| k as u32 * 64 + c as u32 - 10;
        for (k, q) in chunks.iter().enumerate().take(3) {
            for &(c, v) in q {
                let want = seg.raw_value_after(Samples(at(k as u64, c) as usize));
                assert_eq!(v, want, "chunk {k}, frame {c}");
            }
        }
        // Chunk 0: the event, the stride (8) from 16 on, the last frame.
        let frames0: Vec<i32> = chunks[0].iter().map(|p| p.0).collect();
        assert_eq!(frames0.first(), Some(&10));
        assert_eq!(frames0.last(), Some(&63));
        assert!(!chunks[1].is_empty(), "points in the next chunk");
        // It arrives 149 frames after frame 10: frame 159, chunk 2 frame 31.
        assert_eq!(chunks[2].last(), Some(&(31, 1.0)), "lands on its frame");
        assert!(chunks[2].iter().all(|p| p.0 <= 31));
        assert!(chunks[3].is_empty(), "nothing after it arrives");
    }

    /// A later event for the same parameter stops the segment on its frame;
    /// another parameter's segment carries on.
    ///
    /// Mutation (run): `interrupt` doing nothing → the stopped segment keeps
    /// sending points → fails.
    #[test]
    fn an_interrupting_event_stops_the_segment() {
        let b = ParamAddress::Opaque(ParamId::new(8));
        let seg = ParamSegment::foreign(7, 0.0, 1.0, Samples(500), SegmentShape::Linear);
        let mut sp = SegmentPoints::default();
        let mut d = delivery([7, 8]);
        sp.prepare(MAX_SEGMENTS);
        d.begin();
        sp.begin(64);
        sp.start(&mut d, A, seg, 0, 0);
        sp.start(&mut d, b, seg, 0, 0);
        sp.points(&mut d, 1, 20, 1);
        sp.interrupt(A);
        sp.points(&mut d, 20, 64, 20);
        assert!(queue(&mut d, A).iter().all(|p| p.0 < 20));
        assert_eq!(queue(&mut d, b).last().map(|p| p.0), Some(63));
    }

    /// **Two hundred segments moving at once each land their end value on
    /// their own arrival frame**, with every point along the way: a chunk
    /// has a queue for every parameter, so none is starved. (This replaces
    /// the 16-queue chunk, past which a starved segment's end value landed
    /// a chunk or more late.)
    ///
    /// Mutations (run), each fails: the delivery's chunk sized at 16 queues
    /// (`ParameterChanges::reserved(16, MAX_POINTS)` in `ParamDelivery::new`) →
    /// 184 parameters get no points in the first chunk; a track dropped before its arrival
    /// point is placed (`return false` ahead of the `point` call) → the end
    /// values never land.
    #[test]
    fn two_hundred_segments_each_land_on_their_frame() {
        let n = 200u32;
        let addr = |k: u32| ParamAddress::Opaque(ParamId::new(100 + k));
        // Address `k` arrives `400 - k` frames after frame 0.
        let seg = |k: u32| {
            ParamSegment::foreign(
                100 + k,
                0.0,
                1.0,
                Samples(401 - k as usize),
                SegmentShape::PowerIn(2.0),
            )
        };
        let mut sp = SegmentPoints::default();
        let mut d = delivery((0..n).map(|k| 100 + k));
        sp.prepare(d.room());
        let mut landed_at = vec![None; n as usize];
        let mut latest = vec![f32::NAN; n as usize];
        for chunk in 0..8u64 {
            d.begin();
            sp.begin(64);
            let base = chunk * 64;
            if chunk == 0 {
                for k in 0..n {
                    sp.start(&mut d, addr(k), seg(k), 0, 0);
                }
                sp.points(&mut d, 1, 64, 1);
            } else {
                sp.points(&mut d, base, base + 64, 0);
            }
            for k in 0..n {
                let points = queue(&mut d, addr(k));
                if chunk == 0 {
                    assert_eq!(
                        points.first().map(|p| p.0),
                        Some(0),
                        "{k} starts on frame 0"
                    );
                }
                for (c, v) in points {
                    let at = base + c as u64;
                    assert_eq!(
                        v,
                        seg(k).raw_value_after(Samples(at as usize)),
                        "{k} at {at}"
                    );
                    latest[k as usize] = v;
                    if v == 1.0 && landed_at[k as usize].is_none() {
                        landed_at[k as usize] = Some(at);
                    }
                }
            }
            hand_off(&mut d);
        }
        for k in 0..n {
            assert_eq!(latest[k as usize], 1.0, "address {k} reached its end value");
            let arrival = seg(k).arrives_after().expect("arrives").get() as u64;
            assert_eq!(
                landed_at[k as usize],
                Some(arrival),
                "address {k} on its frame"
            );
        }
    }

    /// A segment that finds every slot taken is sent as a ramp: its end
    /// value at once, on its event frame, never its start value held.
    ///
    /// Mutation (run): the fallback sending the start value
    /// (`raw_value_after(Samples(0))`) → fails.
    #[test]
    fn a_segment_past_the_slots_is_sent_as_a_ramp() {
        let mut sp = SegmentPoints::default();
        let mut d = delivery([1, 2, 3]);
        sp.prepare(2);
        let seg = |id: u32| ParamSegment::foreign(id, 0.0, 1.0, Samples(500), SegmentShape::Linear);
        d.begin();
        sp.begin(64);
        for id in [1, 2, 3] {
            sp.start(
                &mut d,
                ParamAddress::Opaque(ParamId::new(id)),
                seg(id),
                5,
                5,
            );
        }
        assert_eq!(
            queue(&mut d, ParamAddress::Opaque(ParamId::new(3))),
            vec![(5, 1.0)]
        );
        assert!(queue(&mut d, ParamAddress::Opaque(ParamId::new(1)))[0].1 < 0.01);
    }
}
