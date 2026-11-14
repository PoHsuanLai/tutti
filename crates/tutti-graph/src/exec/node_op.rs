//! Running a node op: its params, its mode and any crossfade, and the call
//! into the node.

use std::cell::Cell;

use tutti_types::{MaxBlock, Tail};

use super::state::{Inject, Unit};
use super::tail_elapsed;
use crate::arena::Role;
use crate::command::Deliveries;
use crate::event::PortCount;
use crate::event::{Event, EventWriter, SortedEvents, merge_into};
use crate::history::TransportHistory;
use crate::io::Io;
use crate::loss::Dropped;
use crate::node::{ConstantMask, Cx, Env, InPlaceMask, MAX_PORTS, Node, SilenceMask, Status};
use crate::param::ParamInput;
use crate::plan::{Direct, Form, NodeRec, Plan};
use crate::slots::{AudioSlots, EventSlots, SlotFlags};

/// [`Op::Node`](crate::Op::Node): run `u` for `h.rec`: the plain op for a unit whose mode asks
/// nothing of it, else the one that applies its mode. Returns whether a
/// crossfade running there reached its end this block.
#[inline(always)]
pub(crate) fn node_dispatch<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
) -> bool {
    if u.mode.is_plain() {
        return run_node_op(u, head, st);
    }
    super::mode_op::moded_node_op(u, head, st)
}

/// The node op by its borrow form (a crossfade running there takes its own).
/// Returns whether that crossfade reached its end this block.
#[inline(always)]
pub(super) fn run_node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
) -> bool {
    let (rec, plan, frames) = (head.rec, head.plan, head.frames);
    debug_assert_eq!(
        u.generation, rec.generation,
        "unit generation matches the plan"
    );
    let borrows = &plan.nodes.borrows[..];
    if u.fade.is_some() {
        // A crossfade runs here: both units, then the blend. Off the hot
        // path, whatever the form.
        let ports = rec.ports(&plan.nodes.slots);
        return fading_node_op(u, head, st, ports, borrows);
    }
    // Each arm hands `node_op` its port slots. For the three direct forms
    // they are literal one-element arrays, so once `node_op` is inlined every
    // per-port loop in it — masks, skip, finish — folds to straight-line
    // code.
    match rec.form {
        Form::Source { out } => {
            node_op(u, head, st, [&[], &[out], &[], &[]], |call, node, st| {
                let mut outs = [st.arena.slot_mut(out, frames)];
                call.process(node, &[], &mut outs)
            });
        }
        Form::Split { input, out } => node_op(
            u,
            head,
            st,
            [&[input], &[out], &[], &[]],
            |call, node, st| {
                let (i, o) = st.arena.pair(input, out, frames);
                call.process(node, &[i], &mut [o])
            },
        ),
        Form::InPlace { slot } => node_op(
            u,
            head,
            st,
            [&[slot], &[slot], &[], &[]],
            |call, node, st| {
                let mut outs = [st.arena.slot_mut(slot, frames)];
                call.process(node, &[&[]], &mut outs)
            },
        ),
        // The stereo shapes: fixed widths, so `direct_op` gets the same
        // straight-line call path.
        Form::Direct(d) => {
            let slots = &plan.nodes.slots[..];
            match (d.ins, d.outs) {
                (0, 2) => direct_op::<0, 2, A, F, E>(u, head, st, &d, slots),
                (1, 2) => direct_op::<1, 2, A, F, E>(u, head, st, &d, slots),
                (2, 1) => direct_op::<2, 1, A, F, E>(u, head, st, &d, slots),
                (2, 2) => direct_op::<2, 2, A, F, E>(u, head, st, &d, slots),
                _ => unreachable!("rule 7 admits only `Direct::SHAPES`"),
            }
        }
        // Port tables are stack arrays; pick the smallest bucket that fits
        // so a two-port node does not initialise 64 entries per call. An
        // event-free node builds no event table at all, whatever its width.
        Form::Audio => {
            let ports = rec.ports(&plan.nodes.slots);
            node_op(u, head, st, ports, |call, node, st| {
                match rec.ain.max(rec.aout) {
                    0..=4 => call.run_audio::<4, A>(node, &mut *st.arena, borrows),
                    5..=16 => call.run_audio::<16, A>(node, &mut *st.arena, borrows),
                    _ => call.run_audio::<MAX_PORTS, A>(node, &mut *st.arena, borrows),
                }
            });
        }
        Form::General => {
            let ports = rec.ports(&plan.nodes.slots);
            node_op(u, head, st, ports, |call, node, st| {
                let a = rec.ain.max(rec.aout);
                let e = rec.ein.max(rec.eout);
                let (inject, overlay) = (&*st.inject, &st.overlay[..]);
                let extra = Extra { inject, overlay };
                let (arena, events) = (&mut *st.arena, &mut *st.events);
                let dropped = &mut *st.dropped;
                match (a, e) {
                    (0..=4, 0..=4) => {
                        call.run::<4, 4, A, E>(node, arena, events, borrows, extra, dropped)
                    }
                    (0..=16, 0..=4) => {
                        call.run::<16, 4, A, E>(node, arena, events, borrows, extra, dropped)
                    }
                    _ => call.run::<MAX_PORTS, MAX_PORTS, A, E>(
                        node, arena, events, borrows, extra, dropped,
                    ),
                }
            });
        }
    }
    false
}

