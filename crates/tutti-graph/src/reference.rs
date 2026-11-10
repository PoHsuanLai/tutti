//! The reference interpreter: the oracle the plan executor is checked against.
//!
//! A naive interpreter (serial, copy every edge, no colouring) that the
//! optimised executor must match bit for bit on randomly generated
//! topologies. Everything here is written to be *obviously*
//! right rather than fast, and — the point — **independently** of the compiler:
//! it does not call `compile`, read a `Plan`, or share the executor's kernels.
//! It re-derives the evaluation order (by repeated scanning), the latency solve
//! (by memoised recursion), the delays (with `VecDeque`s), the event fan-in
//! (concatenate in source-port order, then a *stable* sort by offset) and
//! feedback (a map of last
//! block's port values) from the spec alone. Where the two agree, it is
//! because two different programs computed the same thing.
//!
//! It never skips a node, never aliases a buffer, and allocates freely. It
//! passes nodes no silence or constant hints, so a node that behaves
//! differently on a hint than without one is caught too.
//!
//! It has no crossfade model: `set_graph` takes a replace as a plain swap, so
//! the held-key behaviour of a queued fade (`held.rs`, `Keys`) is tested on the
//! executors only.
//!
//! # Semantics it pins
//!
//! - **Delay keys are (sink, source)** (see [`DelayKey`]). A PDC delay whose
//!   length changes across a recompile keeps its most recent `min(old, new)`
//!   inputs; a new one starts silent; an audio line whose key disappears is
//!   dropped — a rewired sink never hears its old source's past.
//! - An event delay keeps its pending events with their input times and
//!   reschedules them on a retune, delivering at offset 0 anything already
//!   past due. **When its key disappears its pending events are flushed** to
//!   the sink's next call if the sink survives (with the sink, they go):
//!   each flushed delay keeps its events' spacing, shifted so its earliest
//!   lands at offset 0, and anything that falls past the block is clamped to
//!   its last frame. They sort ahead of that block's own events on ties.
//!   Flushes into one sink queue in this order: undelivered earlier flushes,
//!   event delays in key order, event feedback in key order. A flushed event
//!   can reach a *replaced* unit at that sink — a note-off whose note-on the
//!   old unit saw — which a unit must tolerate.
//! - **A global input is delayed** to its node's arrival like any other
//!   merge-point source.
//! - **A feedback edge delays by exactly the `delay` its edge declares**,
//!   whatever the current block length (see [`FeedbackKey`]): audio keeps the
//!   last `delay` samples of its source port per (port, unit generation,
//!   delay); events keep a FIFO per (sink, source, generation, delay),
//!   flushed like a delay when the key disappears.
//! - Blocks run inside `with_no_denormals`, as the executor's do, so the two
//!   agree about denormals too.
//! - **Crossfades** ([`set_graph_with_fades`](Reference::set_graph_with_fades))
//!   are kept per key, beside the unit map, never in it: the outgoing unit
//!   runs first on a copy of the node's inputs with no events and detached
//!   event writers (every push refused, as the executor's), the incoming
//!   one runs as any node does, and each output sample `i` of the block is
//!   `incoming * g_in + outgoing * g_out` with the gains of frame
//!   `done + i` of the fade while that is inside it. A replace while a fade
//!   runs waits in the entry (the newest wins) and starts, from the running
//!   fade's incoming unit, on the block after that fade ends. A hard edit at
//!   the key, or a re-prepare, drops the entry. Only the gain law
//!   ([`CrossfadeCurve::gains`](crate::CrossfadeCurve::gains)) is shared
//!   with the executor. Like every node here, the outgoing unit is handed
//!   `SilenceMask::NONE` and `ConstantMask::NONE`, where the executor passes
//!   it (and the incoming unit) the real input masks; a unit that renders
//!   differently with a hint than without one diverges, which is the
//!   point.
//!
//! - **Driven params** (see the `param` module docs, `src/param.rs`):
//!   each declared param's state is kept per [`ParamIn`], dropped when its
//!   unit is swapped (a crossfade keeps it), and on a re-prepare. A port
//!   whose event source differs from the one it last ran with starts its
//!   declick and its new source from rest, unless the port has no state yet
//!   (its unit's first block). Ramps and curve segments are evaluated from
//!   their start frame rather than stepped (a segment's shape,
//!   `segment_value`, is shared with the executor). A port's range is the
//!   graph's, unless [`set_param_range`](Reference::set_param_range) moved
//!   it since the last [`set_graph`](Reference::set_graph) —
//!   `Editor::set_param_range`'s contract, taking effect on the next block,
//!   without a new graph.
//!
//! [`FeedbackKey`]: crate::FeedbackKey

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::MAX_NODE_LATENCY;
use crate::NodeKey;
use crate::{Edge, EdgeId, InPort, OutPort, Sink, Source};
use tutti_rt::with_no_denormals;
use tutti_types::{Amplitude, At, Frame, Latency, Samples};

use crate::event::{Event, EventKind, EventWriter, SortedEvents};
use crate::history::TransportHistory;
use crate::io::Io;
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, Node, Prepare, SilenceMask, Status, Transport,
    TransportChanges,
};
use crate::param::{PARAM_DECLICK, ParamIn, ParamInput, ParamRange};
use crate::plan::DelayKey;
use crate::play::PlayClock;
use crate::spec::{EventEdge, EventIn, EventOut, ValidGraph};
use crate::time::Offset;
use tutti_types::PlayReading;
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1, Voice2, decode,
};

/// One param port's state, as the reference keeps it.
#[derive(Default)]
struct RefParam {
    /// The event source it last ran with.
    source: Option<EventOut>,
    modulated: bool,
    last: f32,
    last_base: f32,
    hold: f32,
    /// Frames of the declick left to run.
    fading: usize,
    /// Its event source's ramp.
    ramp: RefRamp,
    /// Frames this port has run, to place its ramps.
    clock: u64,
}

/// The move an event source is on: where it started, where it goes, and
/// how — evaluated from its start frame.
#[derive(Default)]
struct RefRamp {
    from: f32,
    target: f32,
    /// The absolute frame of its event.
    at: u64,
    /// A ramp's length, in frames.
    len: u64,
    /// A ramp's shape.
    shape: tutti_curve::Law,
    /// A curve segment's `(position, length, shape)`; `None`: linear.
    curve: Option<(f32, f32, tutti_curve::Law)>,
    /// Its value on the last frame evaluated.
    value: f32,
    /// Whether the source has sent an event yet: until it has, the port
    /// reads its base.
    started: bool,
}

#[derive(Debug)]
struct RefFifo {
    pending: Vec<(u64, Event)>,
    len: u64,
    clock: u64,
}

/// A naive graph interpreter that the [`Executor`](crate::Executor) is tested
/// against, bit for bit.
///
/// It runs a [`ValidGraph`] directly, written to be obviously right rather
/// than fast and independently of the compiler: it never calls
/// [`compile`](fn@crate::compile), reads a [`Plan`](crate::Plan) or shares the
/// executor's kernels. It re-derives the evaluation order, the latency
/// compensation, the delays, the event fan-in and the feedback from the spec
/// alone, never skips a node, never aliases a buffer and passes nodes no
/// silence or constant hints. It allocates freely, so it is for tests and
/// offline checks, never the audio thread.
///
/// Its public methods mirror the [`Editor`](crate::Editor)/[`Executor`](crate::Executor)
/// pair without the queues: [`set_graph`](Self::set_graph) in place of a
/// commit, [`process`](Self::process) to render a block,
/// [`schedule`](Self::schedule) for timestamped commands, and
/// [`reprepare`](Self::reprepare) for a rate or block-size change.
pub struct Reference {
    prepare: Prepare,
    graph: Option<ValidGraph>,
    units: BTreeMap<NodeKey, (u32, Box<dyn Node>)>,
    /// Crossfades by key: `units` holds the incoming unit.
    fading: BTreeMap<NodeKey, RefFade>,
    arrival: BTreeMap<NodeKey, Latency>,
    delays: BTreeMap<DelayKey, Samples>,
    audio_lines: BTreeMap<DelayKey, VecDeque<f32>>,
    event_lines: BTreeMap<DelayKey, RefFifo>,
    fb_audio: BTreeMap<(OutPort, u32, Samples), VecDeque<f32>>,
    fb_event: BTreeMap<(EventIn, EventOut, u32, Samples), RefFifo>,
    inject: BTreeMap<EventIn, Vec<Event>>,
    /// The keys each tracked event output holds (see `Executor`'s
    /// `held.rs`; written here from `decode`, sharing none of its code).
    held: BTreeMap<EventOut, RefHeld>,
    tracked: BTreeSet<EventOut>,
    /// Scheduled commands not yet due, in scheduling order.
    scheduled: Vec<RefCommand>,
    next_id: u64,
    cancelled: u64,
    /// What continuous playback has crossed — the reference's own record,
    /// sharing no code with the executor's `Playhead`.
    playhead: RefPlayhead,
    /// Between `suspend` and `resume`: the `Prepare` to adopt.
    suspended: Option<Prepare>,
    /// This block's scheduled deliveries, per port, each with its timeline
    /// position (the tie-break after the offset), in scheduling order.
    landing: BTreeMap<EventIn, Vec<(f64, Event)>>,
    late: u64,
    unrouted: u64,
    /// Events a node's writer refused past its declared capacity.
    dropped: u64,
    /// Set by a rate change; the next `set_graph` carries no time-based
    /// state.
    reset_time: bool,
    frame: Frame,
    params: BTreeMap<ParamIn, RefParam>,
    /// Ranges moved by [`set_param_range`](Self::set_param_range) since the
    /// last graph: they override the graph's.
    ranges: BTreeMap<ParamIn, ParamRange>,
    /// The weight each edge gain's lane was given by the last block it ran
    /// (the `w_prev` of doc 013, "Edge gain"), kept while the graph keeps the
    /// edge's entry. The reference's own bookkeeping: none of the executor's.
    lane_prev: BTreeMap<EdgeId, Amplitude>,
    /// Edge gains moved by [`set_edge_gain`](Self::set_edge_gain) since the
    /// last graph: they override the graph's.
    gain_moves: BTreeMap<EdgeId, Amplitude>,
    /// The param-port state of each driven edge gain's lane, as
    /// `Reference::params` keeps a node's: one rule (`ref_port`).
    gain_ports: BTreeMap<EdgeId, RefParam>,
    /// Edge gain ranges moved by [`set_edge_gain_range`](Self::set_edge_gain_range)
    /// since the last graph.
    gain_ranges: BTreeMap<EdgeId, ParamRange>,
    /// The transport's jumps, kept as the executor keeps them: the same type.
    /// (Its answers are checked against the blocks themselves in
    /// `history.rs`'s tests, not against this.)
    history: TransportHistory,
    /// The play clock at the next block's first frame (before a start
    /// there), and whether the last block ended playing: written from the
    /// rules in `src/play.rs`'s module docs, not through its `PlayState`.
    play: PlayReading,
    play_playing: bool,
}

impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reference")
            .field("prepare", &self.prepare)
            .field("graph", &self.graph)
            .field("arrival", &self.arrival)
            .field("delays", &self.delays)
            .field("audio_lines", &self.audio_lines.len())
            .field("fb_audio", &self.fb_audio.len())
            .field("fb_event", &self.fb_event.len())
            .field("inject", &self.inject.len())
            .field("next_id", &self.next_id)
            .field("cancelled", &self.cancelled)
            .field("suspended", &self.suspended)
            .field("landing", &self.landing.len())
            .field("late", &self.late)
            .field("unrouted", &self.unrouted)
            .field("dropped", &self.dropped)
            .field("reset_time", &self.reset_time)
            .field("frame", &self.frame)
            .field("ranges", &self.ranges)
            .field("lane_prev", &self.lane_prev)
            .field("play", &self.play)
            .field("play_playing", &self.play_playing)
            .finish_non_exhaustive()
    }
}

impl Reference {
    /// This block's play clock, and the reading the next block starts from:
    /// a start at the first frame restarts it in a fresh epoch, each start
    /// inside the block takes one reserved epoch from that frame on, and it
    /// otherwise counts one per frame.
    fn play_clock(
        &mut self,
        transport: &Transport,
        changes: &TransportChanges,
        frames: usize,
    ) -> PlayClock {
        if transport.playing && !self.play_playing {
            self.play = PlayReading {
                epoch: crate::play::fresh_epochs(1),
                play: Frame::ZERO,
            };
        }
        let mut playing = transport.playing;
        let mut starts = Vec::new();
        for c in changes.as_slice() {
            if !playing && c.to.playing {
                starts.push(c.at.index());
            }
            playing = c.to.playing;
        }
        let clock = PlayClock {
            start: self.play,
            next_epoch: if starts.is_empty() {
                0
            } else {
                crate::play::fresh_epochs(starts.len() as u32)
            },
        };
        self.play = match starts.last() {
            Some(&at) => PlayReading {
                epoch: (clock.next_epoch + starts.len() as u32 - 1) % PlayReading::EPOCHS,
                play: Frame((frames - at) as u64),
            },
            None => PlayReading {
                epoch: self.play.epoch,
                play: self.play.play + Samples(frames),
            },
        };
        self.play_playing = playing;
        clock
    }

    /// An interpreter with no graph.
    #[must_use]
    pub fn new(prepare: Prepare) -> Self {
        Self {
            prepare,
            graph: None,
            units: BTreeMap::new(),
            fading: BTreeMap::new(),
            arrival: BTreeMap::new(),
            delays: BTreeMap::new(),
            audio_lines: BTreeMap::new(),
            event_lines: BTreeMap::new(),
            fb_audio: BTreeMap::new(),
            fb_event: BTreeMap::new(),
            inject: BTreeMap::new(),
            held: BTreeMap::new(),
            tracked: BTreeSet::new(),
            scheduled: Vec::new(),
            next_id: 0,
            cancelled: 0,
            params: BTreeMap::new(),
            ranges: BTreeMap::new(),
            lane_prev: BTreeMap::new(),
            gain_moves: BTreeMap::new(),
            gain_ports: BTreeMap::new(),
            gain_ranges: BTreeMap::new(),
            playhead: RefPlayhead::default(),
            suspended: None,
            landing: BTreeMap::new(),
            late: 0,
            unrouted: 0,
            dropped: 0,
            reset_time: false,
            history: TransportHistory::new(),
            frame: Frame::ZERO,
            play: PlayReading {
                epoch: crate::play::fresh_epochs(1),
                play: Frame::ZERO,
            },
            play_playing: false,
        }
    }

    /// Delivers `kind` into `to` at `at` — the same contract as
    /// `Editor::schedule`, without the queue: a time already past lands at
    /// offset 0 of the next block and is counted late; one whose port is gone
    /// when it falls due is counted unrouted.
    ///
    /// Returns the command's id, for [`cancel`](Self::cancel).
    pub fn schedule(&mut self, at: At, to: EventIn, kind: EventKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let pos = match at {
            At::Frame(f) => f.get() as f64,
            _ => 0.0,
        };
        self.scheduled.push(RefCommand {
            id,
            at,
            to,
            kind,
            pos,
        });
        id
    }

    /// Takes back command `id` if it has not landed.
    pub fn cancel(&mut self, id: u64) {
        let before = self.scheduled.len();
        self.scheduled.retain(|c| c.id != id);
        self.cancelled += (before - self.scheduled.len()) as u64;
    }

    /// Takes back every command that has not landed.
    pub fn cancel_all(&mut self) {
        self.cancelled += self.scheduled.len() as u64;
        self.scheduled.clear();
    }

    /// Commands cancelled before they landed.
    pub fn cancelled_command_count(&self) -> u64 {
        self.cancelled
    }

    /// The first half of [`reprepare`](Self::reprepare), as the executor
    /// sees it when the editor's first commit lands: from here the
    /// interpreter renders silence and its clock keeps counting, and on a
    /// rate change the clock and every pending `At::Frame` are rescaled to
    /// the same wall-clock time at the new rate (nearest frame). Units are
    /// not touched until [`resume`](Self::resume).
    pub fn suspend(&mut self, prepare: Prepare) {
        let (old, new) = (
            self.prepare.sample_rate().get(),
            prepare.sample_rate().get(),
        );
        if old != new {
            self.history.clear();
            let ratio = new / old;
            self.frame = Frame((self.frame.get() as f64 * ratio).round() as u64);
            self.play.play = Frame((self.play.play.get() as f64 * ratio).round() as u64);
            for c in &mut self.scheduled {
                if let At::Frame(_) = c.at {
                    // Rounded for landing; the unrounded position keeps the
                    // order of two frames that round to one.
                    c.pos *= ratio;
                    c.at = At::Frame(Frame(c.pos.round() as u64));
                }
            }
        }
        // Every crossfade is cut: the newest unit at its key stays.
        for (key, f) in std::mem::take(&mut self.fading) {
            if let (Some((unit, _)), Some(slot)) = (f.next, self.units.get_mut(&key)) {
                slot.1 = unit;
            }
        }
        self.suspended = Some(prepare);
    }

    /// The second half: adopt the `Prepare`, re-prepare every unit, and
    /// re-derive the delays (see [`reprepare`](Self::reprepare) for the
    /// state rule).
    ///
    /// # Panics
    ///
    /// If not suspended.
    pub fn resume(&mut self) {
        let prepare = self.suspended.take().expect("resume after suspend");
        if prepare.sample_rate() != self.prepare.sample_rate() {
            self.reset_time = true;
        }
        self.prepare = prepare;
        for (_, unit) in self.units.values_mut() {
            unit.prepare(&prepare);
        }
        // Every unit comes back from a re-prepare as a fresh insert: its
        // params start over.
        self.params.clear();
        if let Some(graph) = self.graph.clone() {
            for (&at, e) in flat_edges(graph.topology()) {
                if let crate::Edge::Feedback(f) = e {
                    assert!(
                        f.delay >= prepare.max_block().samples(),
                        "feedback into {at:?} is shorter than the new maximum block"
                    );
                }
            }
            self.set_graph(&graph, BTreeMap::new());
        }
    }

    /// Switch to `prepare`, as `Editor::reprepare` does, in one step: every
    /// unit is re-prepared, the delays are re-derived from the new shapes,
    /// and — the rule written out a second time, independently — a
    /// sample-rate change starts every audio delay and audio feedback line
    /// silent and flushes every event delay and event feedback FIFO to its
    /// sink, while a `MaxBlock`-only change keeps them all (retuned by key,
    /// like any recompile).
    ///
    /// # Panics
    ///
    /// If the graph has a feedback edge shorter than the new `MaxBlock` —
    /// `Editor::reprepare` refuses that before it starts, and the reference
    /// is only ever driven alongside it.
    ///
    /// Equivalent to [`suspend`](Self::suspend) then [`resume`](Self::resume)
    /// with no block between.
    pub fn reprepare(&mut self, prepare: Prepare) {
        self.suspend(prepare);
        self.resume();
    }

    /// Scheduled commands that landed late (see `Executor::late_command_count`).
    pub fn late_command_count(&self) -> u64 {
        self.late
    }

    /// Scheduled commands with nowhere to land.
    pub fn unrouted_command_count(&self) -> u64 {
        self.unrouted
    }

