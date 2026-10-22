//! A plugin node's event input, read for its parameters: the parameter
//! events on it (a [`ParamRamp`](tutti_graph::ParamRamp) or a
//! [`ParamSegment`], in the plugin's address model)
//! become points in the node's [`ParamDelivery`], through its
//! [`SegmentPoints`], and every other event is handed back on its frame.
//!
//! One walk for both plugin nodes: the out-of-process node runs it for each
//! piece of a chunk (`PluginChunks::take`, in `graph_node`), the in-process
//! VST2 node for each block (`format::vst2_in_process`), so the two read a
//! ramp, a segment and an address the same way.

use tutti_graph::{Cx, Event, EventKind, Offset, ParamSegment};
use tutti_types::{ParamAddr, Samples};

use crate::protocol::{ParamAddress, ParamId, ParameterQueue};

use super::param_delivery::ParamDelivery;
use super::segment_points::SegmentPoints;

/// The events of `events` (sorted by frame) on block frames
/// `from..from + n`, placed on chunk frames from `at` on.
///
/// - A ramp moves from where its parameter stands on the frame before its
///   own (the segment in flight there, else the value the node last gave
///   it) to its target, along its shape over its duration: it is sent as
///   the [`ParamSegment`] that makes, so its points are a segment's, its
///   first a corner. A ramp with no duration, or on a parameter the node
///   has given no value yet, is one point, its target on its frame. Either
///   way it stops a segment in flight on its parameter.
/// - A segment is its value on its frame, then its value on the play-grid
///   frames, the chunk edges and its arrival ([`SegmentPoints`]), which
///   this walk emits on the frames between events and up to the end of the
///   piece. A point is a grid point when the play clock at its frame
///   (`cx.play_at`) is a multiple of 32: past a queue's room grid points go
///   first (design doc 014 §9 requirement 6).
/// - Any other event (MIDI, harmony) goes to `other` with its chunk frame.
///
/// The classification is by frame alone: a lane's corner (a breakpoint, a
/// wrap) that lands on a grid frame is a grid point, and is dropped first
/// like one. That loses nothing a grid point would not: the lane's grid
/// piece starting on that frame carries the same value. A ramp's first
/// point is always a corner.
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
                let (Some(address), Some(value)) = (address(id, indexed), ramp.foreign_target(id))
                else {
                    continue;
                };
                // Where the parameter stands on the frame before the ramp's:
                // a param port's ramp starts from there too.
                let stands = abs(o)
                    .checked_sub(1)
                    .and_then(|f| segments.value_at(address, f))
                    .or_else(|| params.current(address));
                match stands {
                    Some(stands) if ramp.duration() > Samples(0) => {
                        let seg =
                            ParamSegment::foreign(id, stands, value, ramp.duration(), ramp.shape());
                        segments.start(params, address, seg, abs(o), frame, false);
                    }
                    _ => {
                        // A step: it stops a segment in flight on its
                        // parameter.
                        segments.interrupt(address);
                        let offset = i32::try_from(frame).unwrap_or(i32::MAX);
                        params.point(address, offset, value);
                    }
                }
            }
            EventKind::Segment(seg) => {
                // A plugin parameter takes point values: the segment's value
                // on its frame, then points along it until it lands.
                let ParamAddr::Id(id) = seg.addr() else {
                    continue;
                };
                let Some(address) = address(id, indexed) else {
                    continue;
                };
                segments.start(params, address, seg, abs(o), frame, grid(abs(o)));
            }
            EventKind::Midi(_) | EventKind::Harmony(_) => other(e, frame),
            // A plugin is no source a lane reads: nothing to re-send.
            EventKind::Resync => {}
        }
    }
    segments.points(params, abs(done), abs(from + n), at + done - from, grid);
}