/// A node op with a crossfade running: the outgoing unit on the op's inputs
/// into its scratch, then the incoming unit as any op runs (events and all,
/// through the general borrow), then the blend into the op's output slots.
/// Returns whether the fade reached its end in this block.
///
/// The outgoing unit runs **first**: an input aliased in place is also the
/// incoming unit's output, and holds the input only until that unit runs.
/// It is handed its own output buffers (never in place), no events, and
/// event writers that accept nothing — see the `fade` module (`src/fade.rs`).
/// Only the op's own slots are touched, so the verifier's rules about the op
/// cover the fade too.
#[cold]
#[inline(never)]
pub(super) fn fading_node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    [ain, aout, ein, eout]: [&[u32]; 4],
    borrows: &[(u32, Role)],
) -> bool {
    let frames = h.frames;
    // The key's params, once for both units: the outgoing one hears the
    // modulation the incoming one does.
    let modulated = h.rec.params.len != 0 || u.params.busy();
    if modulated {
        run_params(u, h, st);
    }
    let params = u.params.inputs(frames);
    let params = &params[..u.params.port_count()];
    let x = u.fade.as_mut().expect("a fading unit");
    let status = {
        let (silent, constant) = in_masks(ain, &*st.flags);
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (i, &s) in ins.iter_mut().zip(ain) {
            *i = st.arena.slot(s, frames);
        }
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        x.scratch.leading_mut(frames, &mut outs[..aout.len()]);
        for o in &mut outs[..aout.len()] {
            o.fill(0.0);
        }
        let evin = [SortedEvents::EMPTY; MAX_PORTS];
        let mut evout: [EventWriter<'_>; MAX_PORTS] =
            std::array::from_fn(|_| EventWriter::detached());
        let io = Io::new(
            h.max,
            frames,
            &ins[..ain.len()],
            &mut outs[..aout.len()],
            silent,
            constant,
            InPlaceMask::NONE,
            &evin[..ein.len()],
            &mut evout[..eout.len()],
        )
        .with_main(h.rec.main)
        .with_params(params);
        let cx = Cx {
            env: h.env,
            arrival: h.rec.arrival,
            history: h.history,
        };
        x.out
            .as_mut()
            .expect("a running fade holds its outgoing unit")
            .process(&cx, io)
    };
    // The outgoing unit's status, applied to its scratch as `finish` applies
    // one to the arena.
    for c in 0..aout.len() {
        let o = x.scratch.slot_mut(c as u32, frames);
        match status {
            Status::Modified | Status::Masked { .. } => {}
            Status::Silent | Status::Idle => o.fill(0.0),
            Status::Constant => {
                let v = o[0];
                o.fill(v);
            }
            Status::Bypass => match ain.get(c) {
                Some(&i) => o.copy_from_slice(st.arena.slot(i, frames)),
                None => o.fill(0.0),
            },
        }
    }

    node_op_with(
        u,
        h,
        st,
        [ain, aout, ein, eout],
        !modulated,
        |call, node, st| {
            let (inject, overlay) = (&*st.inject, &st.overlay[..]);
            let extra = Extra { inject, overlay };
            let (arena, events) = (&mut *st.arena, &mut *st.events);
            let dropped = &mut *st.dropped;
            call.run::<MAX_PORTS, MAX_PORTS, A, E>(node, arena, events, borrows, extra, dropped)
        },
    );

    // The blend: `incoming * g_in + outgoing * g_out` while the fade lasts,
    // the incoming unit alone after it (`CrossfadeCurve::gains`).
    let x = u.fade.as_mut().expect("still fading");
    let n = (x.len - x.done).min(frames);
    for (c, &s) in (0u32..).zip(aout) {
        let y = st.arena.slot_mut(s, frames);
        let out = x.scratch.slot(c, frames);
        for (i, (y, &o)) in y[..n].iter_mut().zip(&out[..n]).enumerate() {
            let (g_in, g_out) = x.curve.gains(x.done + i, x.len);
            *y = *y * g_in + o * g_out;
        }
        st.flags.set(s, 0);
    }
    x.done += n;
    // Never skipped while fading: the next block's skip test reads these.
    u.last_quiet = false;
    u.last_idle = false;
    x.done >= x.len
}

/// Per audio slot: every sample of the block is exact `0.0`.
pub(crate) const SILENT: u8 = 1;

/// Per audio slot: the block holds one value throughout.
pub(super) const CONSTANT: u8 = 2;

#[inline]
pub(super) fn flag(silent: bool, constant: bool) -> u8 {
    (silent as u8 * SILENT) | (constant as u8 * CONSTANT)
}

/// A node's input masks from its input slots' flags: one load and two
/// shifts per channel. Channels past 63 are never set (the masks are 64
/// wide, and `compile` refuses a wider node anyway).
#[inline]
pub(super) fn in_masks<F: SlotFlags + ?Sized>(
    ain: &[u32],
    flags: &F,
) -> (SilenceMask, ConstantMask) {
    let (mut silent, mut constant) = (0u64, 0u64);
    for (c, &s) in (0..64u32).zip(ain) {
        let f = u64::from(flags.get(s));
        silent |= (f & u64::from(SILENT)) << c;
        constant |= ((f & u64::from(CONSTANT)) >> 1) << c;
    }
    (SilenceMask(silent), ConstantMask(constant))
}

/// A node op's constants: its record and the block's.
pub(crate) struct Head<'p, 'e> {
    pub(crate) rec: &'p NodeRec,
    /// The plan, for the param tables.
    pub(crate) plan: &'p Plan,
    pub(crate) frames: usize,
    pub(crate) max: MaxBlock,
    pub(crate) cap: usize,
    pub(crate) env: &'e Env,
    /// The executor's transport history, observed before any op runs this
    /// block: every participant reads the same one, and nobody writes it
    /// until the block is done.
    pub(crate) history: &'e TransportHistory,
}

/// What a node op may write besides its own unit: every buffer through `A`
/// and `E` (the arena and event slots themselves serially, a claimed view
/// of them in parallel), and only its own unit's per-block flags.
pub(crate) struct OpState<'s, A: ?Sized, F: ?Sized, E: ?Sized> {
    pub(crate) arena: &'s mut A,
    pub(crate) flags: &'s mut F,
    pub(crate) events: &'s mut E,
    /// Flushed events waiting for their sinks: every entry serially, only
    /// this unit's in parallel. Either way, entries are matched by unit.
    pub(crate) inject: &'s mut [Inject],
    /// Per plan unit, whether flushed events wait for it: every unit's
    /// serially, only this one's (at index `at`) in parallel. Indexed only
    /// for a node with event inputs, so an event-free call pays nothing.
    pub(crate) has_inject: &'s mut [bool],
    /// Per plan unit, whether a scheduled command lands on it this block;
    /// indexed like `has_inject`.
    pub(crate) has_due: &'s mut [bool],
    /// This unit's index into `has_inject` and `has_due`.
    pub(crate) at: usize,
    /// Where a node's scheduled events are merged into its inputs.
    pub(crate) overlay: &'s mut Vec<Event>,
    pub(crate) due: Deliveries<'s>,
    pub(crate) dropped: &'s mut Dropped,
}

