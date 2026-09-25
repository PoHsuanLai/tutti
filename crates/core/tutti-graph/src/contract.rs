//! The sample-accuracy contract suite's harness (doc 013 §6, "Proof"): for
//! a node and a way to excite it, run it through every path the graph can
//! put it on and assert that an excitation at frame `F` produces its
//! response at exactly frame `F + arrival + latency`.
//!
//! Behind the `contract` feature (off by default): it is test support, for
//! this crate's suite and for node crates adding rows (Phase 4 ports each
//! node natively and adds its row here). Enable it from a dev-dependency.
//!
//! # A row
//!
//! A [`Row`] is a node constructor, an [`Excite`] (an event on an event
//! port, or an audio impulse on an audio port), a [`Detect`] (the first
//! sample above a threshold, or an exact expected response) and the output
//! channel to watch. [`Row::check`] runs one [`Path`]; the
//! [`contract_tests!`](crate::contract_tests) macro writes one `#[test]` per
//! path, so each path is a separate case that fails on its own.
//!
//! ```
//! use tutti_graph::contract::{Detect, Excite, Path, Pulse, Row};
//! use tutti_graph::{EventKind, Ump};
//! use tutti_types::{Latency, Samples};
//!
//! let row = Row::new(
//!     "pulse, 37 frames late",
//!     || Box::new(Pulse::new(Latency::new(Samples(37)))),
//!     Excite::Event { port: 0, kind: EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0])) },
//!     Detect::Exact(vec![1.0]),
//! );
//! row.check(Path::BehindPdc);
//! ```
//!
//! # What "exactly" means
//!
//! The expected frame is computed, never measured: `F` is where the
//! excitation was put on the timeline, `arrival` is the node's compiled
//! arrival latency (asserted against what the path built, so a harness bug
//! cannot quietly turn a PDC path into a direct one), and `latency` is the
//! node's **declared** [`Shape::latency`]. A node whose DSP delays by more or
//! less than it declares fails every path — which is the D1–D3 class in doc
//! 013. An event excitation is held to the node's declared
//! [`Shape::event_resolution`] (doc 013 §6): `Sample` is the exact frame,
//! `Frames(n)` any frame within `n - 1` of it **in either direction** (no
//! grid origin is assumed, so a node chunking on its own cursor honours it),
//! `Block` any frame within the block the event arrives in. A node finer
//! than it declares passes. An audio impulse is always exact.
//!
//! For an [`Detect::Exact`] row the whole render is checked: exact silence
//! except each expected response, so a duplicated or a dropped delivery
//! fails as surely as a mistimed one.
//!
//! Every excitation is swept over [`OFFSETS`], so the offset inside its block
//! is 0, 1, either side of a 64-frame `Legacy` chunk boundary, the middle
//! and the last frame — and, behind PDC, both where the excitation starts
//! and where the node sees it.
//!
//! # What is not here
//!
//! A node fed out of band — through a MIDI mailbox (the polysynth, the
//! SoundFont player, plugin instruments under `Legacy`) — has no row: the
//! harness can only time what the graph delivers. Such a node's events are
//! neither PDC-compensated nor stamped against the graph's blocks (`Legacy`
//! calls the unit in 64-frame chunks, and a mailbox offset is relative to
//! whichever chunk polls it), so it cannot honour this contract until events
//! are its ports (doc 013 Phase 4).
//!
//! # The fork's snapshot
//!
//! [`IsolateRow`] (and its one-control form [`assert_isolate_snapshots`])
//! checks the other promise a node crate makes here: that a forkable unit's
//! `isolate` severs every live control it reads, so a fork renders the
//! controls as they were at fork time. See `src/contract/snapshot.rs`.

use tutti_node::AudioUnit;
use tutti_types::{At, Beat, Bpm, ChannelLayout, Frame, Latency, NodeKey, SampleRate, Samples};

use crate::builder::GraphBuilder;
use crate::editor::Editor;
use crate::event::{Event, EventKind};
use crate::exec::Executor;
use crate::io::Io;
use crate::legacy::Legacy;
use crate::node::{
    Cx, IntoNode, Node, Prepare, Resolution, Shape, Status, Transport, TransportChanges,
};
use crate::spec::EventIn;

mod snapshot;
pub use snapshot::{assert_isolate_snapshots, IsolateRow, SNAPSHOT_FRAMES};

/// The rate every contract graph runs at.
pub const SAMPLE_RATE: SampleRate = SampleRate(48_000.0);

/// The `MaxBlock` every contract graph is prepared for. Above 65, so the
/// ragged schedule's 65-frame block fits, and not a multiple of anything a
/// node under test chunks by except 64.
pub const MAX_BLOCK: usize = 128;

/// The latent sibling's declared latency on the PDC paths, and so the
/// node's arrival there. Longer than [`MAX_BLOCK`], so at least one block
/// boundary falls while an excitation is in the PDC delay (the recompile
/// paths commit on it). Not a multiple of 64 or of any block size a
/// schedule uses, so the delay moves an excitation's offset: the harness
/// therefore puts excitations both where the *source* sees each of
/// [`OFFSETS`] and where the *node* does.
pub const SIBLING_LATENCY: usize = 141;

/// Where, inside its block, each excitation is put (under whole
/// [`MAX_BLOCK`] blocks; a ragged schedule moves them).
pub const OFFSETS: [usize; 6] = [0, 1, 63, 64, 100, MAX_BLOCK - 1];

/// Frames rendered before the earliest excitation, so a node's start-up
/// (a limiter's envelope, a convolver's first block) is behind it.
const WARMUP: u64 = 4 * MAX_BLOCK as u64;

