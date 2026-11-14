//! The plan's non-node ops: global inputs, delays, sums, event merges,
//! outputs and captures.

use super::node_op::{CONSTANT, SILENT, flag};
use super::state::Ring;
use crate::arena::Role;
use crate::event::{Event, merge_into};
use crate::gain::{self, SumLane, Weight};
use crate::kernels::{AudioRing, EventFifo};
use crate::node::MAX_PORTS;
use crate::plan::GainPortOp;
use crate::slots::{AudioSlots, EventSlots, SlotFlags, borrow_events, event_pair};

/// An audio delay's ring.
#[inline]
pub(crate) fn audio_ring(r: &mut Option<Ring>) -> &mut AudioRing {
    match r {
        Some(Ring::Audio(a)) => a,
        _ => unreachable!("an audio delay on an event ring"),
    }
}

/// An event delay's FIFO.
#[inline]
pub(crate) fn event_ring(r: &mut Option<Ring>) -> &mut EventFifo {
    match r {
        Some(Ring::Event(f)) => f,
        _ => unreachable!("an event delay on an audio ring"),
    }
}

/// [`Op::GlobalIn`](crate::Op::GlobalIn).
#[inline]
pub(crate) fn global_in<A: AudioSlots, F: SlotFlags + ?Sized>(
    arena: &mut A,
    flags: &mut F,
    inputs: &[&[f32]],
    channel: u16,
    dst: u32,
    frames: usize,
) {
    arena
        .slot_mut(dst, frames)
        .copy_from_slice(&inputs[channel as usize][..frames]);
    flags.set(dst, 0);
}

/// [`Op::Delay`](crate::Op::Delay); `src == dst` is the in-place form.
#[inline]
pub(crate) fn delay_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: &mut AudioRing,
    arena: &mut A,
    flags: &mut F,
    src: u32,
    dst: u32,
    frames: usize,
) {
    let src_silent = flags.get(src) & SILENT != 0;
    let out_silent = if src == dst {
        ring.run_in_place(arena.slot_mut(dst, frames), src_silent)
    } else {
        let (s, d) = arena.pair(src, dst, frames);
        ring.run(s, d, src_silent)
    };
    flags.set(dst, flag(out_silent, out_silent));
}

/// [`Op::Sum`](crate::Op::Sum) or [`Op::WeightedSum`](crate::Op::WeightedSum) of the slots `list` into `dst`: the left
/// fold `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..` in `f32`, one rounding per
/// multiply and per add, never fused. `lanes` is empty for a plain sum, else one per
/// addend; an addend's weight is exactly `1.0` unless its lane says otherwise,
/// and a `1.0` weight is the plain add, no multiply (`crate::gain`).
///
/// An addend flagged [`SILENT`] is skipped whatever its weight: the
/// accumulator starts at `+0.0` and so is never `-0.0`, and `acc + w * (+0.0)`
/// is `acc` bit for bit (a finite `w`; every base is finite). A weight of
/// `0.0` is not skipped by value: `0.0 * NaN` is `NaN`, and the render must
/// not depend on whether a gain happens to be zero. Every lane's state
/// advances **every** block, skipped addend or not. When no addend is live
/// the output is zero-filled and flagged silent; otherwise it is constant when
/// every live addend is **and none is ramping** (a constant times a ramp is
/// not constant). The flags are a claim made from the addends' flags, never
/// from scanning samples. The loop is source-major (it vectorises); each
/// sample still sees the same sequence of adds as the frame-major fold.
///
/// `dst` is the first addend's own slot in the in-place form. That addend is
/// then not borrowed for reading: it is weighted in place (`dst = +0.0 + w *
/// dst`, which turns a `-0.0` into `+0.0`, as `+0.0 + w * a_0` does) and the
/// rest are added into it.
#[inline]
pub(crate) fn sum_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    arena: &mut A,
    flags: &mut F,
    events: &E,
    list: &[u32],
    lanes: &mut [SumLane],
    gains: &[GainPortOp],
    dst: u32,
    frames: usize,
) {
    debug_assert!(!list.is_empty() && list.len() <= MAX_PORTS);
    debug_assert!(lanes.is_empty() || lanes.len() == list.len());
    // Moved on first, for every lane: a silent addend's weight moves too. A
    // driven lane runs the param port's step on its source's events here.
    let mut weights = [Weight::Unit; MAX_PORTS];
    for (k, (w, lane)) in weights.iter_mut().zip(lanes.iter_mut()).enumerate() {
        let source = gains
            .iter()
            .find(|g| usize::from(g.lane) == k)
            .map(|g| (g.from, &events.ev(g.slot)[..]));
        *w = SumLane::weight(lane, frames, source);
    }
    // Requests, then sorted by slot; the order of addition is the `Read`
    // index, among the live addends.
    let mut reqs = [(0u32, Role::Read(0)); MAX_PORTS + 1];
    let mut live_weights = [Weight::Unit; MAX_PORTS];
    let (mut live, mut m) = (0usize, 0usize);
    let (mut in_place, mut constant) = (false, true);
    for (k, &s) in list.iter().enumerate() {
        let f = flags.get(s);
        if f & SILENT != 0 {
            continue;
        }
        constant &= f & CONSTANT != 0
            && !matches!(
                weights[k],
                Weight::Ramp { .. } | Weight::Frames(_) | Weight::Fade { .. }
            );
        if live == 0 && s == dst {
            in_place = true;
        } else {
            reqs[m] = (s, Role::Read(live as u8));
            m += 1;
        }
        live_weights[live] = weights[k];
        live += 1;
    }
    if live == 0 {
        arena.slot_mut(dst, frames).fill(0.0);
        flags.set(dst, SILENT | CONSTANT);
        return;
    }
    reqs[m] = (dst, Role::Write(0));
    m += 1;
    reqs[..m].sort_unstable();
    let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut outs: [&mut [f32]; 1] = [&mut [][..]];
    arena.borrow(frames, &reqs[..m], &mut ins, &mut outs);
    let [out] = outs;
    // `ins[k]` is live addend `k`, but for the first when it is `dst` itself.
    gain::first(out, (!in_place).then(|| ins[0]), live_weights[0]);
    for (a, &w) in ins[1..live].iter().zip(&live_weights[1..live]) {
        gain::add(out, a, w);
    }
    flags.set(dst, flag(false, constant));
}

