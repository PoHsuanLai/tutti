//! Applying a commit: installing its plan, units and state, or checking
//! every unit out for a re-prepare.

use std::collections::BTreeMap;

use ringbuf::traits::Consumer;
use tutti_rt::RtScope;

use super::commit::{Commit, Incoming, Opened, Relined, cut_fades, retire_fade, send_back};
use super::node_op::{CONSTANT, SILENT};
use super::state::{Gone, Inject, Ring, State, Unit};
use super::{Executor, Suspension, par_for};
use crate::NodeKey;
use crate::arena::Arena;
use crate::command::overlay_capacity;
use crate::event::Event;
use crate::event_tap::EventTapWriter;
use crate::gain::SumLane;
use crate::held::Held;
use crate::kernels::{AudioRing, EventFifo};
use crate::meter::MeterWriter;
use crate::plan::{DelayKey, FeedbackKey, Op, ParamPortOp, Plan};
use crate::spec::{EventIn, EventOut};
use crate::tap::TapWriter;
use crate::{EdgeId, EventTapId, MeterId, TapId};

impl Executor {
    /// Applies every queued commit, in the order the editor sent them, and
    /// sends each box back. [`process`](Self::process) calls this first; it is
    /// public so a caller can install a commit without rendering.
    ///
    /// Delay rings, feedback state and units carry over by key (see
    /// [`DelayKey`] and [`FeedbackKey`]). An event delay or event feedback
    /// whose key disappears flushes its pending events to its sink when the
    /// sink survives: they arrive on the sink's next call, keeping their
    /// relative spacing, with the earliest at offset 0.
    ///
    /// # Panics
    ///
    /// If a plan was compiled for a different [`Prepare`](crate::Prepare) than this
    /// executor's (only possible through
    /// [`Editor::package`](crate::Editor::package)).
    pub fn apply_pending(&mut self) {
        let _rt = RtScope::enter();
        while let Some(mut commit) = self.inbox.queue.try_pop() {
            self.apply(&mut commit);
            send_back(&mut self.inbox.back, commit);
        }
    }