/// Frames per beat at 120 BPM and [`SAMPLE_RATE`].
const FRAMES_PER_BEAT: u64 = 24_000;

/// How far after the timed start each [`Path::ScheduledBeat`] command's
/// beat is, in frames: on the start's own frame (the hardest case: the beat
/// the transport begins on, reachable only through the change inside the
/// block), a dozen frames on (usually in the start's block), and a quarter
/// beat on (well past it).
const BEAT_LEADS: [u64; 3] = [0, 12, FRAMES_PER_BEAT / 4];

/// A key no contract graph uses, for the unrelated node the
/// [`Path::RecompileUnrelated`] edit inserts.
const UNRELATED: NodeKey = NodeKey(u64::MAX - 1);

/// How a row excites its node.
#[derive(Clone, Copy, Debug)]
pub enum Excite {
    /// Deliver `kind` into event input `port`. Every path that has an event
    /// source applies: an upstream node's output, a fan-in merge, a
    /// scheduled command at a frame or a beat.
    Event {
        /// The node's event input.
        port: u16,
        /// What to deliver.
        kind: EventKind,
    },
    /// Put one sample of `amplitude` on audio input `port` (the others read
    /// silence).
    Impulse {
        /// The node's audio input.
        port: u16,
        /// The impulse's height.
        amplitude: f32,
    },
}

/// How a row finds its node's response in the watched output.
#[derive(Clone, Debug)]
pub enum Detect {
    /// The response starts at the first sample whose magnitude is above
    /// this. For a node whose response is not known sample for sample (a
    /// limiter's gain, an HRIR), and whose output before it is silence.
    Threshold(f32),
    /// The response is exactly these samples, and everything before it is
    /// exactly zero.
    Exact(Vec<f32>),
}

/// One path through the graph. Each is a separate case (see
/// [`contract_tests!`](crate::contract_tests)), and each has a mutation it
/// was seen to fail under, recorded on its variant (and, for the `Legacy`
/// rows, in the node crates' `tests/contract.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// The excitation straight in: an upstream event source, or the global
    /// input. Whole [`MAX_BLOCK`] blocks.
    ///
    /// Mutation (run): in `SubBlocks::next`, hand the whole block over as
    /// one chunk carrying every event → the native `Sample` rows apply their
    /// event at offset 0 → every path fails but `Blocks1` (where every
    /// offset is 0). In `Legacy::probe`, declare one frame more than `route`
    /// reports → every `Legacy` row fails every path.
    Direct,
    /// Behind PDC: a latent sibling ([`Latent`], [`SIBLING_LATENCY`])
    /// merges with the excitation's path upstream of the node, so the node's
    /// arrival is [`SIBLING_LATENCY`] and the excitation is delayed to match.
    /// Excitations are put both where the source and where the node sees
    /// each of [`OFFSETS`].
    ///
    /// Mutation (run): in `EventFifo::pop_due`, hold back an event due on a
    /// block's last frame to the next block (one frame late, on that frame
    /// only) → the `Sample` event rows fail this path, both recompile paths,
    /// `Blocks1`, `Blocks64` and `BlocksMax` (the coarser rows only
    /// `Blocks1`, where every frame is a last frame). Under whole blocks
    /// only the node-side offsets reach a block's last frame behind the
    /// 141-frame delay, so without them this path would pass.
    ///
    /// Mutation (run): in `compile`, treat every audio and event gap as zero
    /// (no `Delay`/`EventDelay` op) → the excitation arrives
    /// `SIBLING_LATENCY` early → this path, both recompile paths and every
    /// `Blocks*` path fail, while `Direct`, `EventFanIn` and the scheduled
    /// paths (whose commands are compensated separately) pass.
    BehindPdc,
    /// Through an event fan-in merge: two sources on the node's port, the
    /// exciting one second in source order, the first sending the same
    /// event a few frames **later**: usually in the same block, so a merge
    /// that is not by offset hands the node the two out of order.
    ///
    /// Both responses are expected, the second at `f + gap`, and nothing
    /// else.
    ///
    /// Mutation (run): in `merge_into`, take the first source with an event
    /// left rather than the earliest offset → only this path fails (not for
    /// the `Block` row, whose gap is a whole block). Drop the first
    /// source's events (the later one) → only this path fails, on the
    /// missing second response. Ties are `tests/contract.rs`'s
    /// `a_fan_in_tie_goes_by_source_order`.
    EventFanIn,
    /// Behind PDC, with an unrelated node inserted and committed while the
    /// excitation is in flight (between its frame and its response). After
    /// the next block the running plan must hold the new node, so the path
    /// cannot pass having recompiled nothing.
    ///
    /// Mutation (run): in `Executor::rebuild`, carry no delay ring across a
    /// commit → both recompile paths fail, and nothing else. Carry the
    /// event FIFOs with every queued event doubled (a re-delivery) → the
    /// event rows fail both recompile paths (`Pulse` *adds*, so a second
    /// delivery on one frame is a `2.0`). Skip the harness's commit → both
    /// fail on the plan check.
    RecompileUnrelated,
    /// Behind PDC, with the node that feeds this one (the summing node on
    /// the audio path, the exciting source on the event path) re-inserted
    /// (a new generation) while the excitation is in flight in the delay
    /// into it. After the next block the running plan must carry the
    /// feeder's new generation.
    ///
    /// Mutation (run): in `Executor::rebuild`, drop the carried ring of any
    /// delay whose sink or source changed generation → only this path fails.
    RecompileUpstreamGeneration,
    /// Direct and behind PDC, in blocks of one frame.
    Blocks1,
    /// Direct and behind PDC, in blocks of 63 frames.
    Blocks63,
    /// Direct and behind PDC, in blocks of 64 frames.
    Blocks64,
    /// Direct and behind PDC, in blocks of 65 frames.
    Blocks65,
    /// Direct and behind PDC, in [`MAX_BLOCK`] blocks.
    BlocksMax,
    /// Direct and behind PDC, in blocks of random lengths from 1 to
    /// [`MAX_BLOCK`] (a fixed seed, so a failure reproduces).
    ///
    /// Mutations (run), for the `Blocks*` family, each one assuming blocks
    /// come in whole 64-frame chunks:
    ///
    /// - `EventFifo::run` advancing its clock by the block rounded up to 64
    ///   → the event rows fail `Blocks1`, `Blocks63`, `Blocks65` and
    ///   `BlocksRandom`, and pass `Blocks64`, `BlocksMax` and every other
    ///   path;
    /// - [`Lookahead`] moving its ring by a whole chunk on a partial one →
    ///   its row fails `Blocks63`, `Blocks65` and `BlocksRandom` only;
    /// - `Legacy::process` calling the unit for a whole chunk when fewer
    ///   frames remain → the limiter and convolver rows fail `Blocks1`,
    ///   `Blocks63`, `Blocks65` and `BlocksRandom` only.
    BlocksRandom,
    /// Direct and behind PDC, the excitation delivered by
    /// [`Editor::schedule`] at `At::Frame(F)`.
    ///
    /// Mutation (run): land every due command at offset 0 of its block →
    /// both scheduled paths fail. Resolve every command at arrival zero →
    /// both scheduled paths fail (their PDC halves), and nothing else.
    ScheduledFrame,
    /// Direct and behind PDC, the excitation delivered at `At::Beat(b)`
    /// under a transport started by a timed start inside a block (a change
    /// in [`TransportChanges`]), for a beat on the start's own frame, a
    /// dozen frames after it and a quarter beat after it.
    ///
    /// Mutation (run): in `Env::due`, resolve a beat against the block's
    /// first transport only (ignore its changes) → only this path fails.
    ScheduledBeat,
}