    /// Events refused by a node's writer past the node's declared
    /// [`Shape::event_capacity`](crate::Shape::event_capacity) — the only
    /// place the reference refuses one: its merges, delays and feedback
    /// FIFOs are unbounded. An undeclared port is unbounded here too (the
    /// executor's default is its own configuration, not the node's).
    pub fn dropped_event_count(&self) -> u64 {
        self.dropped
    }

    /// The PDC delay the interpreter derived for `key`, or zero — so a test
    /// can compare its latency solve against the compiler's.
    pub fn delay(&self, key: DelayKey) -> Samples {
        self.delays.get(&key).copied().unwrap_or_default()
    }

    /// Moves param port `at`'s range, from the next block on, without a new
    /// graph: `Editor::set_param_range`'s contract. The next
    /// [`set_graph`](Self::set_graph) replaces it with the graph's (the
    /// editor's spec records the move, so the two agree).
    pub fn set_param_range(&mut self, at: ParamIn, range: ParamRange) {
        self.ranges.insert(at, range);
    }

    /// Moves edge `id`'s weight, from the next block on, without a new graph:
    /// `Editor::set_edge_gain`'s contract. The next
    /// [`set_graph`](Self::set_graph) replaces it with the graph's (the
    /// editor's spec records the move, so the two agree).
    pub fn set_edge_gain(&mut self, id: EdgeId, base: Amplitude) {
        self.gain_moves.insert(id, base);
    }

    /// Moves edge `id`'s range, from the next block on, without a new graph:
    /// `Editor::set_edge_gain_range`'s contract.
    pub fn set_edge_gain_range(&mut self, id: EdgeId, range: ParamRange) {
        self.gain_ranges.insert(id, range);
    }

    /// Switch to `graph`. `fresh` supplies a unit for every node that is new or
    /// whose generation changed; units whose key and generation survive keep
    /// their state.
    ///
    /// # Panics
    ///
    /// If `fresh` lacks a needed unit.
    pub fn set_graph(&mut self, graph: &ValidGraph, fresh: BTreeMap<NodeKey, Box<dyn Node>>) {
        self.set_graph_with_fades(graph, fresh, &BTreeMap::new());
    }

    /// As [`set_graph`](Self::set_graph), with the regenerated keys in
    /// `fades` crossfading from the unit they had rather than swapping it —
    /// `Editor::replace`'s contract, derived here on its own (see the module
    /// docs). A fade naming a key that is not regenerated, or has no unit
    /// here to fade from, is ignored; a zero-length one is a swap.
    ///
    /// # Panics
    ///
    /// If a fade's two units differ in shape beyond their tails — the editor
    /// refuses those, and the reference is only driven alongside it.
    pub fn set_graph_with_fades(
        &mut self,
        graph: &ValidGraph,
        mut fresh: BTreeMap<NodeKey, Box<dyn Node>>,
        fades: &BTreeMap<NodeKey, crate::Fade>,
    ) {
        let t = graph.topology();
        // Keys whose unit is not the one running now: removed, or replaced.
        let gone: BTreeSet<NodeKey> = self
            .units
            .iter()
            .filter(|(k, (g, _))| !t.nodes.contains_key(k) || *g != graph.generation(**k))
            .map(|(&k, _)| k)
            .collect();
        let replaced: BTreeSet<NodeKey> = gone
            .iter()
            .copied()
            .filter(|k| t.nodes.contains_key(k))
            .collect();
        let mut departed: BTreeMap<EventOut, RefHeld> = BTreeMap::new();
        for from in self.held.keys().copied().collect::<Vec<_>>() {
            if gone.contains(&from.node) {
                departed.insert(from, self.held.remove(&from).expect("listed"));
            }
        }
        self.tracked.retain(|f| !gone.contains(&f.node));
        // A node whose declared latency changed without a new generation
        // (`Editor::set_latency`) has its crossfade cut: both units of a
        // fade must share the latency the graph compensates. Derived here
        // from the two values, not told.
        if let Some(old) = self.graph.as_ref() {
            let moved: Vec<NodeKey> = self
                .fading
                .keys()
                .copied()
                .filter(|k| {
                    let (was, now) = (old.topology().nodes.get(k), t.nodes.get(k));
                    matches!((was, now), (Some(a), Some(b)) if a.latency != b.latency)
                        && old.generation(*k) == graph.generation(*k)
                })
                .collect();
            for key in moved {
                let f = self.fading.remove(&key).expect("listed");
                if let (Some((unit, _)), Some(slot)) = (f.next, self.units.get_mut(&key)) {
                    slot.1 = unit;
                }
            }
        }
        for (&key, fade) in fades {
            let generation = graph.generation(key);
            let changed = self.units.get(&key).is_some_and(|(g, _)| *g != generation);
            if fade.duration.is_zero() || !t.nodes.contains_key(&key) || !changed {
                continue;
            }
            let mut incoming = fresh
                .remove(&key)
                .unwrap_or_else(|| panic!("no unit for node {}", key.0));
            incoming.prepare(&self.prepare);
            let slot = self.units.get_mut(&key).expect("checked");
            let (was, now) = (slot.1.shape(), incoming.shape());
            assert!(
                was.audio_in == now.audio_in
                    && was.audio_out == now.audio_out
                    && was.event_in == now.event_in
                    && was.event_out == now.event_out
                    && was.latency == now.latency
                    && was.in_place == now.in_place
                    && was.event_resolution == now.event_resolution
                    && was.params == now.params,
                "node {} crossfades between shapes",
                key.0
            );
            slot.0 = generation;
            match self.fading.get_mut(&key) {
                // Behind the running fade; a unit waiting there never ran.
                Some(running) => running.next = Some((incoming, *fade)),
                None => {
                    let old = std::mem::replace(&mut slot.1, incoming);
                    self.fading.insert(
                        key,
                        RefFade {
                            old,
                            done: 0,
                            len: fade.duration.get(),
                            curve: fade.curve,
                            next: None,
                        },
                    );
                }
            }
        }
        self.units.retain(|k, (generation, _)| {
            t.nodes.contains_key(k) && *generation == graph.generation(*k)
        });
        // A key swapped or removed takes its crossfade with it.
        let units = &self.units;
        self.fading.retain(|k, _| units.contains_key(k));
        for &key in t.nodes.keys() {
            if !self.units.contains_key(&key) {
                let mut unit = fresh
                    .remove(&key)
                    .unwrap_or_else(|| panic!("no unit for node {}", key.0));
                unit.prepare(&self.prepare);
                self.units.insert(key, (graph.generation(key), unit));
                // A new unit, or a swapped one: its params start over. (A
                // crossfade keeps the key's, as the unit slot stays.)
                self.params.retain(|at, _| at.node != key);
            }
        }
        let units = &self.units;
        self.params.retain(|at, _| units.contains_key(&at.node));

        // Latency, by memoised recursion over direct predecessors. Read
        // from the spec, not the unit: `Editor::set_latency` changes a
        // node's figure without touching the unit, whose own `shape()` may
        // lag. The compiler reads the spec
        // too, and checks it against the shapes it was handed.
        let lat =
            |k: NodeKey| Latency::new(t.nodes[&k].latency).min(Latency::new(MAX_NODE_LATENCY));
        let mut arrival: BTreeMap<NodeKey, Latency> = BTreeMap::new();
        fn arrive(
            k: NodeKey,
            g: &ValidGraph,
            lat: &dyn Fn(NodeKey) -> Latency,
            memo: &mut BTreeMap<NodeKey, Latency>,
        ) -> Latency {
            if let Some(&a) = memo.get(&k) {
                return a;
            }
            let mut best = Latency::ZERO;
            for (at, e) in flat_edges(g.topology()) {
                if at.node == k
                    && let Edge::Direct(Source::Node(p)) = e
                {
                    best = best.max(arrive(p.node, g, lat, memo) + lat(p.node));
                }
            }
            for (at, sources) in g.events() {
                if at.node == k {
                    for e in sources {
                        if let EventEdge::Direct(from) = e {
                            best = best.max(arrive(from.node, g, lat, memo) + lat(from.node));
                        }
                    }
                }
            }
            for (at, m) in g.params() {
                if at.node == k
                    && let Some(from) = m.source
                {
                    let n = from.node;
                    best = best.max(arrive(n, g, lat, memo) + lat(n));
                }
            }
            for (id, gain) in g.gains() {
                if let (Sink::Port(at), Some(from)) = (id.sink, gain.source)
                    && at.node == k
                {
                    best = best.max(arrive(from.node, g, lat, memo) + lat(from.node));
                }
            }
            memo.insert(k, best);
            best
        }
        for &k in t.nodes.keys() {
            arrive(k, graph, &lat, &mut arrival);
        }
        let departure = |k: NodeKey| arrival[&k] + lat(k);

        let mut delays: BTreeMap<DelayKey, Samples> = BTreeMap::new();
        for (&at, e) in flat_edges(t) {
            let dep = match *e {
                Edge::Direct(Source::Node(p)) => departure(p.node).samples(),
                Edge::Direct(Source::Global(_)) => Samples::ZERO,
                _ => continue,
            };
            let Edge::Direct(from) = *e else { continue };
            // Written as a subtraction on the raw counts, not with
            // `Latency::gap_to` the compiler uses: two spellings of one
            // rule, so a slip in either shows up as a divergence.
            let d = arrival[&at.node]
                .samples()
                .checked_sub(dep)
                .unwrap_or_default();
            if !d.is_zero() {
                delays.insert(DelayKey::Audio { at, from }, d);
            }
        }
        for (&at, sources) in graph.events() {
            for e in sources {
                if let EventEdge::Direct(from) = *e {
                    let d = arrival[&at.node]
                        .samples()
                        .checked_sub(departure(from.node).samples())
                        .unwrap_or_default();
                    if !d.is_zero() {
                        delays.insert(DelayKey::Event { at, from }, d);
                    }
                }
            }
        }
        for (&at, m) in graph.params() {
            if let Some(from) = m.source {
                let d = arrival[&at.node]
                    .samples()
                    .checked_sub(departure(from.node).samples())
                    .unwrap_or_default();
                if !d.is_zero() {
                    delays.insert(DelayKey::ParamEvent { at, from }, d);
                }
            }
        }
        // The output time is the latest departure over every source of every
        // channel; each source of a summed channel is delayed to it on its own.
        let source_departure = |s: &Source| match *s {
            Source::Node(p) => departure(p.node).samples(),
            Source::Global(_) | Source::Zero => Samples::ZERO,
        };
        let total = t
            .outputs
            .iter()
            .flatten()
            .map(source_departure)
            // A lane driving an output edge's gain counts toward the output
            // time too.
            .chain(
                graph
                    .gains()
                    .iter()
                    .filter_map(|(id, g)| match (id.sink, g.source) {
                        (Sink::Output(_), Some(from)) => Some(departure(from.node).samples()),
                        _ => None,
                    }),
            )
            .max()
            .unwrap_or_default();
        for (&id, gain) in graph.gains() {
            let Some(from) = gain.source else { continue };
            let to = match id.sink {
                Sink::Port(at) => arrival[&at.node].samples(),
                Sink::Output(_) => total,
            };
            let d = to
                .checked_sub(departure(from.node).samples())
                .unwrap_or_default();
            if !d.is_zero() {
                delays.insert(DelayKey::EdgeGain { id, from }, d);
            }
        }
        for (ch, sources) in t.outputs.iter().enumerate() {
            for s in sources {
                let d = total.checked_sub(source_departure(s)).unwrap_or_default();
                if !d.is_zero() && *s != Source::Zero {
                    delays.insert(
                        DelayKey::Output {
                            channel: ch as u16,
                            from: *s,
                        },
                        d,
                    );
                }
            }
        }

        // Flush first, while the old state is intact. A sink survives when
        // its node is still here with that event input.
        let survives = |at: &EventIn| {
            self.units
                .get(&at.node)
                .is_some_and(|(_, u)| at.port < u.shape().event_in)
        };
        let fb_event_keys: BTreeSet<(EventIn, EventOut, u32, Samples)> = graph
            .events()
            .iter()
            .flat_map(|(&at, v)| {
                v.iter().filter_map(move |e| match *e {
                    EventEdge::Feedback { from, delay } => {
                        Some((at, from, graph.generation(from.node), delay))
                    }
                    EventEdge::Direct(_) => None,
                })
            })
            .collect();
        let mut inject = std::mem::take(&mut self.inject);
        inject.retain(|at, _| survives(at));
        let mut flush = |at: EventIn, pending: Vec<Event>| {
            if survives(&at) {
                // Kept in spacing order (stable: flush order on ties) before
                // any clamping, which would erase it.
                let list = inject.entry(at).or_default();
                list.extend(pending);
                list.sort_by_key(|e| e.offset);
            }
        };
        // After a rate change nothing time-based survives, so every event
        // line and event feedback FIFO is flushed as if it had vanished.
        let reset = std::mem::take(&mut self.reset_time);
        for (k, f) in &self.event_lines {
            if let DelayKey::Event { at, .. } = *k
                && (reset || !delays.contains_key(k))
            {
                flush(at, relative(f));
            }
        }
        for (k, f) in &self.fb_event {
            if reset || !fb_event_keys.contains(k) {
                flush(k.0, relative(f));
            }
        }
        // Keys held on an edge that ended go to the sink that lost it, after
        // everything flushed to it.
        let old_edges = self.graph.as_ref().map(edges_of).unwrap_or_default();
        let new_edges = edges_of(graph);
        for &(from, at) in &old_edges {
            let in_new = new_edges.contains(&(from, at));
            if (in_new && !gone.contains(&from.node))
                || replaced.contains(&at.node)
                || !survives(&at)
            {
                continue;
            }
            let held = if gone.contains(&from.node) {
                departed.get(&from)
            } else {
                self.held.get(&from)
            };
            let Some(h) = held else { continue };
            // A replaced source whose edge and delay go on: the close
            // travels the delay, behind what is in flight.
            let key = DelayKey::Event { at, from };
            if in_new
                && delays.contains_key(&key)
                && let Some(f) = self.event_lines.get_mut(&key)
            {
                let at_now = f.clock;
                let offs = h.closing(Offset::ZERO);
                f.pending.extend(offs.into_iter().map(|e| (at_now, e)));
                continue;
            }
            let list = inject.entry(at).or_default();
            let end = list.iter().map(|e| e.offset).max().unwrap_or(Offset::ZERO);
            list.extend(h.closing(end));
            list.sort_by_key(|e| e.offset);
        }
        for (&node, (_, unit)) in &self.units {
            let shape = unit.shape();
            for port in (0..shape.event_out).filter(|_| shape.note_events) {
                self.tracked.insert(EventOut { node, port });
            }
        }
        self.inject = inject;
        if reset {
            self.audio_lines.clear();
            self.event_lines.clear();
            self.fb_audio.clear();
            self.fb_event.clear();
        }

        // Delay state: keep by key (retuned), drop the rest, start new ones
        // silent.
        self.audio_lines.retain(|k, _| delays.contains_key(k));
        self.event_lines.retain(|k, _| delays.contains_key(k));
        for (&key, &d) in &delays {
            if matches!(
                key,
                DelayKey::Event { .. } | DelayKey::ParamEvent { .. } | DelayKey::EdgeGain { .. }
            ) {
                let f = self.event_lines.entry(key).or_insert(RefFifo {
                    pending: Vec::new(),
                    len: 0,
                    clock: 0,
                });
                f.len = d.get() as u64;
            } else {
                let line = self
                    .audio_lines
                    .entry(key)
                    .or_insert_with(|| VecDeque::from(vec![0.0; d.get()]));
                while line.len() > d.get() {
                    line.pop_front();
                }
                while line.len() < d.get() {
                    line.push_front(0.0);
                }
            }
        }

        let audio_fb: BTreeSet<(OutPort, u32, Samples)> = t
            .edges
            .values()
            .flatten()
            .filter_map(|e| match e {
                Edge::Feedback(f) => Some((f.from, graph.generation(f.from.node), f.delay)),
                _ => None,
            })
            .collect();
        self.fb_audio.retain(|k, _| audio_fb.contains(k));
        for k in audio_fb {
            self.fb_audio
                .entry(k)
                .or_insert_with(|| VecDeque::from(vec![0.0; k.2.get()]));
        }
        self.fb_event.retain(|k, _| fb_event_keys.contains(k));
        for k in fb_event_keys {
            self.fb_event.entry(k).or_insert(RefFifo {
                pending: Vec::new(),
                len: k.3.get() as u64,
                clock: 0,
            });
        }

        self.arrival = arrival;
        self.delays = delays;
        self.graph = Some(graph.clone());
        self.ranges.clear();
        // A lane lives as long as its entry: a commit that keeps the edge's
        // entry keeps its weight, one that drops it forgets it.
        self.lane_prev
            .retain(|id, _| graph.gains().contains_key(id));
        self.gain_moves.clear();
        self.gain_ranges.clear();
        self.gain_ports
            .retain(|id, _| graph.gains().contains_key(id));
    }

