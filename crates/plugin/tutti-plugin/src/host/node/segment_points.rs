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
//!   spaces its points (so a chunk holds at most [`MAX_POINTS`] per
//!   parameter, past which the queue replaces its last point);
//! - its end value on the frame it arrives ([`ParamSegment::arrives_after`]),
//!   after which it is dropped: the plugin holds the end value exactly.
//!
//! A later ramp or segment for the same parameter interrupts it on its
//! frame, as in a graph param port.
//!
//! **Bounds, and what happens past them.**
//!
//! - **Segments in flight:** one slot per parameter address (a later segment
//!   on an address replaces its slot), [`MAX_SEGMENTS`] slots, allocated by
//!   [`prepare`](SegmentPoints::prepare) on the control thread. The plugin's
//!   parameter count would be the exact bound, but asking a plugin-server
//!   for it is a blocking round trip a live `prepare` must not make, so the
//!   bound is a figure past any realistic count of parameters automated at
//!   once. A segment that finds every slot taken is sent **as a ramp: its
//!   end value at once, on its event frame**, so the parameter never holds
//!   its start value. On the audio thread nothing allocates.
//! - **Parameters per chunk:** a chunk's parameter changes hold
//!   [`ParameterChanges`]' inline 16 queues (a protocol limit every
//!   automation source shares; past it a new parameter's points are
//!   dropped, never spilled onto the heap). A segment whose point is
//!   dropped for want of a queue keeps its slot, past its arrival if need
//!   be, and sends the value it has reached on the first frame of a later
//!   take that has room: with more than 16 parameters moving at once, a
//!   starved segment's end value lands late, never not at all.
//!
//! A re-prepare or a reset drops every segment in flight.

use tutti_graph::ParamSegment;
use tutti_types::Samples;

use super::automation_node;
use super::param_automation_source::stride_for;
#[cfg(doc)]
use super::param_automation_source::MAX_POINTS;
use crate::protocol::{ParamAddress, ParameterChanges};

/// Segments the node follows at once (one per parameter address): the
/// slots [`SegmentPoints::prepare`] allocates.
pub(super) const MAX_SEGMENTS: usize = 1024;

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
    /// A point it owes: one was dropped for want of a queue, so it sends
    /// where it has got to at the next chance.
    owed: bool,
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
    /// ramp: its end value, now.
    pub(super) fn start(
        &mut self,
        changes: &mut ParameterChanges,
        address: ParamAddress,
        segment: ParamSegment,
        frame: u64,
        at: usize,
    ) {
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
        let owed = !point(changes, address, at, segment.raw_value_after(Samples(0)));
        self.tracks.push(Track {
            address,
            segment,
            start: frame,
            next: frame + 1,
            end: arrives.map(|j| frame + j.get() as u64),
            owed,
        });
    }

    /// The points the segments in flight put on absolute frames
    /// `from..to`, the first of them at chunk frame `at`.
    pub(super) fn points(&mut self, changes: &mut ParameterChanges, from: u64, to: u64, at: usize) {
        if from >= to {
            return;
        }
        let stride = stride_for(self.chunk).max(1);
        let last = self.chunk.saturating_sub(1);
        self.tracks.retain_mut(|t| {
            let value = |t: &Track, f: u64| {
                let j = f.saturating_sub(t.start);
                t.segment
                    .raw_value_after(Samples(usize::try_from(j).unwrap_or(usize::MAX)))
            };
            // A point owed from before: where the segment has got to, on
            // this take's first frame.
            if t.owed && t.next <= from {
                t.owed = !point(changes, t.address, at, value(t, from));
                if t.end.is_some_and(|e| e <= from) {
                    // Arrived while starved: delivered now, or still owed.
                    t.next = to;
                    return t.owed;
                }
            }
            let mut f = t.next.max(from);
            while f < to {
                let c = at + (f - from) as usize;
                let arrived = t.end == Some(f);
                if arrived || c.is_multiple_of(stride) || c == last {
                    let placed = point(changes, t.address, c, value(t, f));
                    t.owed = !placed;
                    if arrived {
                        t.next = to;
                        // Done once its end value is placed.
                        return !placed;
                    }
                }
                f += 1;
            }
            t.next = to;
            true
        });
    }
}