impl Path {
    /// Every path.
    pub const ALL: [Path; 13] = [
        Path::Direct,
        Path::BehindPdc,
        Path::EventFanIn,
        Path::RecompileUnrelated,
        Path::RecompileUpstreamGeneration,
        Path::Blocks1,
        Path::Blocks63,
        Path::Blocks64,
        Path::Blocks65,
        Path::BlocksMax,
        Path::BlocksRandom,
        Path::ScheduledFrame,
        Path::ScheduledBeat,
    ];

    /// Whether the path needs an event excitation. The audio-impulse rows
    /// run every other one.
    pub const fn needs_events(self) -> bool {
        matches!(
            self,
            Path::EventFanIn | Path::ScheduledFrame | Path::ScheduledBeat
        )
    }
}

/// The graph around the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Topo {
    Direct,
    Pdc,
}

/// Where the excitation comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// An [`Emitter`] upstream, or the global input for an impulse.
    Source,
    /// Two emitters on one port.
    FanIn,
    /// `Editor::schedule(At::Frame(F))`.
    AtFrame,
    /// `Editor::schedule(At::Beat(..))` after a timed start.
    AtBeat,
}

/// A mid-stream recompile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    None,
    Unrelated,
    Regenerate,
}

/// A node, how to excite it, and how to find its response — one row of the
/// contract suite. See the module docs (`src/contract.rs`).
pub struct Row {
    name: String,
    make: Box<dyn Fn() -> Box<dyn Node>>,
    excite: Excite,
    detect: Detect,
    output: u16,
    latency: Option<Samples>,
}

impl Row {
    /// A row for the node `make` builds (a fresh one per run).
    pub fn new(
        name: &str,
        make: impl Fn() -> Box<dyn Node> + 'static,
        excite: Excite,
        detect: Detect,
    ) -> Self {
        if let Detect::Exact(p) = &detect {
            assert!(
                p.first().is_some_and(|&x| x != 0.0),
                "{name}: an exact response starts with a non-zero sample (it is found by it)"
            );
        }
        Self {
            name: name.to_string(),
            make: Box::new(make),
            excite,
            detect,
            output: 0,
            latency: None,
        }
    }

