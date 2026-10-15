//! A plugin node's event input, read for its parameters: the parameter
//! events on it (a [`ParamRamp`](tutti_graph::ParamRamp) or a
//! [`ParamSegment`](tutti_graph::ParamSegment), in the plugin's address model)
//! become points in the node's [`ParamDelivery`], through its
//! [`SegmentPoints`], and every other event is handed back on its frame.
//!
//! One walk for both plugin nodes: the out-of-process node runs it for each
//! piece of a chunk (`PluginChunks::take`, in `graph_node`), the in-process
//! VST2 node for each block (`format::vst2_in_process`), so the two read a
//! ramp, a segment and an address the same way.

use tutti_graph::{Cx, Event, EventKind, Offset};
use tutti_types::ParamAddr;

use crate::protocol::ParameterQueue;

use super::automation_node;
use super::param_delivery::ParamDelivery;
use super::segment_points::SegmentPoints;

/// The events of `events` (sorted by frame) on block frames
/// `from..from + n`, placed on chunk frames from `at` on.
///
/// - A ramp is one point, its target on its frame, and stops a segment in
///   flight on its parameter.
/// - A segment is its value on its frame, then its value on the play-grid
///   frames, the chunk edges and its arrival ([`SegmentPoints`]), which
///   this walk emits on the frames between events and up to the end of the
///   piece. A point is a grid point when the play clock at its frame
///   (`cx.play_at`) is a multiple of 32: past a queue's room grid points go
///   first (design doc 014 §9 requirement 6).
/// - Any other event (MIDI, harmony) goes to `other` with its chunk frame.
///
/// `cx` is the node's call: its `Env::frame` is the absolute frame of block
/// frame 0 (where a segment in flight is along itself), and its play clock
/// classifies a segment's first point. `indexed` says how a parameter
/// event's number is read: a VST2 index, or an opaque handle. A parameter
/// event with an address of the other kind, or with no value for this
/// plugin, is skipped. Allocation-free.
#[allow(
    clippy::too_many_arguments,
    reason = "the two plugin nodes keep the delivery and the segments in \
              different places; a struct over them would exist only for this call"
)]
pub(crate) fn take(
    events: &[Event],
    from: usize,
    n: usize,
    at: usize,
    cx: &Cx<'_>,
    indexed: bool,
    segments: &mut SegmentPoints,
    params: &mut ParamDelivery,
    mut other: impl FnMut(&Event, usize),
) {
    let first = events.partition_point(|e| e.offset.index() < from);
    // This call's frame `i` as the node's absolute frame.
    let base = cx.env.frame.get();
    let abs = |i: usize| base + i as u64;
    // Whether absolute frame `f` (in this call) is a play-grid frame: its
    // play clock count a multiple of 32 (design doc 014 §5.3).
    let grid = |f: u64| {
        usize::try_from(f - base)
            .ok()
            .and_then(|i| Offset::new(i, cx.env.block_len))
            .is_some_and(|off| {
                cx.play_at(off)
                    .play
                    .get()
                    .is_multiple_of(ParameterQueue::POINT_SPACING as u64)
            })
    };
    // The frames whose segment points are sent: up to each event's, then
    // the rest of the piece.
    let mut done = from;
    for e in &events[first..] {
        let o = e.offset.index();
        if o >= from + n {
            break;
        }
        let frame = at + o - from;
        segments.points(params, abs(done), abs(o), at + done - from, grid);
        done = o;
        match e.kind {
            EventKind::Ramp(ramp) => {
                let ParamAddr::Id(id) = ramp.addr() else {
                    continue;
                };
                let (Some(address), Some(value)) = (
                    automation_node::address(id, indexed),
                    ramp.foreign_target(id),
                ) else {
                    continue;
                };
                // A ramp stops a segment in flight on its parameter.
                segments.interrupt(address);
                let offset = i32::try_from(frame).unwrap_or(i32::MAX);
                params.point(address, offset, value);
            }
            EventKind::Segment(seg) => {
                // A plugin parameter takes point values: the segment's value
                // on its frame, then points along it until it lands.
                let ParamAddr::Id(id) = seg.addr() else {
                    continue;
                };
                let Some(address) = automation_node::address(id, indexed) else {
                    continue;
                };
                segments.start(params, address, seg, abs(o), frame, grid(abs(o)));
            }
            EventKind::Midi(_) | EventKind::Harmony(_) => other(e, frame),
        }
    }
    segments.points(params, abs(done), abs(from + n), at + done - from, grid);
}