/// Run one node op: the silence skip, flushed-event injection, the call
/// itself (`call_node` borrows the buffers its [`Form`] names and runs the
/// node), and the status bookkeeping.
///
/// `#[inline(always)]` so each [`Form`] gets its own copy: the direct forms
/// pass literal one-element port arrays, and every per-port loop here then
/// folds to straight-line code. That, and not the borrow alone, is most of
/// what the direct forms save.
#[inline(always)]
pub(super) fn node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    ports: [&[u32]; 4],
    call_node: impl FnOnce(&Call<'_, '_>, &mut dyn Node, &mut OpState<'_, A, F, E>) -> Status,
) {
    node_op_with(u, h, st, ports, false, call_node);
}

/// Run a unit's fused param step (see the `param` module docs,
/// `src/param.rs`): each declared param, from its source this plan, into
/// the unit's param buffers. Off the hot path: only a unit with a driven
/// param, or one mid-declick, gets here.
#[inline(never)]
pub(super) fn run_params<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &OpState<'_, A, F, E>,
) {
    let (rec, frames, plan) = (h.rec, h.frames, h.plan);
    let Unit { node, params, .. } = u;
    params.begin();
    let ports = &plan.param_ports[rec.params.range()];
    let mut next = ports.iter().peekable();
    let declared = rec.declared;
    // The state was sized from the unit's shape; a fade's two units declare
    // the same params (checked where it is asked for), so the two agree.
    let n = declared.len().min(params.port_count());
    for (k, &param) in declared.as_slice().iter().enumerate().take(n) {
        let base = node.param_base(k);
        match next.next_if(|p| p.port as usize == k) {
            Some(p) => {
                let events = st.events.ev(p.slot);
                params.port(k, param, frames, Some((p.from, events)), base);
            }
            None => params.port(k, param, frames, None, base),
        }
    }
}