    /// A row for an `AudioUnit`, run through [`Legacy`] as a graph would run
    /// it today.
    pub fn legacy<U: AudioUnit + 'static>(
        name: &str,
        make: impl Fn() -> U + 'static,
        excite: Excite,
        detect: Detect,
    ) -> Self {
        Self::new(
            name,
            move || Legacy::new(make()).into_node().0,
            excite,
            detect,
        )
    }

    /// Watch output channel `channel` (the first by default).
    #[must_use]
    pub fn output(mut self, channel: u16) -> Self {
        self.output = channel;
        self
    }

    /// Also pin the node's declared latency, once prepared, to `latency`:
    /// for a node whose figure is known independently (an HRTF frame, a
    /// lookahead), so a declaration and a DSP that drift *together* still
    /// fail.
    #[must_use]
    pub fn expect_latency(mut self, latency: Samples) -> Self {
        self.latency = Some(latency);
        self
    }

    /// Run `path` over every offset in [`OFFSETS`], and panic, naming the
    /// row, the path, the excitation frame and both frames, at the first
    /// response that is not exactly where the contract puts it.
    ///
    /// # Panics
    ///
    /// On a contract failure, and when `path` needs an event excitation and
    /// this row has an audio one ([`Path::needs_events`]): a row cannot pass
    /// a path it does not run.
    pub fn check(&self, path: Path) {
        let is_event = matches!(self.excite, Excite::Event { .. });
        assert!(
            is_event || !path.needs_events(),
            "{}: {path:?} needs an event excitation; this row's is an audio impulse",
            self.name
        );
        let (topos, feed, edit, blocks): (&[Topo], Feed, Edit, Option<Schedule>) = match path {
            Path::Direct => (&[Topo::Direct], Feed::Source, Edit::None, None),
            Path::BehindPdc => (&[Topo::Pdc], Feed::Source, Edit::None, None),
            Path::EventFanIn => (&[Topo::Direct], Feed::FanIn, Edit::None, None),
            Path::RecompileUnrelated => (&[Topo::Pdc], Feed::Source, Edit::Unrelated, None),
            Path::RecompileUpstreamGeneration => {
                (&[Topo::Pdc], Feed::Source, Edit::Regenerate, None)
            }
            Path::Blocks1 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(1))),
            Path::Blocks63 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(63))),
            Path::Blocks64 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(64))),
            Path::Blocks65 => (BOTH, Feed::Source, Edit::None, Some(Schedule::Fixed(65))),
            Path::BlocksMax => (
                BOTH,
                Feed::Source,
                Edit::None,
                Some(Schedule::Fixed(MAX_BLOCK)),
            ),
            Path::BlocksRandom => (BOTH, Feed::Source, Edit::None, Some(Schedule::Random)),
            Path::ScheduledFrame => (BOTH, Feed::AtFrame, Edit::None, None),
            Path::ScheduledBeat => (BOTH, Feed::AtBeat, Edit::None, None),
        };
        let schedule = blocks.unwrap_or(Schedule::Fixed(MAX_BLOCK));
        let leads: &[u64] = if feed == Feed::AtBeat {
            &BEAT_LEADS
        } else {
            &[0]
        };
        // Room before the first excitation for the longest beat lead, in
        // whole blocks, so every timed start is past the warm-up too.
        let max = MAX_BLOCK as u64;
        let lead_room = leads.iter().copied().max().unwrap_or(0).div_ceil(max) * max;
        let base = WARMUP + lead_room;
        for &topo in topos {
            let arrival = match topo {
                Topo::Direct => 0,
                Topo::Pdc => SIBLING_LATENCY as u64,
            };
            for &k in &OFFSETS {
                let k = k as u64;
                // Each offset where the excitation *starts* (its source's
                // block, under whole blocks) and, behind PDC, also where the
                // *node* sees it: `SIBLING_LATENCY` moves every offset, so
                // without the second the node would never see a block's
                // first or last frame, or the 64-frame `Legacy` seam.
                let mut frames = vec![base + k];
                let at_node = base + (k + max - arrival % max) % max;
                if at_node != base + k {
                    frames.push(at_node);
                }
                for f in frames {
                    for &lead in leads {
                        self.run(path, topo, feed, edit, schedule, f, lead);
                    }
                }
            }
        }
    }

    /// One graph, one excitation (two on the fan-in path), one assertion
    /// over the whole render.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        path: Path,
        topo: Topo,
        feed: Feed,
        edit: Edit,
        schedule: Schedule,
        f: u64,
        lead: u64,
    ) {
        let node_shape = (self.make)().shape();
        let gap = self.fan_in_gap(node_shape.event_resolution);
        // On the fan-in path both events share one block when the gap
        // allows: the exciting one moves back rather than the later one
        // spilling into the next block.
        let max = MAX_BLOCK as u64;
        let f = if feed == Feed::FanIn && f % max + gap >= max && gap < max {
            f - gap
        } else {
            f
        };
        // On the beat path `f` is the command's beat, `lead` frames after a
        // timed start.
        let start = (feed == Feed::AtBeat).then(|| f - lead);
        let mut rig = self.build(topo, feed, f, gap);
        let want_arrival = match topo {
            Topo::Direct => 0,
            Topo::Pdc => SIBLING_LATENCY,
        };
        let ctx = format!(
            "{}: {path:?} ({topo:?}), excitation at frame {f} (the node sees offset {} in \
             whole blocks{})",
            self.name,
            (f + want_arrival as u64) % max,
            start.map_or(String::new(), |s| format!(", timed start at {s}"))
        );
        assert_eq!(
            rig.arrival.get(),
            want_arrival,
            "{ctx}: the harness built the wrong path (the node's arrival)"
        );
        if let Some(l) = self.latency {
            assert_eq!(
                rig.latency, l,
                "{ctx}: the node declares a latency other than the row pins"
            );
        }

        // Schedule the command now, when there is one: the plan is in.
        let to = EventIn {
            node: rig.node,
            port: self.event_port(),
        };
        match feed {
            Feed::AtFrame => {
                rig.ed
                    .schedule(At::Frame(Frame(f)), to, self.event_kind())
                    .expect("room for one command");
            }
            Feed::AtBeat => {
                rig.ed
                    .schedule(
                        At::Beat(Beat(lead as f64 / FRAMES_PER_BEAT as f64)),
                        to,
                        self.event_kind(),
                    )
                    .expect("room for one command");
            }
            Feed::Source | Feed::FanIn => {}
        }

        // Every frame the node is handed an excitation on.
        let arrival = rig.arrival.get() as u64;
        let mut delivered = vec![f + arrival];
        if feed == Feed::FanIn {
            delivered.push(f + gap + arrival);
        }
        let pattern_len = match &self.detect {
            Detect::Exact(p) => p.len() as u64,
            Detect::Threshold(_) => 1,
        };
        let lat = rig.latency.get() as u64;
        let last = *delivered.last().expect("one at least");
        let total = last + lat + pattern_len + 3 * max;
        let blocks = schedule.blocks(total);

        // Where each response may start: `delivered + latency`, within what
        // the node's resolution promises (doc 013 §6: `Frames(n)` within
        // `n - 1` frames either way, `Block` within the block it lands in).
        let expect: Vec<(u64, u64)> = delivered
            .iter()
            .map(|&d| {
                let tolerance = match (self.excite, rig.resolution) {
                    (Excite::Impulse { .. }, _) | (_, Resolution::Sample) => 0,
                    (_, Resolution::Frames(n)) => u64::from(n.max(1)) - 1,
                    (_, Resolution::Block) => {
                        let (bs, be) = block_of(&blocks, d);
                        be - bs - 1
                    }
                };
                (d + lat, tolerance)
            })
            .collect();

        let impulse_port = match self.excite {
            Excite::Impulse { amplitude, .. } => Some(amplitude),
            Excite::Event { .. } => None,
        };
        let mut out = Vec::with_capacity(total as usize);
        let mut done = 0u64;
        // `Some(generation before)` from the edit until the block after it
        // has proved the recompile installed.
        let mut pending_edit: Option<u32> = None;
        let mut edited = false;
        for &n in &blocks {
            // The edit lands on the first block after the excitation's.
            if edit != Edit::None && !edited && done > f {
                assert!(
                    done < expect[0].0,
                    "{ctx}: no block boundary while the excitation is in flight"
                );
                pending_edit = Some(rig.edit(edit, self));
                edited = true;
            }
            let input: Vec<f32> = (done..done + n as u64)
                .map(|i| match impulse_port {
                    Some(a) if i == f => a,
                    _ => 0.0,
                })
                .collect();
            let (transport, changes) = transport_for(start, done, n);
            let mut block = vec![vec![0.0f32; n]; 1];
            {
                let mut refs: Vec<&mut [f32]> = block.iter_mut().map(|c| &mut c[..]).collect();
                let ins: Vec<&[f32]> = if rig.has_input {
                    vec![&input[..]]
                } else {
                    vec![]
                };
                rig.exec
                    .process_with_changes(n, &transport, &changes, &ins, &mut refs);
            }
            if let Some(before) = pending_edit.take() {
                rig.assert_installed(edit, before, &ctx);
            }
            rig.ed.collect();
            out.extend_from_slice(&block[0]);
            done += n as u64;
        }
        assert!(
            edit == Edit::None || edited,
            "{ctx}: the edit never happened"
        );
        if matches!(feed, Feed::AtFrame | Feed::AtBeat) {
            assert_eq!(rig.exec.late_commands(), 0, "{ctx}: the command was late");
            assert_eq!(
                rig.ed.commands_outstanding(),
                0,
                "{ctx}: the command never landed"
            );
        }
        self.assert_responses(&out, &expect, &ctx);
    }

    /// Check `out` against the expected responses, each `(exact frame,
    /// tolerance)` in time order.
    ///
    /// A [`Detect::Threshold`] row is held on its first response's start:
    /// what follows (a limiter's release, an HRIR) is not known sample for
    /// sample. A [`Detect::Exact`] row is held on the **whole render**: exact
    /// silence except each expected response, so a duplicate, a missing
    /// second response or a stray sample anywhere fails.
    fn assert_responses(&self, out: &[f32], expect: &[(u64, u64)], ctx: &str) {
        let place = |e: u64, tol: u64| {
            if tol == 0 {
                format!("{e}")
            } else {
                format!("{e} (within {tol})")
            }
        };
        let check_start = |got: u64, (e, tol): (u64, u64)| {
            assert!(
                got.abs_diff(e) <= tol,
                "{ctx}: the response starts at frame {got}; the contract puts it at {}",
                place(e, tol)
            );
        };
        match &self.detect {
            Detect::Threshold(th) => {
                let first = out.iter().position(|x| x.abs() > *th);
                let Some(first) = first else {
                    panic!(
                        "{ctx}: no response at all; expected one at frame {}",
                        place(expect[0].0, expect[0].1)
                    );
                };
                check_start(first as u64, expect[0]);
            }
            Detect::Exact(p) => {
                let mut cursor = 0usize;
                for &(e, tol) in expect {
                    let Some(at) = out[cursor..].iter().position(|&x| x != 0.0) else {
                        panic!(
                            "{ctx}: a response is missing; expected one at frame {}",
                            place(e, tol)
                        );
                    };
                    let at = cursor + at;
                    check_start(at as u64, (e, tol));
                    let got = &out[at..(at + p.len()).min(out.len())];
                    assert_eq!(
                        got,
                        &p[..],
                        "{ctx}: the response at frame {at} is not the expected one"
                    );
                    cursor = at + p.len();
                }
                if let Some(extra) = out[cursor..].iter().position(|&x| x != 0.0) {
                    panic!(
                        "{ctx}: an extra response at frame {} (value {}); the contract \
                         expects silence after the last one",
                        cursor + extra,
                        out[cursor + extra]
                    );
                }
            }
        }
    }

    fn event_port(&self) -> u16 {
        match self.excite {
            Excite::Event { port, .. } | Excite::Impulse { port, .. } => port,
        }
    }

    fn event_kind(&self) -> EventKind {
        match self.excite {
            Excite::Event { kind, .. } => kind,
            Excite::Impulse { .. } => unreachable!("checked in `check`"),
        }
    }

    /// How much later the fan-in's first source sends its event: far
    /// enough that the two responses cannot overlap or swap even with each
    /// at the far edge of its resolution's tolerance, and near enough to
    /// share a block (see `run`), so a merge that is not by offset hands the
    /// node the two out of order.
    fn fan_in_gap(&self, resolution: Resolution) -> u64 {
        let p = match &self.detect {
            Detect::Exact(p) => p.len() as u64,
            Detect::Threshold(_) => 1,
        };
        let tolerance = match resolution {
            Resolution::Sample => 0,
            Resolution::Frames(n) => u64::from(n.max(1)) - 1,
            Resolution::Block => MAX_BLOCK as u64 - 1,
        };
        p + 2 * tolerance + 1
    }

    fn build(&self, topo: Topo, feed: Feed, f: u64, gap: u64) -> Rig {
        let node = (self.make)();
        let shape = node.shape();
        let audio = matches!(self.excite, Excite::Impulse { .. });
        let port = self.event_port();
        if audio {
            assert!(
                port < shape.audio_in.count(),
                "{}: audio input {port} out of range",
                self.name
            );
        } else {
            assert!(
                port < shape.event_in,
                "{}: event input {port} out of range",
                self.name
            );
        }
        let ins = if audio {
            ChannelLayout::MONO
        } else {
            ChannelLayout::EMPTY
        };
        let mut g = GraphBuilder::new(ins, ChannelLayout::MONO);
        let n = g.add(node);
        let mut feeder = None;
        let p = port as usize;
        if audio {
            match topo {
                Topo::Direct => {
                    g.connect_input(0, n, p);
                }
                Topo::Pdc => {
                    let lat = g.add(Latent::new(Latency::new(Samples(SIBLING_LATENCY))));
                    let sum = g.add(Sum);
                    g.connect(lat, 0, sum, 0).connect_input(0, sum, 1);
                    g.connect(sum, 0, n, p);
                    feeder = Some((sum, FeederKind::Sum));
                }
            }
        } else {
            let kind = self.event_kind();
            if topo == Topo::Pdc {
                let lat = g.add(Latent::new(Latency::new(Samples(SIBLING_LATENCY))));
                g.event_connect(lat, 0, n, p);
            }
            match feed {
                Feed::Source => {
                    let e = g.add(Emitter::new(Frame(f), kind));
                    g.event_connect(e, 0, n, p);
                    feeder = Some((e, FeederKind::Emitter(Frame(f), kind)));
                }
                Feed::FanIn => {
                    let late = g.add(Emitter::new(Frame(f + gap), kind));
                    let e = g.add(Emitter::new(Frame(f), kind));
                    g.event_connect(late, 0, n, p).event_connect(e, 0, n, p);
                }
                Feed::AtFrame | Feed::AtBeat => {}
            }
        }
        g.connect_output(n, self.output as usize, 0);
        let (ed, exec) = g
            .build(Prepare::new(SAMPLE_RATE, Samples(MAX_BLOCK)))
            .unwrap_or_else(|e| panic!("{}: the contract graph does not build: {e}", self.name));
        let unit = exec
            .plan()
            .and_then(|p| p.unit(n))
            .expect("the node is in the plan");
        let (arrival, prepared) = (unit.arrival.samples(), unit.shape);
        Rig {
            ed,
            exec,
            node: n,
            feeder,
            arrival,
            latency: prepared.latency.samples(),
            resolution: prepared.event_resolution,
            has_input: audio,
        }
    }
}