/// A point at chunk frame `at`; `false` when the chunk had no queue for it.
fn point(changes: &mut ParameterChanges, address: ParamAddress, at: usize, value: f32) -> bool {
    let offset = i32::try_from(at).unwrap_or(i32::MAX);
    automation_node::add_point(changes, address, offset, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ParamId;
    use tutti_graph::SegmentShape;

    const A: ParamAddress = ParamAddress::Opaque(ParamId::new(7));

    /// `(chunk frame, value)` of `address`'s queue.
    fn queue(changes: &ParameterChanges, address: ParamAddress) -> Vec<(i32, f32)> {
        changes
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

    /// A segment spanning several chunks sends points along itself in each,
    /// on the stride and each chunk's last frame, and lands its end value
    /// exactly on the frame it arrives; then nothing more.
    ///
    /// Mutations (run), each fails: no point on the arrival frame (only the
    /// stride and the last frame) → the end value lands late; dropping a
    /// track after each run of points (`*slot = None` for `t.next = to`) →
    /// the next chunk gets no points; never tracking (treating every segment
    /// as arrived) → only the first point is sent.
    #[test]
    fn a_segment_sends_points_until_it_lands() {
        let seg = ParamSegment::foreign(7, 0.0, 1.0, Samples(150), SegmentShape::PowerIn(2.0));
        let mut sp = SegmentPoints::default();
        sp.prepare(MAX_SEGMENTS);
        let chunk = 64;
        let mut chunks = Vec::new();
        for k in 0..4u64 {
            let mut changes = ParameterChanges::default();
            sp.begin(chunk);
            let base = k * chunk as u64;
            if k == 0 {
                // The event on frame 10.
                sp.points(&mut changes, 0, 10, 0);
                sp.start(&mut changes, A, seg, 10, 10);
                sp.points(&mut changes, 10, 64, 10);
            } else {
                sp.points(&mut changes, base, base + 64, 0);
            }
            chunks.push(queue(&changes, A));
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
        sp.prepare(MAX_SEGMENTS);
        sp.begin(64);
        let mut changes = ParameterChanges::default();
        sp.start(&mut changes, A, seg, 0, 0);
        sp.start(&mut changes, b, seg, 0, 0);
        sp.points(&mut changes, 1, 20, 1);
        sp.interrupt(A);
        sp.points(&mut changes, 20, 64, 20);
        assert!(queue(&changes, A).iter().all(|p| p.0 < 20));
        assert_eq!(queue(&changes, b).last().map(|p| p.0), Some(63));
    }

    /// More segments in flight than a chunk has queues (20 for 16), the
    /// shortest ones starved through their arrival: every parameter still
    /// ends on its segment's end value, and one that had a queue lands it
    /// on its arrival frame.
    ///
    /// Mutations (run), each fails: a starved point not owed in `points`
    /// (`owed` always `false`: a segment starved through its arrival is
    /// never dropped and sends forever); a track dropped on its arrival frame whether or not
    /// its point was placed (`return false`); the slots capped at 16 with
    /// the start value sent past them (the bug this replaces).
    #[test]
    fn more_segments_than_queues_each_reach_their_end() {
        let n = 20u32;
        let addr = |k: u32| ParamAddress::Opaque(ParamId::new(100 + k));
        // Address `k` arrives `310 - 10k` frames in: the late addresses,
        // which the queues starve, arrive first.
        let seg = |k: u32| {
            ParamSegment::foreign(
                100 + k,
                0.0,
                1.0,
                Samples(311 - 10 * k as usize),
                SegmentShape::PowerIn(2.0),
            )
        };
        let mut sp = SegmentPoints::default();
        sp.prepare(MAX_SEGMENTS);
        let mut latest = vec![f32::NAN; n as usize];
        let mut landed_at = vec![None; n as usize];
        for chunk in 0..8u64 {
            let mut changes = ParameterChanges::default();
            sp.begin(64);
            let base = chunk * 64;
            if chunk == 0 {
                for k in 0..n {
                    sp.start(&mut changes, addr(k), seg(k), 0, 0);
                }
                sp.points(&mut changes, 1, 64, 1);
            } else {
                sp.points(&mut changes, base, base + 64, 0);
            }
            assert!(changes.queues.len() <= 16, "never past the inline queues");
            if chunk == 7 {
                // Every segment arrived and delivered: nothing more is sent.
                assert!(changes.queues.is_empty(), "done segments are dropped");
            }
            for k in 0..n {
                if let Some(&(c, v)) = queue(&changes, addr(k)).last() {
                    latest[k as usize] = v;
                    if v == 1.0 && landed_at[k as usize].is_none() {
                        landed_at[k as usize] = Some(base + c as u64);
                    }
                }
            }
        }
        for k in 0..n {
            assert_eq!(latest[k as usize], 1.0, "address {k} reached its end value");
        }
        // Address 0 always had a queue: its end value on its arrival frame.
        let arrival = seg(0).arrives_after().expect("arrives").get() as u64;
        assert_eq!(landed_at[0], Some(arrival));
    }

    /// A segment that finds every slot taken is sent as a ramp: its end
    /// value at once, on its event frame, never its start value held.
    ///
    /// Mutation (run): the fallback sending the start value
    /// (`raw_value_after(Samples(0))`) → fails.
    #[test]
    fn a_segment_past_the_slots_is_sent_as_a_ramp() {
        let mut sp = SegmentPoints::default();
        sp.prepare(2);
        let seg = |id: u32| ParamSegment::foreign(id, 0.0, 1.0, Samples(500), SegmentShape::Linear);
        let mut changes = ParameterChanges::default();
        sp.begin(64);
        for id in [1, 2, 3] {
            sp.start(
                &mut changes,
                ParamAddress::Opaque(ParamId::new(id)),
                seg(id),
                5,
                5,
            );
        }
        assert_eq!(
            queue(&changes, ParamAddress::Opaque(ParamId::new(3))),
            vec![(5, 1.0)]
        );
        assert!(queue(&changes, ParamAddress::Opaque(ParamId::new(1)))[0].1 < 0.01);
    }
}
