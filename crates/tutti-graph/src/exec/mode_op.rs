//! The node op of a unit whose mode asks something of it (`mode.rs`, doc 013
//! "Bypass and mute are graph state"): feeding its bypass line, running a fade
//! between modes, muting, and parking.
//!
//! Written once, generic over the slots like every op, so the serial and the
//! parallel executor run the same code; it touches only the op's own slots and
//! its own unit.

use super::*;
use crate::mode::{MixState, NodeMode};

/// [`node_dispatch`] for a unit that is not plain steady Active: toggle if
/// the block's mode snapshot differs, then either park the node (the aligned
/// input path alone) or call it and blend its output with the aligned input.
/// Returns whether a crossfade running at the unit ended this block.
#[inline(never)]
pub(super) fn moded_node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
) -> bool {
    let (rec, plan, frames) = (head.rec, head.plan, head.frames);
    let [ain, aout, ein, eout] = rec.ports(&plan.nodes.slots);

    // The mix's lane, before anything reads the block's mix (it moves while
    // the node is parked too, as the mix's snapshot does).
    let source = plan.param_ports[rec.params.range()]
        .last()
        .filter(|p| p.is_mix())
        .map(|p| (p.from, &st.events.ev(p.slot)[..]));
    u.mode.drive_step(frames, source);

    let want = u.mode.seen();
    if want != u.mode.heading() && u.mode.toggle(want) {
        // Un-bypass: its state is stale. Reset before the first call.
        u.node.reset();
        u.quiet = 0;
        u.last_quiet = false;
        u.last_idle = false;
        u.reported = None;
    }

    if u.mode.is_parked() {
        park(u, head, st, [ain, aout, ein, eout]);
        return false;
    }

    // The aligned input: fed through the line every block (the line is the
    // node's own history), and kept beside the node's output when this
    // block's blend needs it. Before the call: a node that works in place
    // overwrites its input.
    let main = u.mode.inputs();
    let need = u.mode.needs_input();
    let mut input_silent = 0u64;
    for (c, &s) in ain.iter().enumerate().take(main) {
        let silent = st.flags.get(s) & SILENT != 0;
        let src = st.arena.slot(s, frames);
        if u.mode.align(c, src, silent, need, frames) {
            input_silent |= 1 << c;
        }
    }

    let ended = run_node_op(u, head, st);

    let seg = u.mode.step(frames);
    if seg.is_steady() && seg.steady == NodeMode::Active && seg.mix.is_wet() {
        return ended;
    }
    if need {
        u.mode.fold_aligned(frames);
    }
    let lane = u.mode.lane(frames);
    let main = u.mode.outputs();
    for (c, &s) in aout.iter().enumerate() {
        let x = (c < main && need).then(|| u.mode.passed(c, frames));
        seg.blend(st.arena.slot_mut(s, frames), x, lane);
        if seg.is_steady() {
            match seg.steady {
                NodeMode::Active => {
                    // A blend off the ends: silent only where both of its
                    // terms are (a channel with no input has one term, and
                    // at DRY it is silence).
                    let node_silent = st.flags.get(s) & SILENT != 0;
                    let no_input = c >= main;
                    let x_silent = no_input || u.mode.passes_silence(c, input_silent);
                    match seg.mix.state() {
                        MixState::Wet => {}
                        MixState::Dry => st.flags.set(s, flag(x_silent, x_silent)),
                        MixState::Between => {
                            let silent = node_silent && x_silent;
                            st.flags.set(s, flag(silent, silent));
                        }
                        MixState::Moving => st.flags.set(s, 0),
                    }
                }
                NodeMode::Muted => st.flags.set(s, SILENT | CONSTANT),
                NodeMode::Bypassed => {
                    let silent = c >= main || u.mode.passes_silence(c, input_silent);
                    st.flags.set(s, flag(silent, silent));
                }
            }
        } else {
            st.flags.set(s, 0);
        }
    }
    ended
}

/// A parked node: not called. Its audio outputs are the aligned input (the
/// bypass line, or the input itself when the node has no latency), its event
/// outputs only what its held keys still owe, its events discarded.
fn park<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    [ain, aout, ein, eout]: [&[u32]; 4],
) {
    let rec = head.rec;
    // Its params go on stepping, as a skipped node's do: their ramps and
    // declick follow the timeline while it is out.
    if rec.params.len != 0 || u.params.busy() {
        run_params(u, head, st);
    }
    if !u.mode.is_stale() {
        // The first parked block: what the node held on its event outputs
        // is closed downstream, ahead of nothing (it writes no events now).
        u.mode.park();
        u.keys.owe_held();
    }

    // The outputs are the inputs delayed by the declared latency: output `c`
    // is input `c` (main buses of one width naming no placement), or the fold
    // of the main buses (`pass.rs`).
    if u.mode.folds() {
        park_folded(u, head, st, ain, aout);
    } else {
        park_by_index(u, head, st, ain, aout);
    }

    for (port, &s) in eout.iter().enumerate() {
        let events = st.events.ev_mut(s);
        events.clear();
        u.keys.close_port(port, events);
    }
    u.keys.settle();

    // Events sent to a parked node are discarded: flushed ones and commands.
    if !ein.is_empty() {
        let unit = rec.unit;
        if st.has_inject[st.at] {
            for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
                inj.events.clear();
                inj.merged.clear();
                inj.live = false;
            }
            st.has_inject[st.at] = false;
        }
        st.has_due[st.at] = false;
    }
    u.quiet = 0;
    u.last_quiet = false;
    u.last_idle = false;
}

/// A parked node's outputs by index: output `c` is input `c`.
fn park_by_index<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    ain: &[u32],
    aout: &[u32],
) {
    let frames = head.frames;
    let main = u.mode.inputs();
    for (c, &s) in aout.iter().enumerate() {
        if c >= main {
            st.arena.slot_mut(s, frames).fill(0.0);
            st.flags.set(s, SILENT | CONSTANT);
            continue;
        }
        let i = ain[c];
        let silent_in = st.flags.get(i) & SILENT != 0;
        match u.mode.ring(c) {
            None => {
                if i != s {
                    st.arena.copy_slot(i, s);
                    st.flags.set(s, st.flags.get(i));
                }
            }
            Some(ring) => {
                let silent = if i == s {
                    ring.run_in_place(st.arena.slot_mut(s, frames), silent_in)
                } else {
                    let (src, dst) = st.arena.pair(i, s, frames);
                    ring.run(src, dst, silent_in)
                };
                st.flags.set(s, flag(silent, silent));
            }
        }
    }
}

/// A parked node's outputs as the fold of its main buses: every input through
/// its ring into the scratch first (an output may share an input's slot), then
/// each output channel folded from the scratch.
fn park_folded<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    ain: &[u32],
    aout: &[u32],
) {
    let frames = head.frames;
    let mut silent_in = 0u64;
    for (c, &i) in ain.iter().enumerate().take(u.mode.inputs()) {
        let silent = st.flags.get(i) & SILENT != 0;
        if u.mode
            .align(c, st.arena.slot(i, frames), silent, true, frames)
        {
            silent_in |= 1 << c;
        }
    }
    let outputs = u.mode.outputs();
    for (c, &s) in aout.iter().enumerate() {
        if c >= outputs {
            st.arena.slot_mut(s, frames).fill(0.0);
            st.flags.set(s, SILENT | CONSTANT);
            continue;
        }
        u.mode
            .fold_aligned_into(c, st.arena.slot_mut(s, frames), frames);
        let silent = u.mode.passes_silence(c, silent_in);
        st.flags.set(s, flag(silent, silent));
    }
}