const BOTH: &[Topo] = &[Topo::Direct, Topo::Pdc];

/// What feeds the node, to re-insert for [`Path::RecompileUpstreamGeneration`].
#[derive(Clone, Copy)]
enum FeederKind {
    Sum,
    Emitter(Frame, EventKind),
}

struct Rig {
    ed: Editor,
    exec: Executor,
    node: NodeKey,
    feeder: Option<(NodeKey, FeederKind)>,
    arrival: Samples,
    latency: Samples,
    resolution: Resolution,
    has_input: bool,
}

impl Rig {
    /// Make the edit and commit it. Returns the generation the node it
    /// proves itself on had before: the feeder's, or 0 for the unrelated
    /// insert (which had none).
    fn edit(&mut self, edit: Edit, row: &Row) -> u32 {
        let before = match (edit, self.feeder) {
            (Edit::Regenerate, Some((key, _))) => self.ed.spec().generation(key),
            _ => 0,
        };
        match edit {
            Edit::None => {}
            Edit::Unrelated => {
                // A node on no path: it touches neither the node under test
                // nor anything feeding it, so the recompile must not either.
                self.ed.insert(
                    UNRELATED,
                    "unrelated",
                    Latent::new(Latency::new(Samples(7))),
                );
            }
            Edit::Regenerate => {
                let (key, kind) = self
                    .feeder
                    .unwrap_or_else(|| panic!("{}: no upstream node to regenerate", row.name));
                match kind {
                    FeederKind::Sum => {
                        self.ed.insert(key, "sum", Sum);
                    }
                    // Armed for a frame already past, so it sends nothing
                    // more: the one event in flight is the old unit's.
                    FeederKind::Emitter(at, kind) => {
                        self.ed.insert(key, "emitter", Emitter::new(at, kind));
                    }
                }
            }
        }
        self.ed
            .commit()
            .unwrap_or_else(|e| panic!("{}: the mid-stream commit failed: {e}", row.name));
        before
    }