    fn apply(&mut self, c: &mut Commit) {
        self.inbox.applied = c.seq;
        if let Some(prepare) = c.suspend {
            // Check every unit out, into the box, for the editor to
            // re-prepare on the control thread. The plan and every piece of
            // delay and feedback state stay where they are, for the resume
            // commit's rebuild to carry — or, after a rate change, to reset.
            //
            // A re-prepare cuts every crossfade: what is checked out is the
            // newest unit at each key (a waiting one, if any), the one the
            // plan's generation names; the others go back with their fades.
            if let Some(plan) = &self.plan {
                for u in &plan.units {
                    let Some(mut unit) =
                        self.store.get_mut(u.idx.0 as usize).and_then(Option::take)
                    else {
                        continue;
                    };
                    // The unit a cut displaced still owes its keys: they close
                    // in the first block after the resume.
                    if let Some(stale) = cut_fades(&mut unit, &mut self.inbox.fade_back) {
                        c.spare_held.push(unit.keys.park(stale));
                    }
                    c.retired.push((u.key, unit.node));
                    c.spare_params.push(unit.params);
                    c.spare_modes.push(unit.mode);
                    self.parked.push((u.key, unit.keys));
                }
            }
            let mut reset_time = false;
            let (old, new) = (
                self.prepare.sample_rate().get(),
                prepare.sample_rate().get(),
            );
            if old != new {
                // `Frame` always means samples at the *current* rate since
                // start. Graph time tracks device time, so the clock and
                // every pending frame-timed command move to the same
                // wall-clock time at the new rate.
                reset_time = true;
                // Frames at the old rate name nothing at the new one.
                self.clock.history.clear();
                let ratio = new / old;
                self.clock.frame = crate::command::rescale(self.clock.frame, ratio);
                self.clock.play.rescale(ratio);
                self.inbox.commands.rescale(ratio, c.seq);
            }
            self.suspension = Suspension::Suspended {
                next: prepare,
                reset_time,
            };
            return;
        }
        // The resume commit of a re-prepare: its plan is for the `Prepare`
        // the suspend carried, and its rebuild carries time-based state
        // unless the rate changed.
        let carry = match std::mem::replace(&mut self.suspension, Suspension::Running) {
            Suspension::Running => true,
            Suspension::Suspended { next, reset_time } => {
                self.prepare = next;
                !reset_time
            }
        };
        let plan = c
            .plan
            .take()
            .expect("a commit carries its plan until applied");
        assert_eq!(
            plan.prepare, self.prepare,
            "a plan compiled for another Prepare: every unit is prepared for the executor's"
        );

        // A key retired or replaced leaves its held keys behind: the rebuild
        // below closes them on the edges they were held on. Not so a key
        // replaced with a fade while one runs: its unit keeps writing, and
        // the replacement waits (`end_fades` closes its keys then).
        let mut departed = Gone::default();
        let n_retire = c.delta.retire.len();
        for (i, p) in c
            .delta
            .retire
            .iter()
            .chain(c.delta.replace.iter().map(|(old, _)| old))
            .enumerate()
        {
            let waits = i >= n_retire
                && c.fades_in(c.delta.replace[i - n_retire].1.key)
                && self
                    .store
                    .get(p.idx.0 as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|u| u.fade.is_some());
            if waits {
                continue;
            }
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                departed
                    .held
                    .push((p.key, u.keys.depart(&mut departed.spare)));
            }
        }
        departed.replaced = c.delta.replace.iter().map(|(_, new)| new.key).collect();
        // The state a replaced node hands its successor, written while it is
        // still in the store (see `Node::chase`).
        departed.chase_events = std::mem::take(&mut self.chase);
        departed.chase_events.clear();
        for (old, new) in &c.delta.replace {
            let running = c.fades_in(new.key)
                && self
                    .store
                    .get(old.idx.0 as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|u| u.fade.is_some());
            let capacity = self
                .plan
                .as_ref()
                .and_then(|p| p.unit(old.key))
                .and_then(|u| u.shape.chase);
            let node = self.store.get(old.idx.0 as usize).and_then(Option::as_ref);
            if let (Some(capacity), Some(u)) = (capacity, node) {
                let from = departed.chase_events.len();
                let mut out =
                    crate::node::Chase::new(&mut departed.chase_events, capacity.get() as usize);
                u.node.chase(&mut out);
                let refused = out.refused();
                if running {
                    // Behind a running fade: the unit fading in is the
                    // predecessor, and the successor starts once that fade
                    // ends. What it wrote waits in the queued crossfade
                    // (`end_fades` delivers it).
                    let fade = c
                        .incoming
                        .iter_mut()
                        .find(|i| i.idx == new.idx)
                        .and_then(|i| i.fade.as_mut());
                    if let Some(fade) = fade {
                        fade.chase.extend_from_slice(&departed.chase_events[from..]);
                        departed.held_refused.push((new.key, refused));
                    }
                    departed.chase_events.truncate(from);
                } else {
                    departed
                        .chased
                        .push((new.key, from..departed.chase_events.len(), refused));
                }
            }
        }
        // A retired key, or one replaced without a fade, goes whole: its
        // unit and any crossfade running or waiting there. A key replaced
        // with a fade stays, for the incoming unit to fade in over it.
        for i in 0..c.delta.retire.len() + c.delta.replace.len() {
            let p = match i.checked_sub(c.delta.retire.len()) {
                None => c.delta.retire[i],
                Some(j) => {
                    let (old, new) = c.delta.replace[j];
                    if c.fades_in(new.key) {
                        continue;
                    }
                    old
                }
            };
            if let Some(u) = self.store.get_mut(p.idx.0 as usize).and_then(Option::take) {
                debug_assert_eq!(
                    u.generation, p.generation,
                    "retiring the unit the delta named"
                );
                c.retired.push((p.key, u.node));
                c.spare_params.push(u.params);
                c.spare_modes.push(u.mode);
                for f in [u.fade, u.queued].into_iter().flatten() {
                    retire_fade(&mut self.inbox.fade_back, f);
                }
            }
        }
        // Cuts: a kept unit's crossfades go, its newest unit stays.
        for p in &c.delta.cuts {
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                debug_assert_eq!(
                    u.generation, p.generation,
                    "cutting the unit the delta named"
                );
                if let Some(stale) = cut_fades(u, &mut self.inbox.fade_back) {
                    departed.held.push((p.key, stale));
                }
            }
        }
        if self.store.len() < c.delta.store_len as usize {
            self.store.resize_with(c.delta.store_len as usize, || None);
        }
        // Taken out and put back, so the list's buffer is not freed here.
        let mut incoming = std::mem::take(&mut c.incoming);
        for Incoming {
            idx,
            generation,
            node,
            fade,
            params,
            mode,
        } in incoming.drain(..)
        {
            let slot = &mut self.store[idx.0 as usize];
            match (fade, slot.as_mut()) {
                (Some(mut x), Some(u)) => {
                    // The key's param state and mode run on across the
                    // replace; the ones built for the incoming unit go back
                    // unused.
                    c.spare_params.push(params);
                    c.spare_modes.push(mode);
                    // A crossfade: the unit at this key fades out — or, with
                    // a fade already running here, this one waits for it,
                    // superseding one that was waiting.
                    u.generation = generation;
                    if u.fade.is_some() {
                        x.waiting = Some(node);
                        if let Some(q) = u.queued.replace(x) {
                            retire_fade(&mut self.inbox.fade_back, q);
                        }
                    } else {
                        x.out = Some(std::mem::replace(&mut u.node, node));
                        u.fade = Some(x);
                        u.quiet = 0;
                        u.last_quiet = false;
                        u.last_idle = false;
                        u.reported = None;
                    }
                }
                (fade, _) => {
                    debug_assert!(slot.is_none(), "store index {} is occupied", idx.0);
                    *slot = Some(Unit::new(generation, node, params, mode));
                    // Nothing to fade from (reachable only through
                    // `package`): the crossfade goes back unused.
                    if let Some(x) = fade {
                        retire_fade(&mut self.inbox.fade_back, x);
                    }
                }
            }
        }
        c.incoming = incoming;
        // A kept unit's bypass line, rebuilt for its new declared latency.
        let mut relined = std::mem::take(&mut c.relined);
        for Relined { idx, latency, line } in relined.drain(..) {
            if let Some(u) = self.store.get_mut(idx.0 as usize).and_then(Option::as_mut)
                && let Some(old) = u.mode.relined(latency, line)
            {
                c.spare_lines.push(old);
            }
        }
        c.relined = relined;

        let old_plan = self.plan.take();
        let new_state = self.rebuild(
            &plan,
            old_plan.as_deref(),
            &mut departed,
            &mut c.opened,
            carry,
        );
        self.chase = std::mem::take(&mut departed.chase_events);
        self.chase.clear();
        let wanted: usize = plan
            .units
            .iter()
            .filter_map(|u| u.shape.chase)
            .map(|n| n.get() as usize)
            .sum();
        self.chase.reserve(wanted);
        c.spare_held.extend(departed.held.drain(..).map(|(_, k)| k));
        c.spare_held.append(&mut departed.spare);
        // Which units' mix this plan drives: a lane's state is built here for
        // a unit that has none (the plan's own sizing, as a driven edge's).
        let max_block = self.prepare.max_block().get();
        for rec in &plan.nodes.recs {
            let driven = plan.param_ports[rec.params.range()]
                .last()
                .is_some_and(ParamPortOp::is_mix);
            if let Some(u) = self.store[rec.store as usize].as_mut() {
                u.mode.set_driven(driven, max_block);
            }
        }
        let old_state = std::mem::replace(&mut self.state, new_state);
        c.old_state = Some(old_state);
        self.plan = Some(plan);
        c.plan = old_plan;
    }

    /// The state for `plan`, carrying what survives out of `self.state`
    /// (which keeps what does not, to be retired with it).
    fn rebuild(
        &mut self,
        plan: &Plan,
        old_plan: Option<&Plan>,
        departed: &mut Gone,
        opened: &mut Opened,
        carry: bool,
    ) -> State {
        let cap = self.event_cap;
        let max_block = self.prepare.max_block().get();
        // After a sample-rate change nothing time-based carries: every ring
        // and FIFO below starts fresh, and the old event FIFOs, left behind,
        // are flushed by the pass that flushes vanished delays (see
        // `Editor::reprepare` for the rule).
        let old = &mut self.state;

        // PDC rings and FIFOs, by key.
        let old_delays: BTreeMap<DelayKey, usize> = old_plan
            .map(|p| {
                p.delays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (d.key, i))
                    .collect()
            })
            .unwrap_or_default();
        // An event FIFO is sized from its source port's declared rate
        // (`Shape::event_capacity`), or the default for one that declared
        // none: the same figure the source's writer enforces.
        // The rate is what the port can hold, note-off room included: the
        // FIFO takes every event the port wrote.
        let rate = |from: crate::EventTapSource| plan.event_tap_rate(from, cap);
        let mut rings: Vec<Option<Ring>> = plan
            .delays
            .iter()
            .map(|d| {
                let carried = old_delays
                    .get(&d.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.rings.get_mut(i))
                    .and_then(Option::take);
                Some(match (d.key, carried) {
                    (key, Some(Ring::Event(mut f))) if let Some(from) = key.event_origin() => {
                        f.retune(d.len);
                        f.resize(rate(from), max_block);
                        Ring::Event(f)
                    }
                    (key, _) if let Some(from) = key.event_origin() => {
                        Ring::Event(EventFifo::sized(d.len, rate(from), max_block))
                    }
                    (_, Some(Ring::Audio(mut r))) => {
                        r.retune(d.len);
                        Ring::Audio(r)
                    }
                    (_, _) => Ring::Audio(AudioRing::new(d.len)),
                })
            })
            .collect();

        // Feedback delays, by key: exactly the edge's delay long.
        let old_fb = |kind: &[crate::plan::FeedbackSpec]| -> BTreeMap<FeedbackKey, usize> {
            kind.iter().enumerate().map(|(i, f)| (f.key, i)).collect()
        };
        let old_afb = old_plan
            .map(|p| old_fb(&p.audio_feedback))
            .unwrap_or_default();
        let old_efb = old_plan
            .map(|p| old_fb(&p.event_feedback))
            .unwrap_or_default();
        let audio_fb = plan
            .audio_feedback
            .iter()
            .map(|f| {
                let carried = old_afb
                    .get(&f.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.audio_fb.get_mut(i))
                    .and_then(Option::take);
                Some(carried.unwrap_or_else(|| AudioRing::new(f.key.delay())))
            })
            .collect();
        let event_fb: Vec<Option<EventFifo>> = plan
            .event_feedback
            .iter()
            .map(|f| {
                let FeedbackKey::Event { from, .. } = f.key else {
                    unreachable!("event feedback keys are events")
                };
                let carried = old_efb
                    .get(&f.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.event_fb.get_mut(i))
                    .and_then(Option::take)
                    .map(|mut fifo| {
                        fifo.resize(rate(crate::EventTapSource::Out(from)), max_block);
                        fifo
                    });
                Some(carried.unwrap_or_else(|| {
                    EventFifo::sized(
                        f.key.delay(),
                        rate(crate::EventTapSource::Out(from)),
                        max_block,
                    )
                }))
            })
            .collect();

        // Taps, by id: a tap the old plan ran carries its writer; one opened
        // since the last commit arrives in the commit and wins over a carried
        // one of the same id (opening an id again replaces it). A writer the
        // new plan does not name stays in the old state (or the commit) and is
        // freed on the control side with it.
        let old_taps: BTreeMap<TapId, usize> = old_plan
            .map(|p| p.taps.iter().enumerate().map(|(i, k)| (*k, i)).collect())
            .unwrap_or_default();
        let taps: Vec<Option<TapWriter>> = plan
            .taps
            .iter()
            .map(|k| {
                opened
                    .taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.taps.swap_remove(at).1)
                    .or_else(|| {
                        old_taps
                            .get(k)
                            .and_then(|&i| old.taps.get_mut(i))
                            .and_then(Option::take)
                    })
            })
            .collect();

        // Event taps, the same way. A tap whose port is missing has an entry
        // and no op, so its writer is carried across plans all the same.
        let old_event_taps: BTreeMap<EventTapId, usize> = old_plan
            .map(|p| {
                p.event_taps
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        let event_taps: Vec<Option<EventTapWriter>> = plan
            .event_taps
            .iter()
            .map(|k| {
                opened
                    .event_taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.event_taps.swap_remove(at).1)
                    .or_else(|| {
                        old_event_taps
                            .get(k)
                            .and_then(|&i| old.event_taps.get_mut(i))
                            .and_then(Option::take)
                    })
            })
            .collect();

        // Meters, the same way: carried by id, replaced by one opened since,
        // and the width each now measures told to its cells.
        let old_meters: BTreeMap<MeterId, usize> = old_plan
            .map(|p| {
                p.meters
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        // Weighted lanes, by edge: what a lane holds (the weight it was last
        // given, and a driven lane's param state) carries across a commit
        // that keeps the edge (not a sample-rate change's reset: a weight is
        // not time), and a new edge starts at its value.
        let mut old_lanes: BTreeMap<EdgeId, usize> = old_plan
            .map(|p| {
                p.sum_lanes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, id)| Some(((*id)?, i)))
                    .collect()
            })
            .unwrap_or_default();
        // The lanes a source drives in this plan: each driven lane's state
        // is sized here, with the plan, so the audio thread allocates none.
        let mut driven = vec![false; plan.sum_lanes.len()];
        for op in &plan.ops {
            if let Op::WeightedSum { lanes, gains, .. } = *op {
                for g in &plan.gain_ports[gains.range()] {
                    driven[lanes.start as usize + usize::from(g.lane)] = true;
                }
            }
        }
        let lanes: Vec<SumLane> = plan
            .sum_lanes
            .iter()
            .zip(&driven)
            .map(|(id, &driven)| match id {
                None => SumLane::plain(),
                Some(id) => SumLane::weighted(
                    opened
                        .gains
                        .get(id)
                        .cloned()
                        .expect("the editor sends a cell for every weighted lane"),
                    old_lanes
                        .remove(id)
                        .and_then(|i| old.lanes.get_mut(i))
                        .map(SumLane::carry)
                        .unwrap_or_default(),
                    driven,
                    max_block,
                ),
            })
            .collect();
        let meters: Vec<Option<MeterWriter>> = plan
            .meters
            .iter()
            .map(|(k, lanes)| {
                let writer = opened
                    .meters
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.meters.swap_remove(at).1)
                    .or_else(|| {
                        old_meters
                            .get(k)
                            .and_then(|&i| old.meters.get_mut(i))
                            .and_then(Option::take)
                    });
                if let Some(w) = &writer {
                    w.set_width(*lanes);
                }
                writer
            })
            .collect();

        // Flush what disappeared, to sinks that survive. Order: injections
        // not yet delivered, then event delays in key order, then event
        // feedback in key order — the reference interpreter's order too.
        let sink_of = |at: EventIn| -> Option<u32> {
            let u = plan.units.binary_search_by_key(&at.node, |u| u.key).ok()?;
            (at.port < plan.units[u].shape.event_in).then_some(u as u32)
        };
        let mut flushed: BTreeMap<(u32, u16), Vec<Event>> = BTreeMap::new();
        let mut take_into = |at: EventIn, events: Vec<Event>| {
            if let Some(u) = sink_of(at) {
                flushed.entry((u, at.port)).or_default().extend(events);
            }
        };
        // What replaced nodes wrote for their successors goes first: it is
        // the oldest, and lands at offset 0 ahead of everything else. A
        // successor with no event input drops it, counted.
        for (key, span, refused) in departed.chased.drain(..) {
            let at = EventIn { node: key, port: 0 };
            let events = &departed.chase_events[span];
            match sink_of(at) {
                Some(u) => {
                    take_into(at, events.to_vec());
                    self.dropped.at_node(u, refused);
                }
                None => {
                    if let Ok(u) = plan.units.binary_search_by_key(&key, |u| u.key) {
                        let lost = u32::try_from(events.len()).unwrap_or(u32::MAX);
                        self.dropped.at_node(u as u32, lost.saturating_add(refused));
                    }
                }
            }
        }
        if let Some(op) = old_plan {
            for inj in old.inject.iter().filter(|i| i.live) {
                let at = EventIn {
                    node: op.units[inj.unit as usize].key,
                    port: inj.port,
                };
                take_into(at, inj.events.clone());
            }
            let mut gone: Vec<(DelayKey, usize)> = old_delays
                .iter()
                .filter(|(k, _)| matches!(k, DelayKey::Event { .. }))
                .map(|(&k, &i)| (k, i))
                .collect();
            gone.sort();
            for (k, i) in gone {
                if let (DelayKey::Event { at, .. }, Some(Some(Ring::Event(f)))) =
                    (k, old.rings.get(i))
                {
                    take_into(at, f.flushed());
                }
            }
            for (k, &i) in &old_efb {
                if let (FeedbackKey::Event { at, .. }, Some(Some(f))) = (k, old.event_fb.get(i)) {
                    take_into(*at, f.flushed());
                }
            }
        }
        // A re-prepare's tables go back to the units that come back; the
        // others are as retired.
        for (key, mut keys) in std::mem::take(&mut self.parked) {
            let same = |a: Option<&Plan>| a.and_then(|p| p.unit(key)).map(|u| u.generation);
            let back = (same(old_plan) == same(Some(plan)))
                .then(|| plan.units.binary_search_by_key(&key, |u| u.key).ok())
                .flatten()
                .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_mut());
            match back {
                Some(u) => {
                    keys.settle();
                    u.keys = keys;
                }
                None => departed.held.push((key, keys.depart(&mut departed.spare))),
            }
        }
        // Keys held on an edge that ended go to the sink that lost it, after
        // everything flushed to that sink (the offs of notes whose ons were in
        // flight). See `held.rs`.
        if let Some(op) = old_plan {
            let source_gone = |n: NodeKey| departed.held.iter().any(|(k, _)| *k == n);
            // What a port holds, and what it owes and has not yet sent.
            let held_of = |from: EventOut| -> (Option<&Held>, Option<&Held>) {
                let port = usize::from(from.port);
                if let Some((_, tables)) = departed.held.iter().find(|(k, _)| *k == from.node) {
                    return (tables.get(port).and_then(Option::as_ref), None);
                }
                let unit = plan
                    .units
                    .binary_search_by_key(&from.node, |u| u.key)
                    .ok()
                    .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_ref());
                match unit {
                    Some(u) => (
                        u.keys.held().get(port).and_then(Option::as_ref),
                        u.keys.owed(from.port),
                    ),
                    None => (None, None),
                }
            };
            let mut closing: BTreeMap<(u32, u16), Vec<Event>> = BTreeMap::new();
            for e in &op.event_edges {
                let &(from, at) = e;
                let in_plan = plan.event_edges.binary_search(e).is_ok();
                if (in_plan && !source_gone(from.node)) || departed.replaced.contains(&at.node) {
                    continue;
                }
                let Some(u) = sink_of(at) else {
                    continue;
                };
                let (held, owed) = held_of(from);
                let owing = [held, owed]
                    .into_iter()
                    .flatten()
                    .filter(|h| !h.is_empty())
                    .collect::<Vec<_>>();
                if owing.is_empty() {
                    continue;
                }
                // A replaced source whose edge (and its delay) goes on: the
                // close travels the delay, behind what is in flight.
                let delay = plan
                    .delays
                    .iter()
                    .position(|d| d.key == DelayKey::Event { at, from });
                if in_plan && let Some(Some(Ring::Event(f))) = delay.map(|i| &mut rings[i]) {
                    let mut offs = Vec::new();
                    owing.iter().for_each(|h| h.close_into(&mut offs));
                    let lost = f.push(&offs);
                    self.dropped.charge_source(from, lost);
                } else {
                    let offs = closing.entry((u, at.port)).or_default();
                    owing.iter().for_each(|h| h.close_into(offs));
                }
            }
            for (k, mut offs) in closing {
                let end = flushed
                    .get(&k)
                    .and_then(|v| v.iter().map(|e| e.offset).max())
                    .unwrap_or(crate::time::Offset::ZERO);
                for e in &mut offs {
                    e.offset = end;
                }
                flushed.entry(k).or_default().extend(offs);
            }
        }
        // A port that carries notes is tracked from its unit's first block, so
        // an edge added later knows what the port already holds.
        for pu in plan.units.iter().filter(|u| u.shape.event_out > 0) {
            if let Some(u) = self.store[pu.idx.0 as usize].as_mut() {
                let waiting = u.queued.is_some();
                u.keys
                    .size(pu.shape.event_out as usize, pu.shape.note_events, waiting);
            }
        }

        // A FIFO carried across a commit that lowered its bound (a source
        // now declaring a smaller rate, a shorter delay, a larger
        // `MaxBlock`) can hold more than the bound its output slot was
        // priced at. The compiler is pure and cannot know what is queued, so
        // the slot is widened here, by that excess (`EventFifo::excess`),
        // where the state is built — and so is every merge downstream of
        // it, which the compiler priced at the sum of its inputs, and the
        // buffers a sink merges its inputs into (a flush's `merged`, the
        // command overlay, below): the backlog the FIFO holds can still go
        // out in one block, on its frames, and pass everything after it.
        // (One corner stays late, as it was without an excess: a delay
        // retuned below one block makes its whole backlog overdue at offset
        // 0; if that fills the slot, new input falling due in the same
        // block waits for the next.) Here and only here, on a commit that
        // already allocates this whole state; the queue never grows past it
        // (a new non-note-off is refused while it is full), so rendering
        // stays allocation-free. The next rebuild prices it afresh.
        //
        // Per slot, not per value: a slot the colouring pass shares takes
        // the largest excess of anything it holds, which only over-sizes.
        let mut extra = vec![0usize; plan.event_slot_capacity.len()];
        for (f, spec) in event_fb.iter().zip(&plan.event_feedback) {
            if let Some(f) = f {
                let x = &mut extra[spec.slot as usize];
                *x = (*x).max(f.excess());
            }
        }
        // Ops run in topological order, so a merge comes after whatever
        // feeds it.
        for op in &plan.ops {
            match *op {
                Op::EventDelay { delay, dst, .. } => {
                    if let Some(Ring::Event(f)) = &rings[delay as usize] {
                        let x = &mut extra[dst as usize];
                        *x = (*x).max(f.excess());
                    }
                }
                Op::EventMerge { srcs, dst } => {
                    let sum = plan.event_list[srcs.range()]
                        .iter()
                        .map(|&s| extra[s as usize])
                        .sum::<usize>();
                    let x = &mut extra[dst as usize];
                    *x = (*x).max(sum);
                }
                _ => {}
            }
        }
        let extra_total: usize = extra.iter().sum();
        let slot_capacity = |i: usize| plan.event_slot_capacity[i].events(cap) + extra[i];

        // What a crossfade queued behind a running one holds for the unit
        // that waits (`Node::chase`): delivered by `end_fades`, so its room is
        // made here, with the plan, and the audio thread allocates none. What
        // its buffer refused is counted now, and what a successor with no
        // event input cannot take is dropped, counted.
        for (key, refused) in departed.held_refused.drain(..) {
            if let Ok(u) = plan.units.binary_search_by_key(&key, |u| u.key) {
                self.dropped.at_node(u as u32, refused);
            }
        }
        let mut held: BTreeMap<u32, usize> = BTreeMap::new();
        for (u, unit) in plan.units.iter().enumerate() {
            let Some(queued) = self
                .store
                .get_mut(unit.idx.0 as usize)
                .and_then(Option::as_mut)
                .and_then(|s| s.queued.as_mut())
            else {
                continue;
            };
            if queued.chase.is_empty() {
                continue;
            }
            if unit.shape.event_in == 0 {
                let lost = u32::try_from(queued.chase.len()).unwrap_or(u32::MAX);
                self.dropped.at_node(u as u32, lost);
                queued.chase.clear();
            } else {
                held.insert(u as u32, queued.chase.len());
            }
        }
        let held_total: usize = held.values().sum();

        let mut has_inject = vec![false; plan.units.len()];
        let flushed_total: usize = flushed.values().map(Vec::len).sum();
        let widest = (0..plan.event_slot_capacity.len())
            .map(slot_capacity)
            .max()
            .unwrap_or(0);
        let mut inject: Vec<Inject> = Vec::with_capacity(flushed.len() + held.len());
        for ((unit, port), mut events) in flushed {
            has_inject[unit as usize] = true;
            // Several flushes into one sink: one sorted list, ties in
            // flush order (stable).
            events.sort_by_key(|e| e.offset);
            // Room for what `end_fades` may deliver to this input later.
            let room = if port == 0 {
                held.remove(&unit).unwrap_or(0)
            } else {
                0
            };
            events.reserve(room);
            let merged = Vec::with_capacity(events.len() + room + widest);
            inject.push(Inject {
                unit,
                port,
                events,
                merged,
                live: true,
            });
        }
        // The rest of the held: an injection nobody has delivered into yet.
        for (unit, len) in held {
            inject.push(Inject {
                unit,
                port: 0,
                events: Vec::with_capacity(len),
                merged: Vec::with_capacity(len + widest),
                live: false,
            });
        }
        // By unit, then port: the parallel executor's claims take each unit's
        // run of the list (`par.rs`).
        inject.sort_by_key(|i| (i.unit, i.port));

        // The widened slots, all of them: an over-estimate of what they add
        // to the widest node's inputs. The parallel workers' overlays take
        // the same figure.
        let overlay = overlay_capacity(plan, cap, flushed_total + held_total + extra_total);
        let par = par_for(plan, self.pool.as_ref(), self.store.len(), &inject, overlay);
        State {
            arena: Arena::new(plan.audio_slots as usize, max_block),
            flags: {
                let mut v = vec![0u8; plan.audio_slots as usize];
                v[0] = SILENT | CONSTANT;
                v
            },
            events: (0..plan.event_slot_capacity.len())
                .map(|i| Vec::with_capacity(slot_capacity(i)))
                .collect(),
            rings,
            audio_fb,
            event_fb,
            taps,
            event_taps,
            meters,
            lanes,
            inject,
            has_inject,
            has_due: vec![false; plan.units.len()],
            overlay: Vec::with_capacity(overlay),
            par,
        }
    }
}