/// [`node_op`], with the unit's params already run this block when
/// `params_done` (a crossfade runs them once for both units).
#[inline(always)]
pub(super) fn node_op_with<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    [ain, aout, ein, eout]: [&[u32]; 4],
    params_done: bool,
    call_node: impl FnOnce(&Call<'_, '_>, &mut dyn Node, &mut OpState<'_, A, F, E>) -> Status,
) {
    let (rec, frames) = (h.rec, h.frames);
    let unit = rec.unit;
    // Flushed events and scheduled commands only ever go to an event input.
    let injected = !ein.is_empty() && st.has_inject[st.at];
    // A bypassed node is sent no events but the closes of what it holds
    // (`mode.rs`): its scheduled commands are dropped.
    let discards = !ein.is_empty() && u.mode.discards_events();
    let scheduled = !ein.is_empty() && !discards && st.has_due[st.at];

    let (in_silent, in_constant) = in_masks(ain, &*st.flags);
    // A raised `Wake` counts as input. Taken for every node every block, so a
    // raise on a node that is not parked is consumed and cannot wake it later.
    let woken = u.node.take_wake();
    let quiet_inputs = !woken
        && !injected
        && !scheduled
        && in_silent.covers(ain.len())
        && ein.iter().all(|&s| st.events.ev(s).is_empty());

    // See the module docs: a node with event inputs parks only on its own
    // say-so (`Status::Idle`); one without parks when its inputs, its last
    // output and its latency plus tail agree.
    let skip = quiet_inputs
        && if ein.is_empty() {
            !ain.is_empty()
                && u.last_quiet
                && tail_elapsed(
                    match u.reported {
                        None => rec.park_after,
                        Some(t) => Tail::Finite(rec.latency).then(t),
                    },
                    u.quiet,
                )
        } else {
            u.last_idle
        };
    // A bypassed node is called until it has been sent the close of what
    // it holds, whatever it says of itself.
    let skip = skip && !(discards && u.mode.holds_input());
    u.quiet = if quiet_inputs {
        u.quiet.saturating_add(frames as u64)
    } else {
        0
    };
    // The fused param step: only for a unit with a modulated param, or one
    // still declicking off one; every other unit pays this one branch. Run
    // whether or not the node is then skipped, so its ramps and declick
    // advance with the timeline as the reference's do.
    if !params_done && (rec.params.len != 0 || u.params.busy()) {
        run_params(u, h, st);
    }
    if skip {
        for &s in aout {
            st.arena.slot_mut(s, frames).fill(0.0);
            st.flags.set(s, SILENT | CONSTANT);
        }
        for &s in eout {
            st.events.ev_mut(s).clear();
        }
        return;
    }

    if injected {
        // Flushed events keep their spacing where it fits in this block;
        // what does not is clamped to its last frame. Merged with this
        // block's own events, ties to the flushed ones (they are older).
        for inj in st.inject.iter_mut().filter(|i| i.live && i.unit == unit) {
            for e in &mut inj.events {
                e.offset = e.offset.clamp_to(frames);
            }
            let slot = ein[inj.port as usize];
            inj.merged.clear();
            let cap_total = inj.merged.capacity();
            merge_into(
                &[&inj.events, st.events.ev(slot)],
                &mut inj.merged,
                cap_total,
            );
        }
    }

    // Scheduled commands landing on this node this block: one more source
    // per port, after everything else, merged into the overlay buffer.
    let mut views = [(0u16, 0u32, 0u32); MAX_PORTS];
    let mut n_views = 0;
    if scheduled {
        let (inject, events) = (&*st.inject, &*st.events);
        let base = |port: u16| -> &[Event] {
            inject
                .iter()
                .find(|i| i.live && i.unit == unit && i.port == port)
                .map_or(&events.ev(ein[port as usize])[..], |i| &i.merged)
        };
        let (n, lost) = st.due.overlay(unit, base, st.overlay, &mut views);
        n_views = n;
        st.dropped.at_node(rec.unit, lost);
    }
    if discards {
        // Every event input is replaced by what it must close, as far as
        // the overlay takes (the rest next block).
        st.overlay.clear();
        for (port, held) in u.mode.held_in().iter_mut().enumerate() {
            let from = st.overlay.len() as u32;
            held.drain_into(st.overlay);
            views[n_views] = (port as u16, from, st.overlay.len() as u32);
            n_views += 1;
        }
        st.has_due[st.at] = false;
    } else if !u.mode.held_in().is_empty() {
        // What each event input is sent, as the node will see it: the
        // port's slot, or what flushed events and scheduled commands made
        // of it. A bypass closes what is held (`mode.rs`).
        for (port, held) in u.mode.held_in().iter_mut().enumerate() {
            let view = views[..n_views].iter().find(|v| usize::from(v.0) == port);
            let flushed = st
                .inject
                .iter()
                .find(|i| i.live && i.unit == unit && usize::from(i.port) == port);
            let events: &[Event] = match (view, flushed) {
                (Some(&(_, a, b)), _) => &st.overlay[a as usize..b as usize],
                (None, Some(i)) if injected => &i.merged,
                _ => &st.events.ev(ein[port])[..],
            };
            held.observe(events);
        }
    }

    // The table only for a unit whose step delivered frames this block;
    // every other node reads `Base` for every param (`Io::param`).
    let table;
    let params: &[ParamInput<'_>] = if u.params.busy() {
        table = u.params.inputs(frames);
        &table[..u.params.port_count()]
    } else {
        &[]
    };
    let reported = Cell::new(None);
    let call = Call {
        reported: &reported,
        env: h.env,
        history: h.history,
        max: h.max,
        frames,
        rec,
        silent: in_silent,
        constant: in_constant,
        cap: rec.event_capacity.map_or(h.cap, |n| n.get() as usize),
        injected,
        scheduled: &views[..n_views],
        params,
    };
    let status = call_node(&call, &mut *u.node, st);
    // This call's report, or the ceiling when it made none.
    u.reported = reported
        .get()
        .and_then(|r| crate::node::narrow_tail(rec.tail, r));
    if scheduled {
        st.has_due[st.at] = false;
    }
    if injected {
        for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
            inj.events.clear();
            inj.merged.clear();
            inj.live = false;
        }
        st.has_inject[st.at] = false;
    }

    u.last_idle = status == Status::Idle;
    finish(
        status,
        frames,
        ain,
        aout,
        rec.in_place,
        &mut *st.arena,
        st.flags,
    );
    // A node with no outputs claims nothing by being "all silent": it is a
    // sink, called for its side effects, and a skip would starve them.
    u.last_quiet = !(aout.is_empty() && eout.is_empty())
        && aout.iter().all(|&s| st.flags.get(s) & SILENT != 0)
        && eout.iter().all(|&s| st.events.ev(s).is_empty());
    if u.keys.is_closing() {
        // A replace took over from a unit that held keys: close them ahead
        // of this block's events, as many as the slot takes; the rest next
        // block.
        for (port, &s) in eout.iter().enumerate() {
            u.keys.close_port(port, st.events.ev_mut(s));
        }
        u.keys.settle();
    }
    for (port, &s) in eout.iter().enumerate() {
        u.keys.observe(port, st.events.ev(s));
    }
}