    /// After the first block past an edit: the executor runs the edited
    /// plan, so a recompile path cannot pass having recompiled nothing.
    fn assert_installed(&self, edit: Edit, before: u32, ctx: &str) {
        let plan = self.exec.plan().expect("a plan");
        match edit {
            Edit::None => {}
            Edit::Unrelated => assert!(
                plan.unit(UNRELATED).is_some(),
                "{ctx}: the unrelated insert is not in the running plan"
            ),
            Edit::Regenerate => {
                let (key, _) = self.feeder.expect("checked at the edit");
                let now = plan.unit(key).expect("the feeder is in the plan").gen;
                assert!(
                    now > before,
                    "{ctx}: the feeder is still generation {now} in the running plan"
                );
            }
        }
    }
}

/// A block schedule.
#[derive(Clone, Copy, Debug)]
enum Schedule {
    Fixed(usize),
    Random,
}

impl Schedule {
    /// Blocks covering at least `total` frames.
    fn blocks(self, total: u64) -> Vec<usize> {
        let mut out = Vec::new();
        let mut done = 0u64;
        // xorshift64: no dependency, and the same schedule every run.
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        while done < total {
            let n = match self {
                Schedule::Fixed(n) => n,
                Schedule::Random => {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    1 + (s % MAX_BLOCK as u64) as usize
                }
            };
            out.push(n);
            done += n as u64;
        }
        out
    }
}