/// [`Op::EventDelay`](crate::Op::EventDelay). Returns the events its FIFO refused.
#[inline]
pub(crate) fn event_delay_op<E: EventSlots + ?Sized>(
    fifo: &mut EventFifo,
    events: &mut E,
    src: u32,
    dst: u32,
    frames: usize,
) -> u64 {
    let (input, output) = event_pair(events, src, dst);
    output.clear();
    fifo.run(input, output, frames) as u64
}

/// [`Op::EventMerge`](crate::Op::EventMerge) of the slots `list` into `dst`. Returns the events it
/// dropped, which is none: the slot holds all its inputs
/// (`event_slot_capacity`). Counted anyway, in case it ever does.
pub(crate) fn event_merge_op<E: EventSlots + ?Sized>(
    list: &[u32],
    dst: u32,
    events: &mut E,
) -> u64 {
    let mut reqs = [(0u32, Role::Read(0)); MAX_PORTS + 1];
    for (i, &s) in list.iter().enumerate() {
        reqs[i] = (s, Role::Read(i as u8));
    }
    reqs[list.len()] = (dst, Role::Write(0));
    let mut ins: [&[Event]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut output: Option<&mut Vec<Event>> = None;
    borrow_events(
        events,
        &mut reqs[..=list.len()],
        |port, v| ins[port as usize] = v,
        |_, v| output = Some(v),
    );
    let output = output.expect("dst borrowed");
    output.clear();
    let room = output.capacity();
    merge_into(&ins[..list.len()], output, room) as u64
}

/// [`Op::Output`](crate::Op::Output): slot `src` into `out`, through the channel's alignment
/// ring if it has one.
#[inline]
pub(crate) fn output_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: Option<&mut AudioRing>,
    out: &mut [f32],
    arena: &A,
    flags: &F,
    src: u32,
) {
    let s = arena.slot(src, out.len());
    match ring {
        Some(ring) => {
            ring.run(s, out, flags.get(src) & SILENT != 0);
        }
        None => out.copy_from_slice(s),
    }
}

/// [`Op::Capture`](crate::Op::Capture).
#[inline]
pub(crate) fn capture_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: &mut AudioRing,
    arena: &A,
    flags: &F,
    src: u32,
    frames: usize,
) {
    ring.push(arena.slot(src, frames), flags.get(src) & SILENT != 0);
}
