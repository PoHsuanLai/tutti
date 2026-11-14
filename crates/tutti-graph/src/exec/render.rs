//! Rendering a block: the serial and parallel paths over a plan's ops.

use ringbuf::HeapProd;
use ringbuf::traits::Producer;
use tutti_rt::{RtScope, with_no_denormals};
use tutti_types::Samples;

use super::commit::{Crossfade, retire_fade};
use super::node_op::{Head, OpState, flag, node_dispatch};
use super::ops::{
    audio_ring, capture_op, delay_op, event_delay_op, event_merge_op, event_ring, global_in,
    output_op, sum_op,
};
use super::state::{Inject, State, Unit};
use super::{Clock, Executor, Inbox, Suspension};
use crate::history::TransportHistory;
use crate::meter::meter_op;
use crate::node::{Env, TransportChanges, TransportPosition};
use crate::par::Block;
use crate::plan::{Op, Plan};
use crate::play::PlayClock;
use crate::slots::{EventSlots, SlotFlags};

/// After a block in which a crossfade reached its end: retire each ended
/// fade's outgoing unit into the commit that started it, and start the fade
/// waiting behind it, if any — from the next block on.
#[cold]
#[inline(never)]
pub(super) fn end_fades(
    plan: &Plan,
    store: &mut [Option<Unit>],
    fade_back: &mut HeapProd<Box<Crossfade>>,
    inject: &mut [Inject],
    has_inject: &mut [bool],
) {
    for rec in &plan.nodes.recs {
        let Some(u) = store[rec.store as usize].as_mut() else {
            continue;
        };
        if !u.fade.as_ref().is_some_and(|x| x.done >= x.len) {
            continue;
        }
        let ended = u.fade.take().expect("checked");
        retire_fade(fade_back, ended);
        if let Some(mut next) = u.queued.take() {
            let incoming = next.waiting.take().expect("a queued fade holds its unit");
            // What the unit that just finished fading in wrote for this one
            // (`Node::chase`), delivered at its first block: the injection
            // `rebuild` sized for it, ahead of anything already waiting
            // there. Rotated into place, so nothing allocates.
            if !next.chase.is_empty()
                && let Some(inj) = inject
                    .iter_mut()
                    .find(|i| i.unit == rec.unit && i.port == 0)
            {
                inj.events.extend_from_slice(&next.chase);
                inj.events.rotate_right(next.chase.len());
                inj.live = true;
                has_inject[rec.unit as usize] = true;
            }
            next.chase.clear();
            next.out = Some(std::mem::replace(&mut u.node, incoming));
            u.keys.promote();
            u.fade = Some(next);
            u.quiet = 0;
            u.last_quiet = false;
            u.last_idle = false;
            u.reported = None;
        }
    }
}

impl Executor {
    /// Renders one block of `frames` into `outputs`, reading `inputs`.
    ///
    /// `inputs` must have a slice per global input channel the plan reads, and
    /// `outputs` one per global output channel; every slice at least `frames`
    /// long. Applies queued commits first ([`apply_pending`](Self::apply_pending));
    /// with none queued it never allocates. A caller that sizes the block from
    /// the executor's plan or prepare applies once itself and renders with
    /// [`render_applied`](Self::render_applied). Marks the thread with
    /// [`RtScope::enter`] for the duration.
    ///
    /// # Panics
    ///
    /// If `frames` is zero or exceeds the prepared maximum block.
    pub fn process(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        self.process_with_changes(frames, transport, &TransportChanges::NONE, inputs, outputs);
    }

    /// As [`process`](Self::process), with the transport changing inside the
    /// block: `transport` holds at the first frame, and each of `changes`
    /// from its offset on (a transport command lands on its frame). The block
    /// is **not** split at them. Every node gets the whole block and an
    /// [`Env`] carrying the changes, and scheduled `At::Beat`
    /// commands resolve against the transport in force where playback
    /// reaches their beat.
    ///
    /// # Panics
    ///
    /// As [`process`](Self::process), and if a change is not inside the
    /// block (outside a re-prepare, when any block length is accepted).
    pub fn process_with_changes(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        changes: &TransportChanges,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        with_no_denormals(|| {
            self.apply_pending();
            self.render_applied(frames, transport, changes, inputs, outputs);
        });
    }