/// The `[start, end)` frames of the block holding `frame`.
fn block_of(blocks: &[usize], frame: u64) -> (u64, u64) {
    let mut bs = 0u64;
    for &n in blocks {
        let be = bs + n as u64;
        if frame < be {
            return (bs, be);
        }
        bs = be;
    }
    panic!("frame {frame} is past the render");
}

/// The transport for the block `[bs, bs + n)`: stopped until `start` (when
/// there is one), then rolling from beat 0 at 120 BPM — a timed start, as a
/// change inside its block when it falls inside one.
fn transport_for(start: Option<u64>, bs: u64, n: usize) -> (Transport, TransportChanges) {
    let stopped = Transport::default();
    let rolling = |at: u64| Transport {
        playing: true,
        tempo: Bpm(120.0),
        beat: Beat((at - start.unwrap_or(0)) as f64 / FRAMES_PER_BEAT as f64),
        looping: None,
        origin: None,
    };
    let mut changes = TransportChanges::NONE;
    let Some(s) = start else {
        return (stopped, changes);
    };
    if bs >= s {
        return (rolling(bs), changes);
    }
    if s < bs + n as u64 {
        let at = crate::time::Offset::new((s - bs) as usize, Samples(n)).expect("inside");
        changes.push(at, rolling(s)).expect("one change");
    }
    (stopped, changes)
}

// ---- the harness's own nodes -------------------------------------------------

/// An event-driven impulse: every event on its one event input adds one
/// sample of `1.0` to its one audio output, [`latency`](Self::new) frames
/// after the frame the event takes effect on. *Adds*: two events on one
/// frame are one sample of `2.0`, so a duplicated delivery is visible.
///
/// Written against [`Io::sub_blocks`](crate::Io::sub_blocks), so at
/// [`Resolution::Sample`] (the default) it is sample-accurate by
/// construction: this is the native row, and the source the engine-level
/// rows play. [`with_resolution`](Self::with_resolution) makes it as coarse
/// as it declares:
///
/// - `Frames(n)`: an event takes effect at the start of the node's **own**
///   next `n`-frame chunk (a grid of its own, offset from the graph's
///   blocks, as an engine that renders in fixed chunks would be) — up to
///   `n - 1` frames late;
/// - `Block`: at its block's first frame.
#[derive(Clone, Debug)]
pub struct Pulse {
    latency: Latency,
    resolution: Resolution,
    /// Absolute frames of impulses still to come; a fixed array, so the
    /// audio thread never allocates.
    pending: [u64; PULSE_PENDING],
    len: usize,
}

/// How many impulses a [`Pulse`] holds in flight.
const PULSE_PENDING: usize = 16;

/// Where a `Frames(n)` [`Pulse`]'s own chunk grid starts: frame 3, so it
/// lines up with no block boundary a schedule of the usual sizes makes.
const PULSE_GRID_PHASE: u64 = 3;

impl Pulse {
    /// A pulse delayed by `latency`, declared as its [`Shape::latency`].
    pub fn new(latency: Latency) -> Self {
        Self {
            latency,
            resolution: Resolution::Sample,
            pending: [0; PULSE_PENDING],
            len: 0,
        }
    }