/// The address a ramp's or segment's number names on a plugin that
/// addresses its parameters by VST2 index (`indexed`) or by opaque handle:
/// the plugin reads a foreign lane's `u32` (`ParamAddr::Id`) back in its
/// own model.
fn address(id: u32, indexed: bool) -> Option<ParamAddress> {
    if indexed {
        i32::try_from(id).ok().map(ParamAddress::Index)
    } else {
        Some(ParamAddress::Opaque(ParamId::new(id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::node::param_delivery::DeliveryCounters;
    use crate::protocol::{ParamAddress, ParamId};
    use std::sync::Arc;
    use tutti_graph::{
        Env, ParamRamp, PlayClock, PlayReading, SegmentShape, Transport, TransportChanges,
        TransportHistory,
    };
    use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate, Samples};

    /// The block's `Env` at absolute frame 0, its play clock starting at
    /// `play`.
    fn env(len: usize, play: u64) -> Env {
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(len),
            transport: Transport::new(false, Bpm(120.0), Beat(0.0), None),
            changes: TransportChanges::NONE,
            play: PlayClock {
                start: PlayReading {
                    epoch: 0,
                    play: Frame(play),
                },
                next_epoch: 0,
            },
        }
    }

    /// **A ramp reaches a plugin along its shape.** In one 64-frame block
    /// whose play clock starts at 1 (frame 31 is the one grid frame): a
    /// ramp over 8 frames to 0 on a parameter the node has given nothing is
    /// a step, its target on its frame; a `PowerIn(2)` ramp to 1 over 40
    /// frames at frame 11 moves from that 0, its points on its frame
    /// (`(1/40)²`) and on the grid frame (`(21/40)²`); a linear ramp at
    /// frame 40 interrupts it from where it stood on frame 39 (`(29/40)²`),
    /// a quarter of the way to 0 on its frame, 0 on its last. The shaped
    /// ramp's own arrival (frame 50) is never sent.
    ///
    /// Mutations (run), each fails: the ramp sent `Linear` (`ramp.shape()`
    /// ignored) → frame 11 reads 1/40. The start read from the delivery only
    /// (`segments.value_at` skipped) → frame 40 starts from the grid point,
    /// `(21/40)²`. A ramp's duration ignored (every ramp a step) → frame 11
    /// reads 1. `ParamDelivery::current` reading only what was handed over
    /// → the shaped ramp finds no start and is a step.
    #[test]
    fn a_ramp_reaches_a_plugin_along_its_shape() {
        const TOL: f64 = 1e-6;
        let address = ParamAddress::Opaque(ParamId::new(7));
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([address], Arc::clone(&counters));
        params.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let len = 64;
        let env = env(len, 1);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
        };
        let at = |i: usize, ramp: ParamRamp| {
            Event::ramp(Offset::new(i, Samples(len)).expect("inside"), ramp)
        };
        let events = [
            at(0, ParamRamp::foreign(7, 0.0, Samples(8))),
            at(
                11,
                ParamRamp::foreign(7, 1.0, Samples(40)).with_shape(SegmentShape::PowerIn(2.0)),
            ),
            at(40, ParamRamp::foreign(7, 0.0, Samples(4))),
        ];
        take(
            &events,
            0,
            len,
            0,
            &cx,
            false,
            &mut segments,
            &mut params,
            |_, _| {},
        );
        let points: Vec<(i32, f64)> = params.changes().queues[0]
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();
        let stood = (29.0f64 / 40.0).powi(2);
        let want = [
            (0, 0.0),
            (11, (1.0f64 / 40.0).powi(2)),
            (31, (21.0f64 / 40.0).powi(2)),
            (40, stood * 0.75),
            (43, 0.0),
        ];
        assert_eq!(points.len(), want.len(), "{points:?}");
        for ((o, v), (wo, wv)) in points.iter().zip(want) {
            assert_eq!(*o, wo, "{points:?}");
            assert!((v - wv).abs() <= TOL, "frame {o}: {v} vs {wv}");
        }
    }

    /// **The play clock decides which points are grid points.** A block
    /// whose play clock starts at 10 takes a segment on each of frames
    /// 0..300 (two frames long, holding alternating values) into one
    /// parameter whose queue holds 259 points (a 64-frame chunk's room).
    /// Nine of the 300 starts fall on a frame whose play count is a multiple
    /// of 32 (22, 54, .. 278): past the room those nine go first, counted as
    /// grid drops, and only then are corners merged: 300 - 259 - 9, less
    /// the one start that follows the refused grid point at 278 and so
    /// repeats the queue's last value (skipped, changes only). The last
    /// segment's arrival repeats its value too. None is left on a grid
    /// frame.
    ///
    /// Mutation (run): `take`'s grid test always false (`let grid = |_: u64|
    /// false`) → no grid drops, 42 merges → fails. Mutation (run): the
    /// block's frame for the play clock (`f.is_multiple_of(32)`) → the drops
    /// land on 0, 32, .. → fails.
    #[test]
    fn a_full_queue_drops_the_play_grid_first() {
        let address = ParamAddress::Opaque(ParamId::new(7));
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([address], Arc::clone(&counters));
        let room = params.prepare(64);
        assert_eq!(room, 259);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let len = 512;
        let env = env(len, 10);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
        };
        let events: Vec<Event> = (0..300)
            .map(|i| {
                let v = if i % 2 == 0 { 0.25 } else { 0.75 };
                let seg = ParamSegment::foreign(7, v, v, Samples(2), SegmentShape::Linear);
                Event::segment(Offset::new(i, Samples(len)).expect("inside"), seg)
            })
            .collect();
        take(
            &events,
            0,
            len,
            0,
            &cx,
            false,
            &mut segments,
            &mut params,
            |_, _| {},
        );
        let stats = counters.snapshot();
        assert_eq!(stats.grid_points_dropped, 9, "the nine grid frames");
        assert_eq!(stats.points_merged, 300 - 259 - 9 - 1);
        let offsets: Vec<i32> = params.changes().queues[0]
            .points
            .iter()
            .map(|p| p.sample_offset)
            .collect();
        assert_eq!(offsets.len(), room);
        assert!(
            offsets.iter().all(|&o| (10 + o) % 32 != 0),
            "no point left on a grid frame: {offsets:?}"
        );
    }
}