/// A [`Form::Direct`] node op with `I` inputs and `O` outputs: its port
/// slots as fixed-size arrays, so `node_op`'s per-port loops fold as they do
/// for the one-channel forms, and its buffers borrowed by
/// [`Arena::direct`](crate::arena::Arena::direct).
#[inline(always)]
pub(super) fn direct_op<
    const I: usize,
    const O: usize,
    A: AudioSlots,
    F: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    d: &Direct,
    slots: &[u32],
) {
    let [ain, aout, _, _] = h.rec.ports(slots);
    let ain: &[u32; I] = ain.try_into().expect("a direct record's input count");
    let aout: &[u32; O] = aout.try_into().expect("a direct record's output count");
    let frames = h.frames;
    node_op(u, h, st, [ain, aout, &[], &[]], |call, node, st| {
        let (ins, mut outs) = st.arena.direct::<I, O>(frames, d);
        call.process(node, &ins, &mut outs)
    });
}

/// Event sources a general node call reads besides its slots: flushed
/// events (`inject`) and scheduled commands merged in (`overlay`).
#[derive(Clone, Copy)]
pub(super) struct Extra<'s> {
    inject: &'s [Inject],
    overlay: &'s [Event],
}

/// One node call's constants, shared by every borrow form.
pub(super) struct Call<'p, 'e> {
    env: &'e Env,
    history: &'e TransportHistory,
    max: MaxBlock,
    frames: usize,
    rec: &'p NodeRec,
    silent: SilenceMask,
    constant: ConstantMask,
    /// Events each of the node's event output writers accepts: its declared
    /// `Shape::event_capacity`, or the executor's default.
    cap: usize,
    /// Whether flushed events wait for this call (see the module docs).
    injected: bool,
    /// `(port, start, end)` into the overlay buffer: ports whose events this
    /// block include scheduled commands.
    scheduled: &'p [(u16, u32, u32)],
    /// What each declared param reads this block.
    params: &'p [ParamInput<'p>],
    /// Where the node's [`Io::report_tail`] lands.
    reported: &'p Cell<Option<Tail>>,
}