    /// This pulse, honouring offsets only as finely as `resolution` (see
    /// the type docs), and declaring so.
    #[must_use]
    pub fn with_resolution(mut self, resolution: Resolution) -> Self {
        self.resolution = resolution;
        self
    }
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_latency(self.latency)
            .with_tail(tutti_types::Tail::Unknown)
            .with_event_resolution(self.resolution)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block = cx.env.frame.get();
        for (range, events) in io.sub_blocks(0) {
            let t = block + range.start as u64;
            let at = match self.resolution {
                Resolution::Sample => t,
                Resolution::Frames(n) => {
                    let n = u64::from(n.max(1));
                    t + (n - (t + PULSE_GRID_PHASE) % n) % n
                }
                Resolution::Block => block,
            };
            for _ in events {
                if self.len < PULSE_PENDING {
                    self.pending[self.len] = at + self.latency.samples().get() as u64;
                    self.len += 1;
                }
            }
        }
        let frames = io.frames() as u64;
        let out = io.output(0);
        out.fill(0.0);
        let mut i = 0;
        while i < self.len {
            let t = self.pending[i];
            if t < block + frames {
                // An impulse already past (never, for a node called every
                // block) is dropped rather than written late.
                if t >= block {
                    out[(t - block) as usize] += 1.0;
                }
                self.len -= 1;
                self.pending[i] = self.pending[self.len];
            } else {
                i += 1;
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.len = 0;
    }
}

/// A native audio delay that declares its delay as processing latency — a
/// lookahead with nothing to look ahead for. The native audio-impulse row.
#[derive(Clone, Debug)]
pub struct Lookahead {
    latency: Latency,
    ring: Vec<f32>,
    pos: usize,
}

impl Lookahead {
    /// A delay of `latency`, declared as such.
    pub fn new(latency: Latency) -> Self {
        Self {
            latency,
            ring: vec![0.0; latency.samples().get()],
            pos: 0,
        }
    }
}

impl Node for Lookahead {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(self.latency)
            .with_tail(tutti_types::Tail::Finite(self.latency.samples()))
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = self.ring.len();
        if n == 0 {
            return Status::Bypass;
        }
        for i in 0..io.frames() {
            let x = io.input(0)[i];
            io.output(0)[i] = std::mem::replace(&mut self.ring[self.pos], x);
            self.pos = (self.pos + 1) % n;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }
}

/// A latent sibling: declares `latency`, outputs silence on one audio
/// output and nothing on one event output. Wired beside a path, it raises
/// the arrival of whatever it merges into without adding a signal.
#[derive(Clone, Copy, Debug)]
pub struct Latent {
    latency: Latency,
}

impl Latent {
    /// A silent node declaring `latency`.
    pub fn new(latency: Latency) -> Self {
        Self { latency }
    }
}

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(0, 1)
            .with_latency(self.latency)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }

    fn reset(&mut self) {}
}

/// Sends one event, `kind`, on its one event output at absolute frame `at`.
#[derive(Clone, Copy, Debug)]
pub struct Emitter {
    at: Frame,
    kind: EventKind,
}

impl Emitter {
    /// An emitter armed for `at`.
    pub fn new(at: Frame, kind: EventKind) -> Self {
        Self { at, kind }
    }
}

impl Node for Emitter {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(offset) = cx.env.offset_of(self.at) {
            io.event_out(0)
                .push(Event {
                    offset,
                    kind: self.kind,
                })
                .expect("one event fits");
        }
        Status::Silent
    }

    fn reset(&mut self) {}
}

/// Two audio inputs, summed into one output.
#[derive(Clone, Copy, Debug)]
pub struct Sum;

impl Node for Sum {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let s = io.input(0)[i] + io.input(1)[i];
            io.output(0)[i] = s;
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// One `#[test]` per [`Path`] for a row, in a module named for it, so each
/// path is its own case and fails on its own.
///
/// `event` rows run every path; `audio` rows (an [`Excite::Impulse`]) run
/// every path but the three that need an event
/// ([`Path::needs_events`]). The row expression is evaluated once per test,
/// in a child module of the caller (so it can name the caller's items).
///
/// ```
/// use tutti_graph::contract::{Detect, Excite, Lookahead, Pulse, Row};
/// use tutti_graph::{EventKind, Ump};
/// use tutti_types::{Latency, Samples};
///
/// fn pulse_row() -> Row {
///     Row::new(
///         "Pulse",
///         || Box::new(Pulse::new(Latency::ZERO)),
///         Excite::Event { port: 0, kind: EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0])) },
///         Detect::Exact(vec![1.0]),
///     )
/// }
///
/// fn lookahead_row() -> Row {
///     Row::new(
///         "Lookahead",
///         || Box::new(Lookahead::new(Latency::new(Samples(45)))),
///         Excite::Impulse { port: 0, amplitude: 1.0 },
///         Detect::Exact(vec![1.0]),
///     )
/// }
///
/// // `pulse::direct`, `pulse::behind_pdc`, … `pulse::scheduled_beat`.
/// tutti_graph::contract_tests!(event pulse => pulse_row());
/// // `lookahead::direct`, … `lookahead::blocks_random`: no event paths.
/// tutti_graph::contract_tests!(audio lookahead => lookahead_row());
/// # fn main() {}
/// ```
#[macro_export]
macro_rules! contract_tests {
    (event $name:ident => $row:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            $crate::contract_tests!(@path $row; direct Direct, behind_pdc BehindPdc,
                recompile_unrelated RecompileUnrelated,
                recompile_upstream_generation RecompileUpstreamGeneration,
                blocks_1 Blocks1, blocks_63 Blocks63, blocks_64 Blocks64, blocks_65 Blocks65,
                blocks_max BlocksMax, blocks_random BlocksRandom,
                event_fan_in EventFanIn, scheduled_frame ScheduledFrame,
                scheduled_beat ScheduledBeat);
        }
    };
    (audio $name:ident => $row:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            $crate::contract_tests!(@path $row; direct Direct, behind_pdc BehindPdc,
                recompile_unrelated RecompileUnrelated,
                recompile_upstream_generation RecompileUpstreamGeneration,
                blocks_1 Blocks1, blocks_63 Blocks63, blocks_64 Blocks64, blocks_65 Blocks65,
                blocks_max BlocksMax, blocks_random BlocksRandom);
        }
    };
    (@path $row:expr; $($test:ident $path:ident),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                $crate::contract::Row::check(&$row, $crate::contract::Path::$path);
            }
        )*
    };
}
