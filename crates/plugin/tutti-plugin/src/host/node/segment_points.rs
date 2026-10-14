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
//! frame, as in a graph param port. At most [`MAX_SEGMENTS`] parameters
//! follow a segment at once: a segment for another parameter past that
//! sends its first point only. The state is a fixed array inside the node,
//! so nothing is allocated, at prepare or on the audio thread; a re-prepare
//! or a reset drops it.

use tutti_graph::ParamSegment;
use tutti_types::Samples;

use super::automation_node;
use super::param_automation_source::stride_for;
#[cfg(doc)]
use super::param_automation_source::MAX_POINTS;
use crate::protocol::{ParamAddress, ParameterChanges};

/// Parameters that can follow a segment at once.
pub(super) const MAX_SEGMENTS: usize = 16;

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
    tracks: [Option<Track>; MAX_SEGMENTS],
    /// The current chunk's length: where its last frame is, and its stride.
    chunk: usize,
}

impl SegmentPoints {
    /// Drop every segment in flight (a reset, a re-prepare).
    pub(super) fn clear(&mut self) {
        self.tracks = [None; MAX_SEGMENTS];
    }

    /// A chunk of `chunk` frames begins.
    pub(super) fn begin(&mut self, chunk: usize) {
        self.chunk = chunk;
    }

    /// A ramp or segment for `address` lands: the segment it was on, if
    /// any, stops here.
    pub(super) fn interrupt(&mut self, address: ParamAddress) {
        for t in &mut self.tracks {
            if t.is_some_and(|t| t.address == address) {
                *t = None;
            }
        }
    }

    /// `segment` for `address` starts at absolute frame `frame`, chunk frame
    /// `at`: its first point, and the segment followed from the next frame
    /// on unless it has already arrived.
    pub(super) fn start(
        &mut self,
        changes: &mut ParameterChanges,
        address: ParamAddress,
        segment: ParamSegment,
        frame: u64,
        at: usize,
    ) {
        self.interrupt(address);
        point(changes, address, at, segment.raw_value_after(Samples(0)));
        let arrives = segment.arrives_after();
        if arrives == Some(Samples(0)) {
            return;
        }
        if let Some(slot) = self.tracks.iter_mut().find(|t| t.is_none()) {
            *slot = Some(Track {
                address,
                segment,
                start: frame,
                next: frame + 1,
                end: arrives.map(|j| frame + j.get() as u64),
            });
        }
    }

    /// The points the segments in flight put on absolute frames
    /// `from..to`, the first of them at chunk frame `at`.
    pub(super) fn points(&mut self, changes: &mut ParameterChanges, from: u64, to: u64, at: usize) {
        let stride = stride_for(self.chunk).max(1);
        let last = self.chunk.saturating_sub(1);
        for slot in &mut self.tracks {
            let Some(t) = slot else {
                continue;
            };
            let mut f = t.next.max(from);
            while f < to {
                let c = at + (f - from) as usize;
                let arrived = t.end == Some(f);
                if arrived || c.is_multiple_of(stride) || c == last {
                    let j = Samples((f - t.start) as usize);
                    point(changes, t.address, c, t.segment.raw_value_after(j));
                }
                if arrived {
                    break;
                }
                f += 1;
            }
            if t.end.is_some_and(|e| e < to) {
                *slot = None;
            } else {
                t.next = to;
            }
        }
    }
}

/// A point at chunk frame `at`.
fn point(changes: &mut ParameterChanges, address: ParamAddress, at: usize, value: f32) {
    let offset = i32::try_from(at).unwrap_or(i32::MAX);
    automation_node::add_point(changes, address, offset, value);
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
}