    /// As [`process_with_changes`](Self::process_with_changes), without
    /// applying queued commits: the block renders the plan, prepare and rate
    /// the executor holds now, and a commit sent later waits for the next
    /// block.
    ///
    /// For a caller that reads [`plan`](Self::plan) or
    /// [`prepare`](Self::prepare) to size the block (its length, `outputs`)
    /// and must have them still true when it renders: it calls
    /// [`apply_pending`](Self::apply_pending) once, sizes from the result and
    /// renders here. [`process`](Self::process) would apply again, and a
    /// commit sent in between (a wider plan, a smaller `MaxBlock` from a
    /// re-prepare's resume) would change what the sizes were read from.
    ///
    /// `inputs` and `outputs` must have one slice per global input and output
    /// channel of the plan. Slices past those are ignored, serial or on a
    /// pool. Fewer of either is a caller bug: it panics in debug builds, and
    /// in release the block renders silence into what was given, running no
    /// node.
    ///
    /// # Panics
    ///
    /// As [`process_with_changes`](Self::process_with_changes).
    pub fn render_applied(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        changes: &TransportChanges,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let _rt = RtScope::enter();
        // `move` captures `self` as one pointer; the precise capture of every
        // field the body names is a large struct built per block.
        with_no_denormals(move || {
            if let Suspension::Suspended { next, .. } = self.suspension {
                // Between a re-prepare's two commits the units are on the control
                // thread: render silence, for whatever block arrives — the device
                // may be on either side of the change, so no bound applies here.
                // The graph's own state (rings, FIFOs) stands still, but its
                // clock tracks device time: the frame counter advances, and the
                // playhead sees the transport move. A command whose time passes
                // meanwhile lands late when the units are back.
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                let rate = next.sample_rate();
                let mut env = Env {
                    frame: self.clock.frame,
                    sample_rate: rate,
                    block_len: Samples(frames),
                    transport: *transport,
                    changes: *changes,
                    play: PlayClock::ZERO,
                };
                env.play = self.clock.play.begin(transport.playing, env.play_starts());
                self.inbox.commands.observe(&env);
                self.clock.history.observe(&env);
                self.clock.play.end(&env);
                self.clock.frame += Samples(frames);
                return;
            }
            let max = self.prepare.max_block();
            assert!(
                frames > 0 && frames <= max.get(),
                "block of {frames} frames against a max of {}",
                max.get()
            );
            assert!(
                changes.as_slice().iter().all(|c| c.at.index() < frames),
                "a transport change past the end of a {frames}-frame block"
            );
            let Self {
                prepare,
                event_cap,
                inbox:
                    Inbox {
                        queue: _,
                        back: _,
                        fade_back,
                        losses,
                        unreported,
                        edge_fades,
                        commands,
                        applied,
                    },
                plan,
                store,
                state,
                clock:
                    Clock {
                        frame,
                        history,
                        play,
                    },
                dropped,
                suspension: _,
                parked: _,
                chase: _,
                pool,
            } = self;
            let mut env = Env {
                frame: *frame,
                sample_rate: prepare.sample_rate(),
                block_len: Samples(frames),
                transport: *transport,
                changes: *changes,
                play: PlayClock::ZERO,
            };
            env.play = play.begin(transport.playing, env.play_starts());
            // Before any node reads it, and also when there is nothing to render:
            // the transport moved all the same, and so did the play clock.
            history.observe(&env);
            play.end(&env);
            let history: &TransportHistory = history;
            let Some(plan) = plan.as_ref() else {
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                *frame += Samples(frames);
                return;
            };
            if outputs.len() < plan.global_outputs()
                || inputs.len() < usize::from(plan.global_inputs())
            {
                debug_assert!(
                    false,
                    "{} input and {} output slices for a plan with {} global inputs and {} outputs",
                    inputs.len(),
                    outputs.len(),
                    plan.global_inputs(),
                    plan.global_outputs()
                );
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                commands.observe(&env);
                *frame += Samples(frames);
                return;
            }
            let State {
                arena,
                flags,
                events,
                rings,
                audio_fb,
                event_fb,
                taps,
                event_taps,
                meters,
                lanes,
                inject,
                has_inject,
                has_due,
                overlay,
                par,
            } = state;
            // Slices, not `&mut Vec`s: a slice's pointer and length are locals
            // the op loop can keep in registers across the opaque node calls,
            // where a `Vec` behind a reference is reloaded after every one.
            let (flags, events, inject, has_inject, has_due) = (
                &mut flags[..],
                &mut events[..],
                &mut inject[..],
                &mut has_inject[..],
                &mut has_due[..],
            );
            let plan: &Plan = plan;
            let cap = *event_cap;
            commands.gather(&env, plan, *applied, has_due);
            let mut fade_ended = false;

            // Every weighted lane reads its cell once, here: a store while
            // the block runs reaches no op of it, so the serial and the
            // parallel render cannot disagree on when it landed.
            for lane in lanes.iter_mut() {
                lane.snapshot();
            }
            // And every unit its mode (`mode.rs`), for the same reason.
            for rec in &plan.nodes.recs {
                if let Some(u) = store[rec.store as usize].as_mut() {
                    u.mode.snapshot();
                }
            }

            // Feedback reads happen before any op: the delay is a whole
            // `MaxBlock`, so nothing this block's captures queue is due yet.
            for (f, spec) in audio_fb.iter().zip(&plan.audio_feedback) {
                let ring = f.as_ref().expect("built by apply");
                let is_silent = ring.peek_oldest(arena.slot_mut(spec.slot, frames));
                flags.set(spec.slot, flag(is_silent, is_silent));
            }
            for (f, spec) in event_fb.iter_mut().zip(&plan.event_feedback) {
                let fifo = f.as_mut().expect("built by apply");
                let out = &mut events[spec.slot as usize];
                out.clear();
                fifo.pop_due(out, frames);
            }

            let due = commands.deliveries();
            // The cost model's verdict for this plan on this pool, and for a
            // block this long (`Plan::schedule`); either way the same samples.
            let spread = par.as_ref().is_some_and(|p| p.spreads(frames));
            if let (true, Some(par), Some(pool)) = (spread, par.as_mut(), pool.as_ref()) {
                let ended = crate::par::run_block(
                    par,
                    pool,
                    Block {
                        dropped: &mut *dropped,
                        plan,
                        env: &env,
                        history,
                        frames,
                        max,
                        cap,
                        inputs,
                        // The claim table is sized for the plan's width; slices
                        // past it are ignored here as on the serial path.
                        outputs: &mut outputs[..plan.global_outputs()],
                        arena,
                        flags,
                        events,
                        store,
                        rings,
                        audio_fb,
                        event_fb,
                        taps,
                        event_taps,
                        meters: &*meters,
                        lanes: &mut lanes[..],
                        inject,
                        has_inject,
                        has_due,
                        due,
                    },
                );
                fade_ended |= ended;
            } else {
                // Built once per block, not per op: rebuilt per node call it was
                // ~20 stack moves a node (measured on `graph_render`'s
                // `overhead` chains). A node op sets `at`.
                let mut st = OpState {
                    arena,
                    flags,
                    events,
                    inject,
                    has_inject,
                    has_due,
                    at: 0,
                    overlay,
                    due,
                    dropped: &mut *dropped,
                };
                for op in &plan.ops {
                    match *op {
                        Op::GlobalIn { channel, dst } => {
                            global_in(&mut *st.arena, &mut *st.flags, inputs, channel, dst, frames);
                        }
                        Op::Delay { delay, src, dst } => {
                            delay_op(
                                audio_ring(&mut rings[delay as usize]),
                                &mut *st.arena,
                                &mut *st.flags,
                                src,
                                dst,
                                frames,
                            );
                        }
                        Op::Sum { srcs, dst } => {
                            sum_op(
                                &mut *st.arena,
                                &mut *st.flags,
                                &*st.events,
                                &plan.audio_list[srcs.range()],
                                &mut [],
                                &[],
                                dst,
                                frames,
                            );
                        }
                        Op::WeightedSum {
                            srcs,
                            dst,
                            lanes: span,
                            gains,
                        } => {
                            sum_op(
                                &mut *st.arena,
                                &mut *st.flags,
                                &*st.events,
                                &plan.audio_list[srcs.range()],
                                &mut lanes[span.range()],
                                &plan.gain_ports[gains.range()],
                                dst,
                                frames,
                            );
                        }
                        Op::EventDelay { delay, src, dst } => {
                            let lost = event_delay_op(
                                event_ring(&mut rings[delay as usize]),
                                &mut *st.events,
                                src,
                                dst,
                                frames,
                            );
                            st.dropped.charge_delay(plan, delay, lost);
                        }
                        Op::EventMerge { srcs, dst } => {
                            let lost = event_merge_op(
                                &plan.event_list[srcs.range()],
                                dst,
                                &mut *st.events,
                            );
                            st.dropped.count(lost);
                        }
                        Op::Node { unit, .. } => {
                            // One record per node op, lowered at compile time and
                            // checked by the verifier: store index, generation,
                            // arrival, tail, port slots and presorted borrows.
                            let rec = &plan.nodes.recs[unit as usize];
                            let u = store[rec.store as usize]
                                .as_mut()
                                .expect("the delta placed every unit the plan runs");
                            let head = Head {
                                rec,
                                plan,
                                frames,
                                max,
                                cap,
                                env: &env,
                                history,
                            };
                            st.at = unit as usize;
                            fade_ended |= node_dispatch(u, &head, &mut st);
                        }
                        Op::Output {
                            channel,
                            src,
                            delay,
                        } => {
                            let ring = delay.map(|d| audio_ring(&mut rings[d as usize]));
                            output_op(
                                ring,
                                &mut outputs[channel as usize][..frames],
                                &*st.arena,
                                &*st.flags,
                                src,
                            );
                        }
                        Op::Tap { tap, srcs } => {
                            if let Some(tap) = taps[tap as usize].as_mut() {
                                crate::tap::tap_op(
                                    tap,
                                    &*st.arena,
                                    &plan.audio_list[srcs.range()],
                                    env.frame,
                                    frames,
                                );
                            }
                        }
                        Op::EventTap { tap, src } => {
                            if let Some(tap) = event_taps[tap as usize].as_mut() {
                                crate::event_tap::event_tap_op(tap, &*st.events, src, &env);
                            }
                        }
                        Op::Meter { meter, srcs } => {
                            if let Some(meter) = meters[meter as usize].as_ref() {
                                meter_op(
                                    meter,
                                    &*st.arena,
                                    &*st.flags,
                                    &plan.audio_list[srcs.range()],
                                    frames,
                                );
                            }
                        }
                        Op::Capture { feedback, src } => {
                            let ring = audio_fb[feedback as usize]
                                .as_mut()
                                .expect("built by apply");
                            capture_op(ring, &*st.arena, &*st.flags, src, frames);
                        }
                        Op::EventCapture { feedback, src } => {
                            let fifo = event_fb[feedback as usize]
                                .as_mut()
                                .expect("built by apply");
                            let lost = fifo.push(st.events.ev(src));
                            st.dropped.charge_feedback(plan, feedback, u64::from(lost));
                        }
                    }
                }
            }

            dropped.flush(plan, losses, unreported);
            // An edge whose declick fade ended this block tells the editor,
            // which drops it (or its entry) at its next collect. A full
            // ring keeps the report in the lane for the next block.
            for (lane, id) in lanes.iter_mut().zip(&plan.sum_lanes) {
                if let (Some(kind), Some(id)) = (lane.finished(), id)
                    && edge_fades.try_push((*id, kind)).is_ok()
                {
                    lane.acknowledge();
                }
            }
            for f in event_fb.iter_mut().flatten() {
                f.advance(frames);
            }
            if fade_ended {
                end_fades(plan, store, fade_back, inject, has_inject);
            }
            *frame += Samples(frames);
        });
    }
}