    /// Renders one block. Same contract as
    /// [`Executor::process`](crate::Executor::process).
    pub fn process(
        &mut self,
        frames: usize,
        transport: &Transport,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        self.process_with_changes(frames, transport, &TransportChanges::NONE, inputs, outputs);
    }

    /// Renders one block with the transport changing inside it. Same contract
    /// as [`Executor::process_with_changes`](crate::Executor::process_with_changes).
    ///
    /// # Panics
    ///
    /// If the graph has a cycle.
    pub fn process_with_changes(
        &mut self,
        frames: usize,
        transport: &Transport,
        changes: &TransportChanges,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        // The block's transport pieces, cut here by hand rather than through
        // `Env::segments`, so the executor's cut has an independent check.
        let mut pieces: Vec<(usize, usize, Transport)> = Vec::new();
        let mut cut = 0;
        let mut t = *transport;
        for c in changes.as_slice() {
            let at = c.at.index().min(frames);
            if at > cut {
                pieces.push((cut, at - cut, t));
                cut = at;
            }
            t = c.to;
        }
        if frames > cut {
            pieces.push((cut, frames - cut, t));
        }
        if let Some(next) = self.suspended {
            // Suspended: silence, the clock counts, the transport moves.
            for o in outputs.iter_mut() {
                o[..frames].fill(0.0);
            }
            for &(_, len, t) in &pieces {
                self.playhead.observe(&t, next.sample_rate().get(), len);
            }
            let play = self.play_clock(transport, changes, frames);
            self.history.observe(&Env {
                frame: self.frame,
                sample_rate: next.sample_rate(),
                block_len: Samples(frames),
                transport: *transport,
                changes: *changes,
                play,
            });
            self.frame += Samples(frames);
            return;
        }
        let play = self.play_clock(transport, changes, frames);
        let env = Env {
            frame: self.frame,
            sample_rate: self.prepare.sample_rate(),
            block_len: Samples(frames),
            transport: *transport,
            changes: *changes,
            play,
        };
        self.history.observe(&env);
        let Some(graph) = self.graph.clone() else {
            for o in outputs.iter_mut() {
                o[..frames].fill(0.0);
            }
            self.frame += Samples(frames);
            return;
        };
        let t = graph.topology();
        // Each weighted lane's base for this block, as it stands at its start
        // (a lane's weights are made where its addend is, `lane_weight`).
        let targets: BTreeMap<EdgeId, Amplitude> = graph
            .gains()
            .iter()
            .map(|(&id, gain)| (id, self.gain_moves.get(&id).copied().unwrap_or(gain.base)))
            .collect();
        with_no_denormals(|| {
            // Scheduled commands: which land this block, and where. Kept in
            // scheduling order per port; the gather below appends them after the
            // port's own events.
            self.landing.clear();
            for &(_, len, t) in &pieces {
                self.playhead
                    .observe(&t, self.prepare.sample_rate().get(), len);
            }
            let mut waiting = Vec::new();
            for mut cmd in std::mem::take(&mut self.scheduled) {
                // PDC: timeline frame F reaches this sink at F + its arrival.
                let arrival = self.arrival.get(&cmd.to.node).copied().unwrap_or_default();
                let offset = match self.land(&mut cmd, arrival, &pieces, frames) {
                    None => {
                        waiting.push(cmd);
                        continue;
                    }
                    Some(Some(o)) => o,
                    Some(None) => {
                        self.late += 1;
                        Offset::ZERO
                    }
                };
                let (to, kind, pos) = (cmd.to, cmd.kind, cmd.pos);
                let has_port = self
                    .units
                    .get(&to.node)
                    .is_some_and(|(_, u)| to.port < u.shape().event_in);
                if has_port {
                    self.landing
                        .entry(to)
                        .or_default()
                        .push((pos, Event { offset, kind }));
                } else {
                    self.unrouted += 1;
                }
            }
            self.scheduled = waiting;

            let mut audio: BTreeMap<OutPort, Vec<f32>> = BTreeMap::new();
            let mut events: BTreeMap<EventOut, Vec<Event>> = BTreeMap::new();
            let mut done: BTreeSet<NodeKey> = BTreeSet::new();

            let direct_preds = |k: NodeKey| -> Vec<NodeKey> {
                let mut v: Vec<NodeKey> = t
                    .edges
                    .iter()
                    .filter(|(at, _)| at.node == k)
                    .flat_map(|(_, es)| es)
                    .filter_map(|e| match e {
                        Edge::Direct(Source::Node(p)) => Some(p.node),
                        _ => None,
                    })
                    .collect();
                for (at, sources) in graph.events() {
                    if at.node == k {
                        v.extend(sources.iter().filter_map(|e| match e {
                            EventEdge::Direct(f) => Some(f.node),
                            EventEdge::Feedback { .. } => None,
                        }));
                    }
                }
                for (at, m) in graph.params() {
                    if at.node == k {
                        v.extend(m.source.map(|s| s.node));
                    }
                }
                for (id, gain) in graph.gains() {
                    if let (Sink::Port(at), Some(from)) = (id.sink, gain.source)
                        && at.node == k
                    {
                        v.push(from.node);
                    }
                }
                v
            };

            while done.len() < t.nodes.len() {
                let before = done.len();
                for &key in t.nodes.keys() {
                    if done.contains(&key) || !direct_preds(key).iter().all(|p| done.contains(p)) {
                        continue;
                    }
                    self.run_node(
                        key,
                        &graph,
                        frames,
                        &env,
                        inputs,
                        &mut audio,
                        &mut events,
                        &targets,
                    );
                    done.insert(key);
                }
                assert!(
                    done.len() > before,
                    "reference interpreter: graph has a cycle"
                );
            }

            // Crossfades that reached their end this block: the outgoing unit
            // goes, and one waiting starts — from the next block.
            let ended: Vec<NodeKey> = self
                .fading
                .iter()
                .filter(|(_, f)| f.done >= f.len)
                .map(|(&k, _)| k)
                .collect();
            for key in ended {
                let f = self.fading.remove(&key).expect("listed");
                if let Some((unit, fade)) = f.next {
                    let slot = self.units.get_mut(&key).expect("a fading node has a unit");
                    let old = std::mem::replace(&mut slot.1, unit);
                    self.fading.insert(
                        key,
                        RefFade {
                            old,
                            done: 0,
                            len: fade.duration.get(),
                            curve: fade.curve,
                            next: None,
                        },
                    );
                }
            }

            for (ch, sources) in t.outputs.iter().enumerate() {
                let mut addends: Vec<RefAddend> = Vec::new();
                for source in sorted(sources) {
                    let mut buf = match source {
                        Source::Node(p) => audio[&p].clone(),
                        Source::Global(g) => inputs[g as usize][..frames].to_vec(),
                        Source::Zero => vec![0.0; frames],
                    };
                    let key = DelayKey::Output {
                        channel: ch as u16,
                        from: source,
                    };
                    if let Some(line) = self.audio_lines.get_mut(&key) {
                        delay_line(line, &mut buf);
                    }
                    let id = EdgeId {
                        sink: Sink::Output(ch as u16),
                        from: Edge::Direct(source),
                    };
                    let weight = self.lane_weight(&graph, id, &targets, frames, &events);
                    addends.push(RefAddend { buf, weight });
                }
                outputs[ch][..frames].copy_from_slice(&sum_addends(addends, frames));
            }

            // Feed the feedback delays with this block, after every read of them.
            for ((from, _, _), line) in self.fb_audio.iter_mut() {
                for &x in &audio[from] {
                    line.push_back(x);
                    line.pop_front();
                }
            }
            for ((_, from, _, _), f) in self.fb_event.iter_mut() {
                let start = f.clock;
                f.pending.extend(
                    events[from]
                        .iter()
                        .map(|e| (start + u64::from(e.offset.get()), *e)),
                );
                f.clock += frames as u64;
            }
            self.frame += Samples(frames);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn run_node(
        &mut self,
        key: NodeKey,
        graph: &ValidGraph,
        frames: usize,
        env: &Env,
        inputs: &[&[f32]],
        audio: &mut BTreeMap<OutPort, Vec<f32>>,
        events: &mut BTreeMap<EventOut, Vec<Event>>,
        targets: &BTreeMap<EdgeId, Amplitude>,
    ) {
        let t = graph.topology();
        let shape = self.units[&key].1.shape();

        let mut ins: Vec<Vec<f32>> = Vec::new();
        for port in 0..shape.audio_in.count() {
            let at = InPort { node: key, port };
            // The addends in source order, each through its own PDC delay.
            let mut addends: Vec<RefAddend> = Vec::new();
            for edge in sorted(t.edges.get(&at).map_or(&[][..], Vec::as_slice)) {
                let mut buf = match edge {
                    Edge::Direct(Source::Zero) => vec![0.0; frames],
                    Edge::Direct(Source::Global(g)) => inputs[g as usize][..frames].to_vec(),
                    Edge::Direct(Source::Node(p)) => audio[&p].clone(),
                    Edge::Feedback(f) => {
                        let line =
                            &self.fb_audio[&(f.from, graph.generation(f.from.node), f.delay)];
                        line.iter().take(frames).copied().collect()
                    }
                };
                if let Edge::Direct(from) = edge
                    && let Some(line) = self.audio_lines.get_mut(&DelayKey::Audio { at, from })
                {
                    delay_line(line, &mut buf);
                }
                let id = EdgeId {
                    sink: Sink::Port(at),
                    from: edge,
                };
                let weight = self.lane_weight(graph, id, targets, frames, events);
                addends.push(RefAddend { buf, weight });
            }
            ins.push(sum_addends(addends, frames));
        }

        let mut ev_ins: Vec<Vec<Event>> = Vec::new();
        for port in 0..shape.event_in {
            let at = EventIn { node: key, port };
            // Flushed events first: they are older than anything this block.
            let mut all: Vec<Event> = self.inject.remove(&at).unwrap_or_default();
            for e in &mut all {
                e.offset = e.offset.clamp_to(frames);
            }
            // Source order is `(NodeKey, port)` of the source, whatever order
            // the spec lists them in: re-keyed by source here, where the
            // compiler sorts its list.
            let by_source: BTreeMap<EventOut, EventEdge> = graph
                .events()
                .get(&at)
                .map(|v| v.iter().map(|&e| (e.from(), e)).collect())
                .unwrap_or_default();
            for e in by_source.values() {
                match *e {
                    EventEdge::Direct(from) => {
                        let src = events[&from].clone();
                        match self.event_lines.get_mut(&DelayKey::Event { at, from }) {
                            Some(f) => all.extend(fifo_run(f, &src, frames)),
                            None => all.extend(src),
                        }
                    }
                    EventEdge::Feedback { from, delay } => {
                        let f = self
                            .fb_event
                            .get_mut(&(at, from, graph.generation(from.node), delay))
                            .expect("built by set_graph");
                        all.extend(fifo_due(f, frames));
                    }
                }
            }
            // Stable: equal offsets keep source order.
            // Scheduled commands: one more source, after the edges.
            // Ties at one offset: by timeline position, then scheduling
            // order (the sort is stable).
            let mut landed = self.landing.remove(&at).unwrap_or_default();
            landed.sort_by(|a, b| a.1.offset.cmp(&b.1.offset).then(a.0.total_cmp(&b.0)));
            all.extend(landed.into_iter().map(|(_, e)| e));
            all.sort_by_key(|e| e.offset);
            ev_ins.push(all);
        }

        let param_values = self.run_params(key, graph, frames, events);
        let param_table: Vec<ParamInput<'_>> = param_values
            .iter()
            .map(|v| match v {
                Some(v) => ParamInput::Frames(v),
                None => ParamInput::Base,
            })
            .collect();

        let n_out = shape.audio_out.count() as usize;
        let cx = Cx {
            env,
            arrival: self.arrival[&key],
            history: &self.history,
        };
        // A crossfade here: the outgoing unit first, on its own copy of the
        // inputs, with no events; what it emits goes nowhere.
        let outgoing: Option<Vec<Vec<f32>>> = self.fading.get_mut(&key).map(|f| {
            let mut o: Vec<Vec<f32>> = vec![vec![0.0; frames]; n_out];
            let copies = ins.clone();
            let status = {
                let in_refs: Vec<&[f32]> = copies.iter().map(Vec::as_slice).collect();
                let mut out_refs: Vec<&mut [f32]> = o.iter_mut().map(Vec::as_mut_slice).collect();
                let none: Vec<SortedEvents<'_>> =
                    vec![SortedEvents::EMPTY; shape.event_in as usize];
                // Detached writers, as the executor's: every push refused and
                // nothing counted, so a node that reacts to a refusal does
                // the same under both.
                let mut writers: Vec<EventWriter<'_>> = (0..shape.event_out)
                    .map(|_| EventWriter::detached())
                    .collect();
                let io = Io::new(
                    self.prepare.max_block(),
                    frames,
                    &in_refs,
                    &mut out_refs,
                    SilenceMask::NONE,
                    ConstantMask::NONE,
                    InPlaceMask::NONE,
                    &none,
                    &mut writers,
                )
                .with_params(&param_table);
                f.old.process(&cx, io)
            };
            settle(status, &copies, &mut o);
            o
        });
        let mut outs: Vec<Vec<f32>> = vec![vec![0.0; frames]; n_out];
        let mut ev_bufs: Vec<Vec<Event>> = vec![Vec::new(); shape.event_out as usize];
        let (status, refused) = {
            let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
            let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            let ev_refs: Vec<SortedEvents<'_>> = ev_ins
                .iter()
                .map(|v| SortedEvents::new(v, frames).expect("the reference sorted them"))
                .collect();
            let drops = crate::event::PortCount::default();
            // The node's declared capacity bounds its writers here as in the
            // executor; undeclared, nothing does.
            let cap = shape
                .event_capacity
                .map_or(usize::MAX, |n| n.get() as usize);
            let mut writers: Vec<EventWriter<'_>> = ev_bufs
                .iter_mut()
                .map(|b| EventWriter::new(b, cap, frames as u32, &drops))
                .collect();
            let io = Io::new(
                self.prepare.max_block(),
                frames,
                &in_refs,
                &mut out_refs,
                SilenceMask::NONE,
                ConstantMask::NONE,
                InPlaceMask::NONE,
                &ev_refs,
                &mut writers,
            )
            .with_params(&param_table);
            let status = self
                .units
                .get_mut(&key)
                .expect("unit present")
                .1
                .process(&cx, io);
            (status, drops.refused.get())
        };
        self.dropped += u64::from(refused);
        settle(status, &ins, &mut outs);
        if let Some(old) = outgoing {
            let f = self.fading.get_mut(&key).expect("fading");
            for i in 0..frames {
                let k = f.done + i;
                if k >= f.len {
                    break;
                }
                let (g_in, g_out) = f.curve.gains(k, f.len);
                for (o, x) in outs.iter_mut().zip(&old) {
                    o[i] = o[i] * g_in + x[i] * g_out;
                }
            }
            f.done = (f.done + frames).min(f.len);
        }
        for (port, o) in outs.into_iter().enumerate() {
            audio.insert(
                OutPort {
                    node: key,
                    port: port as u16,
                },
                o,
            );
        }
        for (port, e) in ev_bufs.into_iter().enumerate() {
            let from = EventOut {
                node: key,
                port: port as u16,
            };
            if self.tracked.contains(&from) {
                self.held.entry(from).or_default().observe(&e);
            }
            events.insert(from, e);
        }
    }
}

impl Reference {
    /// Edge `id`'s lane for this block: `None` for an edge with no entry.
    /// Its weights are the doc's ramp from the weight the previous block was
    /// given to the base `targets` holds, or, when a source drives it, the
    /// values of the param port's rule (`ref_port`) over the source's events
    /// through its PDC delay. A lane's state advances here, once a block.
    fn lane_weight(
        &mut self,
        graph: &ValidGraph,
        id: EdgeId,
        targets: &BTreeMap<EdgeId, Amplitude>,
        frames: usize,
        events: &BTreeMap<EventOut, Vec<Event>>,
    ) -> Option<RefWeight> {
        let gain = graph.gains().get(&id)?;
        let to = targets[&id];
        let ran = self.lane_prev.contains_key(&id);
        let evs: Vec<Event> = match gain.source {
            Some(from) => {
                let src = events[&from].clone();
                match self.event_lines.get_mut(&DelayKey::EdgeGain { id, from }) {
                    Some(f) => fifo_run(f, &src, frames),
                    None => src,
                }
            }
            None => Vec::new(),
        };
        // A lane's first block starts at its value; one that has run is on
        // a running port, so a source connected to it later declicks.
        if gain.source.is_some() || self.gain_ports.contains_key(&id) {
            let range = self.gain_ranges.get(&id).copied().unwrap_or(gain.range);
            let st = self.gain_ports.entry(id).or_default();
            if let Some(vals) = ref_port(
                st,
                !ran,
                RefPortIn {
                    param: crate::EdgeGain::ADDR,
                    b1: to.get(),
                    range,
                    source: gain.source,
                    events: &evs,
                    frames,
                },
            ) {
                self.lane_prev.insert(id, Amplitude::new(vals[frames - 1]));
                return Some(RefWeight::Frames(vals));
            }
        }
        let from = self.lane_prev.get(&id).copied().unwrap_or(to);
        self.lane_prev.insert(id, to);
        Some(RefWeight::Ramp { from, to })
    }
}

impl Reference {
    /// Each declared param of `key` for this block: `Some` of its values
    /// when modulated or declicking, `None` when it reads its base. Written
    /// from the spec and the port outputs alone (see the module docs).
    fn run_params(
        &mut self,
        key: NodeKey,
        graph: &ValidGraph,
        frames: usize,
        events: &BTreeMap<EventOut, Vec<Event>>,
    ) -> Vec<Option<Vec<f32>>> {
        let unit = &self.units[&key].1;
        let declared = unit.shape().params;
        let mut out = Vec::new();
        for (k, &param) in declared.as_slice().iter().enumerate() {
            let at = ParamIn { node: key, param };
            let Some(b1) = unit.param_base(k) else {
                self.params.remove(&at);
                out.push(None);
                continue;
            };
            let (range, source) = graph
                .params()
                .get(&at)
                .map(|m| (m.range, m.source))
                .unwrap_or_default();
            let range = self.ranges.get(&at).copied().unwrap_or(range);
            // The source's events this block, through its PDC delay.
            let evs: Vec<Event> = match source {
                Some(from) => {
                    let src = events[&from].clone();
                    match self.event_lines.get_mut(&DelayKey::ParamEvent { at, from }) {
                        Some(f) => fifo_run(f, &src, frames),
                        None => src,
                    }
                }
                None => Vec::new(),
            };

            let fresh = !self.params.contains_key(&at);
            let st = self.params.entry(at).or_default();
            out.push(ref_port(
                st,
                fresh,
                RefPortIn {
                    param,
                    b1,
                    range,
                    source,
                    events: &evs,
                    frames,
                },
            ));
        }
        out
    }
}

/// What one param port (or edge gain lane) is given for a block.
struct RefPortIn<'a> {
    param: tutti_types::UnitAddr,
    /// The base at the end of the block.
    b1: f32,
    range: ParamRange,
    source: Option<EventOut>,
    /// The source's events, through its PDC delay.
    events: &'a [Event],
    frames: usize,
}

/// One block of the param port step, written from the rules in `param.rs`'s
/// docs: `Some` of its values when modulated or declicking, `None` when it
/// reads its base. `fresh` is the port's first block. Shared by a node's
/// param ports and an edge gain's lane, which are one rule.
fn ref_port(st: &mut RefParam, fresh: bool, p: RefPortIn<'_>) -> Option<Vec<f32>> {
    let RefPortIn {
        param,
        b1,
        range,
        source,
        events: evs,
        frames,
    } = p;
    let decl = PARAM_DECLICK.get();
    if fresh {
        st.source = source;
    } else if st.source != source {
        st.hold = if st.modulated { st.last } else { b1 };
        st.fading = decl;
        st.source = source;
        st.ramp = RefRamp::default();
    }
    let start = st.clock;
    st.clock += frames as u64;
    if source.is_none() && st.fading == 0 {
        st.modulated = false;
        return None;
    }
    // The base at frame `i`: ramped from the last block's to this
    // block's, landing on it at the last frame.
    let from = if st.modulated { st.last_base } else { b1 };
    let base = |i: usize| {
        if i == frames - 1 {
            b1
        } else {
            from + (b1 - from) * ((i + 1) as f32 / frames as f32)
        }
    };
    let (lo, hi) = if range.min <= range.max {
        (range.min, range.max)
    } else {
        (range.max, range.min)
    };
    // The event source's value at every frame, the base before its
    // first event; it replaces the base, clamped.
    let mut vals = vec![0.0f32; frames];
    let mut evs = evs.iter().peekable();
    for (i, v) in vals.iter_mut().enumerate() {
        if source.is_none() {
            *v = base(i);
            continue;
        }
        let now = start + i as u64;
        let want = tutti_types::ParamAddr::unit(param);
        while let Some(e) = evs.next_if(|e| e.offset.index() <= i) {
            match e.kind {
                EventKind::Ramp(r) if r.addr() == want => {
                    // A first ramp starts from the base at this frame.
                    let cur = if st.ramp.started {
                        st.ramp.value
                    } else {
                        base(i)
                    };
                    st.ramp = RefRamp {
                        from: cur,
                        target: r.raw_target(),
                        at: now,
                        len: r.duration().get() as u64,
                        shape: r.shape(),
                        curve: None,
                        value: cur,
                        started: true,
                    };
                }
                EventKind::Segment(s) if s.addr() == want => {
                    let (from, to, position, length) = s.raw();
                    // What a ramp replacing it on this frame starts
                    // from: the held value, or (a first event) the
                    // base at this frame, as for a first ramp.
                    let cur = if st.ramp.started {
                        st.ramp.value
                    } else {
                        base(i)
                    };
                    st.ramp = RefRamp {
                        from,
                        target: to,
                        at: now,
                        len: 0,
                        shape: tutti_curve::Law::Linear,
                        curve: Some((position, length, s.shape())),
                        value: cur,
                        started: true,
                    };
                }
                // Another param's, MIDI, a chord, or a resync: a port re-sends
                // nothing.
                EventKind::Ramp(_)
                | EventKind::Segment(_)
                | EventKind::Midi(_)
                | EventKind::Harmony(_)
                | EventKind::Resync => {}
            }
        }
        let r = &st.ramp;
        let value = if !r.started {
            base(i)
        } else if now < r.at {
            r.value
        } else if let Some((position, length, shape)) = r.curve {
            // Frames since its event, `0` on the event's own.
            let j = u32::try_from(now - r.at).unwrap_or(u32::MAX);
            crate::event::segment_value(r.from, r.target, shape, position, length, j)
        } else {
            let into = now - r.at + 1;
            if into >= r.len {
                r.target
            } else {
                let t = into as f32 / r.len as f32;
                r.from + (r.target - r.from) * r.shape.eval(t)
            }
        };
        st.ramp.value = value;
        *v = value.clamp(lo, hi);
    }
    for (i, v) in vals.iter_mut().enumerate() {
        if st.fading > 0 && i < st.fading {
            let j = decl - st.fading + i;
            let g = 1.0 - (j + 1) as f32 / decl as f32;
            *v += (st.hold - *v) * g;
        }
    }
    st.fading = st.fading.saturating_sub(frames);
    st.last = vals[frames - 1];
    st.last_base = b1;
    st.modulated = true;
    Some(vals)
}

/// `(MIDI 2.0, group, channel, note)`.
type RefNote = (bool, u8, u8, NoteNumber);

#[derive(Debug, Default)]
struct RefHeld {
    notes: BTreeSet<RefNote>,
    /// `(MIDI 2.0, group, channel)` with the pedal down.
    pedals: BTreeSet<(bool, u8, u8)>,
}

enum RefKey {
    On(RefNote),
    Off(RefNote),
    Pedal(bool, u8, u8, bool),
}

fn ref_key(e: &Event) -> Option<RefKey> {
    let EventKind::Midi(u) = e.kind else {
        return None;
    };
    match decode(u).ok()? {
        Message::Midi1(a, v) => {
            let (g, c) = (a.group.get(), a.channel.get());
            match v {
                Voice1::NoteOn { note, velocity } if velocity.get() == 0 => {
                    Some(RefKey::Off((false, g, c, note)))
                }
                Voice1::NoteOn { note, .. } => Some(RefKey::On((false, g, c, note))),
                Voice1::NoteOff { note, .. } => Some(RefKey::Off((false, g, c, note))),
                Voice1::ControlChange { controller, value } if controller == CCNumber::SUSTAIN => {
                    Some(RefKey::Pedal(false, g, c, value.get() >= 64))
                }
                _ => None,
            }
        }
        Message::Midi2(a, v) => {
            let (g, c) = (a.group.get(), a.channel.get());
            match v {
                Voice2::NoteOn { note, .. } => Some(RefKey::On((true, g, c, note))),
                Voice2::NoteOff { note, .. } => Some(RefKey::Off((true, g, c, note))),
                Voice2::ControlChange { controller, value } if controller == CCNumber::SUSTAIN => {
                    Some(RefKey::Pedal(true, g, c, value >= 0x8000_0000))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

impl RefHeld {
    fn observe(&mut self, events: &[Event]) {
        for e in events {
            match ref_key(e) {
                Some(RefKey::On(k)) => {
                    self.notes.insert(k);
                }
                Some(RefKey::Off(k)) => {
                    self.notes.remove(&k);
                }
                Some(RefKey::Pedal(m2, g, c, down)) => {
                    if down {
                        self.pedals.insert((m2, g, c));
                    } else {
                        self.pedals.remove(&(m2, g, c));
                    }
                }
                None => {}
            }
        }
    }

    /// Pedals up, then every note off, at one offset.
    fn closing(&self, offset: Offset) -> Vec<Event> {
        let mut out = Vec::new();
        let addr = |g: u8, c: u8| Address {
            group: MidiGroup::of(g),
            channel: MidiChannel::of(c),
        };
        for &(m2, g, c) in &self.pedals {
            let cc = CCNumber::SUSTAIN;
            let msg = if m2 {
                Message::Midi2(
                    addr(g, c),
                    Voice2::ControlChange {
                        controller: cc,
                        value: 0,
                    },
                )
            } else {
                Message::Midi1(
                    addr(g, c),
                    Voice1::ControlChange {
                        controller: cc,
                        value: U7::of(0),
                    },
                )
            };
            out.push(Event {
                offset,
                kind: EventKind::Midi(Ump::from(msg)),
            });
        }
        for &(m2, g, c, note) in &self.notes {
            let msg = if m2 {
                Message::Midi2(addr(g, c), Voice2::note_off(note, 0))
            } else {
                Message::Midi1(
                    addr(g, c),
                    Voice1::NoteOff {
                        note,
                        velocity: U7::of(0),
                    },
                )
            };
            out.push(Event {
                offset,
                kind: EventKind::Midi(Ump::from(msg)),
            });
        }
        out
    }
}

fn edges_of(g: &ValidGraph) -> BTreeSet<(EventOut, EventIn)> {
    g.events()
        .iter()
        .flat_map(|(&at, v)| v.iter().map(move |e| (e.from(), at)))
        .collect()
}

/// Apply a node's status to the outputs it was handed.
fn settle(status: Status, ins: &[Vec<f32>], outs: &mut [Vec<f32>]) {
    match status {
        Status::Modified | Status::Masked { .. } => {}
        Status::Silent | Status::Idle => outs.iter_mut().for_each(|o| o.fill(0.0)),
        Status::Constant => outs.iter_mut().for_each(|o| {
            let v = o[0];
            o.fill(v);
        }),
        Status::Bypass => {
            for (c, o) in outs.iter_mut().enumerate() {
                match ins.get(c) {
                    Some(i) => o.copy_from_slice(i),
                    None => o.fill(0.0),
                }
            }
        }
    }
}

/// A crossfade in the reference: the unit fading out, how far the fade has
/// got, and a replace waiting behind it.
struct RefFade {
    old: Box<dyn Node>,
    done: usize,
    len: usize,
    curve: crate::CrossfadeCurve,
    next: Option<(Box<dyn Node>, crate::Fade)>,
}

/// A command waiting in the reference.
struct RefCommand {
    id: u64,
    at: At,
    to: EventIn,
    kind: EventKind,
    /// Its timeline position, unrounded: the frame it was scheduled for
    /// (rescaled on a rate change), or where a beat resolved. The tie-break
    /// after the offset.
    pos: f64,
}

/// A frame of slack, in beats, when no tempo is usable.
const STILL: f64 = 1e-9;
/// Beat → frame rounding: the first frame at or after the beat, within a
/// millionth of a frame.
const FRAME_ROUNDING: f64 = 1e-6;

/// One block's continuous traversal, stepped through naively: the beat
/// intervals `[from, to)` it covers, each with the frame (from the block's
/// start) at which it begins.
struct Traversal {
    segs: Vec<(f64, f64, f64)>,
}

/// What `t` traverses in `len` frames at `tempo` and `rate`.
fn traverse(t: &Transport, tempo: f64, rate: f64, len: usize) -> Traversal {
    let beat = t.beat().get();
    let usable = |x: f64| x.is_finite() && x > 0.0;
    if !t.playing || !usable(tempo) || !usable(rate) {
        return Traversal { segs: Vec::new() };
    }
    let fpb = rate * 60.0 / tempo;
    let (mut pos, mut left, mut frame0) = (beat, len as f64 / fpb, 0.0);
    let mut segs = Vec::new();
    loop {
        match t.looping {
            Some(l) if l.start.get() < l.end.get() && pos < l.end.get() => {
                let room = l.end.get() - pos;
                if left < room {
                    segs.push((pos, pos + left, frame0));
                    pos += left;
                    break;
                }
                segs.push((pos, l.end.get(), frame0));
                frame0 += room * fpb;
                left -= room;
                pos = l.start.get();
                if left <= 0.0 {
                    break;
                }
            }
            _ => {
                segs.push((pos, pos + left, frame0));
                pos += left;
                break;
            }
        }
    }
    let _ = pos;
    Traversal { segs }
}

/// The beat intervals a playhead at `t.beat` covers moving `dist` beats
/// forward, wrapping at `t`'s loop.
fn walk(t: &Transport, dist: f64) -> Vec<(f64, f64)> {
    let (mut pos, mut left) = (t.beat().get(), dist.max(0.0));
    let mut path = Vec::new();
    while left > 0.0 {
        match t.looping {
            Some(l) if l.start.get() < l.end.get() && pos < l.end.get() => {
                let room = l.end.get() - pos;
                if left < room {
                    path.push((pos, pos + left));
                    break;
                }
                path.push((pos, l.end.get()));
                left -= room;
                pos = l.start.get();
            }
            _ => {
                path.push((pos, pos + left));
                break;
            }
        }
    }
    path
}

/// The reference's record of continuous playback: every beat interval
/// traversed since the last discontinuity, as a list. Deliberately naive and
/// independent of the executor's `Playhead`, so a shared-decision bug in
/// either shows up as a divergence.
#[derive(Default)]
struct RefPlayhead {
    history: Vec<(f64, f64)>,
    last: Option<(Transport, f64, usize)>,
    now: Option<Transport>,
}

impl RefPlayhead {
    /// Record the block about to be rendered.
    fn observe(&mut self, t: &Transport, rate: f64, len: usize) {
        let now = t.beat().get();
        let walked = self.last.and_then(|(p, prate, plen)| {
            if p.looping != t.looping {
                return None;
            }
            // How far the previous block could have carried the playhead:
            // its length at any tempo between its own and this block's (the
            // tempo changed somewhere inside it), in beats; nothing if it was
            // stopped. Slack: one frame at the faster tempo.
            let usable = |x: f64| x.is_finite() && x > 0.0;
            let tempos: Vec<f64> = [p.tempo.get(), t.tempo.get()]
                .into_iter()
                .filter(|&x| usable(x) && usable(prate))
                .collect();
            let beats = |tempo: f64| plen as f64 * tempo / (60.0 * prate);
            let (lo, hi) = if !p.playing || tempos.is_empty() {
                (0.0, 0.0)
            } else {
                let d: Vec<f64> = tempos.iter().map(|&x| beats(x)).collect();
                (
                    d.iter().cloned().fold(f64::MAX, f64::min),
                    d.iter().cloned().fold(0.0, f64::max),
                )
            };
            let slack = [p.tempo.get(), t.tempo.get()]
                .into_iter()
                .filter(|&x| usable(x) && usable(rate))
                .map(|x| x / (60.0 * rate))
                .fold(STILL, f64::max);
            // Every distance that would put the playhead at `now`: straight
            // there, or round the loop once, twice, ...
            let from = p.beat().get();
            let mut distances = vec![now - from];
            if let Some(l) = p.looping {
                let (start, end) = (l.start.get(), l.end.get());
                // Only a playhead inside the loop can have wrapped there.
                let inside = now >= start - slack && now < end + slack;
                if start < end && from < end && inside {
                    let mut d = (end - from) + (now - start);
                    while d <= hi + slack {
                        distances.push(d);
                        d += end - start;
                    }
                }
            }
            distances
                .into_iter()
                .find(|&d| d >= lo - slack && d <= hi + slack)
                .map(|d| walk(&p, d))
        });
        match walked {
            Some(path) => self.history.extend(path),
            None => self.history.clear(),
        }
        self.last = Some((*t, rate, len));
        self.now = Some(*t);
    }

    /// Whether continuous playback went through `beat` before this block —
    /// and this pass of a loop will not reach it again (a beat ahead of the
    /// playhead inside the loop is reached again, so it is not crossed).
    /// A beat at or ahead of the playhead is never crossed, even where the
    /// history went past it: a run continues through a step back of up to a
    /// frame (a seek that small is indistinguishable from a host's `f32`
    /// beat jitter), and playback reaches that beat again.
    fn crossed(&self, beat: f64) -> bool {
        if self.now.is_none_or(|t| beat >= t.beat().get()) {
            return false;
        }
        let traversed = self
            .history
            .iter()
            .any(|&(from, to)| from <= beat && beat < to);
        let reached_again = self.now.is_some_and(|t| {
            t.looping.is_some_and(|l| {
                beat >= l.start.get() && beat < l.end.get() && beat >= t.beat().get()
            })
        });
        traversed && !reached_again
    }
}

impl Reference {
    /// Where `cmd` lands this block: `Some(Some(offset))`, `Some(None)` for
    /// late (offset 0), or `None` for not yet. Written out from scratch —
    /// no `Env::due`, no `Playhead` — so the executor's time logic has an
    /// independent check.
    fn land(
        &self,
        cmd: &mut RefCommand,
        arrival: Latency,
        pieces: &[(usize, usize, Transport)],
        frames: usize,
    ) -> Option<Option<Offset>> {
        let start = self.frame.get();
        let at_frame = |f: u64| -> Option<Option<Offset>> {
            let target = f + arrival.samples().get() as u64;
            if target < start {
                Some(None)
            } else if target - start < frames as u64 {
                Some(Some(Offset::raw((target - start) as u32)))
            } else {
                None
            }
        };
        match cmd.at {
            At::NextBlock => {
                cmd.pos = start as f64;
                Some(Some(Offset::ZERO))
            }
            At::Frame(f) => at_frame(f.get()),
            At::Beat(b) => {
                let b = b.get();
                let rate = self.prepare.sample_rate().get();
                // The first piece of the block whose playback reaches it.
                let reached = pieces.iter().find_map(|&(cut, len, t)| {
                    let tempo = t.tempo.get();
                    let fpb = rate * 60.0 / tempo;
                    // A beat less than a frame (minus the rounding) behind
                    // this piece's start is its first frame: a frame at or
                    // after the beat that the piece before could not reach.
                    let now = t.beat().get();
                    let b = if t.playing && b < now && (now - b) * fpb < 1.0 - FRAME_ROUNDING {
                        now
                    } else {
                        b
                    };
                    traverse(&t, tempo, rate, len)
                        .segs
                        .iter()
                        .find(|&&(from, to, _)| from <= b && b < to)
                        .map(|&(from, _, frame0)| {
                            ((b - from) * fpb + frame0 - FRAME_ROUNDING).ceil().max(0.0)
                        })
                        .filter(|&k| k < len as f64)
                        .map(|k| cut as f64 + k)
                });
                match reached {
                    Some(k) => {
                        let f = start + k as u64;
                        cmd.at = At::Frame(Frame(f));
                        cmd.pos = f as f64;
                        at_frame(f)
                    }
                    None if self.playhead.crossed(b) => Some(None),
                    None => None,
                }
            }
        }
    }
}

/// Push every sample through the line: in at the back, out at the front.
fn delay_line(line: &mut VecDeque<f32>, buf: &mut [f32]) {
    for x in buf.iter_mut() {
        line.push_back(*x);
        *x = line.pop_front().expect("line is non-empty");
    }
}

/// A vanished FIFO's events for a flush: due times relative to the
/// earliest, so their spacing survives.
fn relative(f: &RefFifo) -> Vec<Event> {
    let Some(&(first, _)) = f.pending.first() else {
        return Vec::new();
    };
    f.pending
        .iter()
        .map(|&(t, e)| Event {
            offset: Offset::raw((t - first).min(u64::from(u32::MAX)) as u32),
            ..e
        })
        .collect()
}

/// Take what falls due in the block starting at the FIFO's clock, without
/// queueing or advancing (a feedback read).
fn fifo_due(f: &mut RefFifo, frames: usize) -> Vec<Event> {
    let start = f.clock;
    let end = start + frames as u64;
    let due = f
        .pending
        .iter()
        .take_while(|(t, _)| t + f.len < end)
        .count();
    f.pending
        .drain(..due)
        .map(|(t, e)| Event {
            offset: Offset::raw((t + f.len).saturating_sub(start) as u32),
            ..e
        })
        .collect()
}

fn fifo_run(f: &mut RefFifo, input: &[Event], frames: usize) -> Vec<Event> {
    let start = f.clock;
    let end = start + frames as u64;
    f.pending.extend(
        input
            .iter()
            .map(|e| (start + u64::from(e.offset.get()), *e)),
    );
    let due = f
        .pending
        .iter()
        .take_while(|(t, _)| t + f.len < end)
        .count();
    let out = f
        .pending
        .drain(..due)
        .map(|(t, e)| Event {
            offset: Offset::raw((t + f.len).saturating_sub(start) as u32),
            ..e
        })
        .collect();
    f.clock = end;
    out
}

/// Every audio edge with its sink port, in port then sorted order.
fn flat_edges(t: &crate::Topology) -> impl Iterator<Item = (&InPort, &Edge)> {
    t.edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
}

/// A sink's sources in source order: `Edge`'s and `Source`'s derived `Ord`,
/// whatever order the spec lists them in (the compiler sorts its list alike).
fn sorted<T: Copy + Ord>(set: &[T]) -> Vec<T> {
    let mut v = set.to_vec();
    v.sort();
    v
}

/// One addend of a sink's sum as the reference holds it: its samples, and the
/// weights of its lane when its edge has a gain entry.
struct RefAddend {
    buf: Vec<f32>,
    weight: Option<RefWeight>,
}

/// A weighted lane's weights for one block.
enum RefWeight {
    /// Ramping across the block from the weight the previous block was given
    /// to this block's.
    Ramp { from: Amplitude, to: Amplitude },
    /// A driven lane: the weight at every frame.
    Frames(Vec<f32>),
}

/// The value a sink holds: none is silence, one plain addend is itself (an
/// alias, so a `-0.0` stays), anything else is the left fold
/// `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..` in `f32`, one rounding per add
/// and per multiply, each lane weighted by
/// `w[i] = from + (to - from) * ((i + 1) / frames)` (doc 013, "Audio fan-in sums", points 1 and 2, and "Edge
/// gain"). Skips nothing, silent addends and a weight of `1.0` or `0.0`
/// included: the executor skips the silent ones and takes no multiply at
/// `1.0`, and must agree.
fn sum_addends(mut addends: Vec<RefAddend>, frames: usize) -> Vec<f32> {
    match addends.len() {
        0 => vec![0.0; frames],
        1 if addends[0].weight.is_none() => addends.remove(0).buf,
        _ => {
            let mut acc = vec![0.0f32; frames];
            for a in &addends {
                for (i, (x, y)) in acc.iter_mut().zip(&a.buf).enumerate() {
                    *x = match &a.weight {
                        None => *x + *y,
                        Some(RefWeight::Ramp { from, to }) => {
                            let (from, to) = (from.get(), to.get());
                            let w = from + (to - from) * ((i + 1) as f32 / frames as f32);
                            *x + w * *y
                        }
                        Some(RefWeight::Frames(w)) => *x + w[i] * *y,
                    };
                }
            }
            acc
        }
    }
}