impl<'p> Call<'p, '_> {
    /// Run an event-free node on buffers already borrowed.
    #[inline]
    fn process<'a>(
        &self,
        node: &mut dyn Node,
        ins: &'a [&'a [f32]],
        outs: &'a mut [&'a mut [f32]],
    ) -> Status
    where
        'p: 'a,
    {
        let io = Io::new(
            self.max,
            self.frames,
            ins,
            outs,
            self.silent,
            self.constant,
            self.rec.in_place,
            &[],
            &mut [],
        )
        .with_main(self.rec.main)
        .with_params(self.params)
        .with_tail(self.reported);
        node.process(&self.cx(), io)
    }

    /// Built at the call, not kept in `Call`: the node takes it by
    /// reference, so whatever holds it is spilled to the stack.
    #[inline]
    fn cx(&self) -> Cx<'_> {
        Cx {
            env: self.env,
            arrival: self.rec.arrival,
            history: self.history,
        }
    }

    /// Borrow an event-free node's audio buffers by its presorted requests
    /// (`borrows` is the plan's whole list; the record names its run) and
    /// run it. `N` must hold the wider of its two sides.
    fn run_audio<const N: usize, A: AudioSlots>(
        &self,
        node: &mut dyn Node,
        arena: &mut A,
        borrows: &[(u32, Role)],
    ) -> Status {
        let (n_in, n_out) = (self.rec.ain as usize, self.rec.aout as usize);
        debug_assert!(n_in <= N && n_out <= N);
        let mut ins: [&[f32]; N] = [&[]; N];
        let mut outs: [&mut [f32]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(
            self.frames,
            &borrows[self.rec.borrows.range()],
            &mut ins,
            &mut outs,
        );
        self.process(node, &ins[..n_in], &mut outs[..n_out])
    }

    /// Borrow a node's audio and event buffers and run it. `N` must hold the
    /// wider audio side and `M` the wider event side. Returns the node's
    /// status, and charges the events its writers refused to `dropped`, by
    /// port.
    fn run<const N: usize, const M: usize, A: AudioSlots, E: EventSlots + ?Sized>(
        &self,
        node: &mut dyn Node,
        arena: &mut A,
        events: &mut E,
        borrows: &[(u32, Role)],
        extra: Extra<'_>,
        dropped: &mut Dropped,
    ) -> Status {
        let Extra { inject, overlay } = extra;
        let rec = self.rec;
        let frames = self.frames;
        let (n_in, n_out) = (rec.ain as usize, rec.aout as usize);
        let (e_in, e_out) = (rec.ein as usize, rec.eout as usize);
        debug_assert!(n_in.max(n_out) <= N && e_in.max(e_out) <= M);

        let mut ins: [&[f32]; N] = [&[]; N];
        let mut outs: [&mut [f32]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(frames, &borrows[rec.borrows.range()], &mut ins, &mut outs);

        let mut evin: [SortedEvents<'_>; M] = [SortedEvents::EMPTY; M];
        let mut evout_bufs: [Option<&mut Vec<Event>>; M] = std::array::from_fn(|_| None);
        events.borrow_sorted(
            &borrows[rec.event_borrows.range()],
            |port, v| evin[port as usize] = SortedEvents::trusted(v, frames),
            |port, v| evout_bufs[port as usize] = Some(v),
        );
        if self.injected {
            for inj in inject.iter().filter(|i| i.live && i.unit == rec.unit) {
                evin[inj.port as usize] = SortedEvents::trusted(&inj.merged, frames);
            }
        }
        for &(port, a, b) in self.scheduled {
            evin[port as usize] = SortedEvents::trusted(&overlay[a as usize..b as usize], frames);
        }
        // One count per port, so a loss is charged to the port that had it.
        let drops: [PortCount; M] = std::array::from_fn(|_| PortCount::default());
        let mut evout: [EventWriter<'_>; M] = std::array::from_fn(|_| EventWriter::detached());
        for ((w, b), d) in evout
            .iter_mut()
            .zip(evout_bufs.iter_mut())
            .zip(&drops)
            .take(e_out)
        {
            let b = b.take().expect("event output borrowed");
            b.clear();
            *w = EventWriter::new(b, self.cap, frames as u32, d);
        }
        let unplaced = Cell::new(0u32);
        let io = Io::new(
            self.max,
            frames,
            &ins[..n_in],
            &mut outs[..n_out],
            self.silent,
            self.constant,
            rec.in_place,
            &evin[..e_in],
            &mut evout[..e_out],
        )
        .with_main(rec.main)
        .with_params(self.params)
        .with_lost(&unplaced)
        .with_tail(self.reported);
        let status = node.process(&self.cx(), io);
        if unplaced.get() != 0 {
            dropped.at_node(rec.unit, unplaced.get());
        }
        for (port, d) in drops.iter().take(e_out).enumerate() {
            let (refused, recovered) = (d.refused.get(), d.recovered.get());
            if refused != 0 || recovered != 0 {
                dropped.at_port(rec.unit, port as u16, refused, recovered);
            }
        }
        status
    }
}

/// Apply a node's [`Status`] to its output slots and their flags.
#[inline(always)]
pub(super) fn finish<A: AudioSlots, F: SlotFlags + ?Sized>(
    status: Status,
    frames: usize,
    ain: &[u32],
    aout: &[u32],
    in_place: InPlaceMask,
    arena: &mut A,
    flags: &mut F,
) {
    match status {
        Status::Modified => {
            for &s in aout {
                flags.set(s, 0);
            }
        }
        Status::Masked {
            silent: sm,
            constant: cm,
        } => {
            for (c, &s) in aout.iter().enumerate() {
                flags.set(s, flag(sm.get(c), cm.get(c) || sm.get(c)));
            }
        }
        Status::Silent | Status::Idle => {
            for &s in aout {
                arena.slot_mut(s, frames).fill(0.0);
                flags.set(s, SILENT | CONSTANT);
            }
        }
        Status::Constant => {
            for &s in aout {
                let buf = arena.slot_mut(s, frames);
                let v = buf[0];
                buf.fill(v);
                flags.set(s, flag(v == 0.0 && v.is_sign_positive(), true));
            }
        }
        Status::Bypass => {
            for (c, &s) in aout.iter().enumerate() {
                match ain.get(c) {
                    Some(_) if in_place.get(c) => {}
                    Some(&i) => {
                        arena.copy_slot(i, s);
                        flags.set(s, flags.get(i));
                    }
                    None => {
                        arena.slot_mut(s, frames).fill(0.0);
                        flags.set(s, SILENT | CONSTANT);
                    }
                }
            }
        }
    }
}
