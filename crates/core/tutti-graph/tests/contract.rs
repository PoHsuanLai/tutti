//! The sample-accuracy contract suite (doc 013 §6, "Proof"), for the
//! graph's own native nodes: an event at offset `k` produces output at frame
//! `k + arrival + latency`, exactly, on every path — direct, behind PDC,
//! through an event fan-in, across a recompile (an unrelated edit, and a
//! generation bump upstream), across ragged blocks (1, 63, 64, 65, `MaxBlock`,
//! random), and delivered by a scheduled `At::Frame` or `At::Beat` command.
//!
//! The harness is `tutti_graph::contract`; the mutation each path was seen
//! to fail under is recorded on its `Path` variant. The node crates carry
//! their own rows (`tutti-nodes`, `tutti-spatial`: the latency-bearing
//! `Legacy` units), and `tutti-core` the engine-level ones.
//!
//! The last tests here are the harness's own: rows that break the contract
//! on purpose, which it must fail — a harness that cannot fail is worse than
//! none.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use tutti_graph::contract::{
    Detect, Emitter, Excite, Lookahead, Path, Pulse, Row, MAX_BLOCK, OFFSETS, SAMPLE_RATE,
};
use tutti_graph::{
    contract_tests, Cx, EventKind, GraphBuilder, Io, Node, Prepare, Resolution, Shape, Status, Ump,
};
use tutti_types::{ChannelLayout, Frame, Latency, Samples, Tail};

/// A MIDI 1.0 note-on, middle C: the event every event row is excited by.
const NOTE_ON: EventKind = EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]));

fn note() -> Excite {
    Excite::Event {
        port: 0,
        kind: NOTE_ON,
    }
}

fn pulse_row() -> Row {
    Row::new(
        "Pulse",
        || Box::new(Pulse::new(Latency::ZERO)),
        note(),
        Detect::Exact(vec![1.0]),
    )
}

fn latent_pulse_row() -> Row {
    Row::new(
        "Pulse, 37 frames of latency",
        || Box::new(Pulse::new(Latency::new(Samples(37)))),
        note(),
        Detect::Exact(vec![1.0]),
    )
    .expect_latency(Samples(37))
}

/// Declares `Frames(8)` (a SoundFont's resolution) and quantizes on its own
/// 8-frame grid, offset from every block schedule: an event takes effect at
/// the start of its next chunk, up to 7 frames late. Doc 013 §6: `Frames(n)`
/// is honoured within `n - 1` frames either way, so it passes on every
/// path, ragged schedules included.
fn chunked_pulse_row() -> Row {
    Row::new(
        "Pulse at Frames(8), own grid",
        || Box::new(Pulse::new(Latency::ZERO).with_resolution(Resolution::Frames(8))),
        note(),
        Detect::Exact(vec![1.0]),
    )
}

/// Declares `Block` and takes every event at its block's first frame.
fn block_pulse_row() -> Row {
    Row::new(
        "Pulse at Block",
        || Box::new(Pulse::new(Latency::ZERO).with_resolution(Resolution::Block)),
        note(),
        Detect::Exact(vec![1.0]),
    )
}

fn lookahead_row() -> Row {
    Row::new(
        "Lookahead, 45 frames",
        || Box::new(Lookahead::new(Latency::new(Samples(45)))),
        Excite::Impulse {
            port: 0,
            amplitude: 0.5,
        },
        Detect::Exact(vec![0.5]),
    )
    .expect_latency(Samples(45))
}

contract_tests!(event pulse => pulse_row());
contract_tests!(event latent_pulse => latent_pulse_row());
contract_tests!(event chunked_pulse => chunked_pulse_row());
contract_tests!(event block_pulse => block_pulse_row());
contract_tests!(audio lookahead => lookahead_row());

// ---- the harness can fail ----------------------------------------------------

/// Where a [`Liar`] puts an event, before its `actual` delay.
#[derive(Clone, Copy)]
enum Timing {
    /// On the event's own frame.
    Exact,
    /// On its block's first frame.
    BlockStart,
    /// Rounded up to a multiple of `n` on the absolute frame grid (a node
    /// cannot write into a block already rendered, so a chunked node that
    /// is late rounds up).
    Grid(u64),
}

/// Declares `declared` frames of latency and `resolution`, and delivers
/// one impulse `actual` frames after where `timing` puts each event: the
/// D1–D3 shape when `actual` and `declared` differ, and a resolution lie
/// when `timing` is coarser than `resolution`.
struct Liar {
    declared: usize,
    actual: u64,
    resolution: Resolution,
    timing: Timing,
    pending: Vec<u64>,
}

impl Liar {
    fn boxed(
        declared: usize,
        actual: u64,
        resolution: Resolution,
        timing: Timing,
    ) -> Box<dyn Node> {
        Box::new(Liar {
            declared,
            actual,
            resolution,
            timing,
            pending: Vec::with_capacity(8),
        })
    }
}

impl Node for Liar {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_latency(Latency::new(Samples(self.declared)))
            .with_tail(Tail::Unknown)
            .with_event_resolution(self.resolution)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block = cx.env.frame.get();
        for e in io.events(0).iter() {
            let t = block + e.offset.get() as u64;
            let at = match self.timing {
                Timing::Exact => t,
                Timing::BlockStart => block,
                Timing::Grid(n) => t.div_ceil(n) * n,
            };
            self.pending.push(at + self.actual);
        }
        let out = io.output(0);
        out.fill(0.0);
        let end = block + out.len() as u64;
        self.pending.retain(|&t| {
            if (block..end).contains(&t) {
                out[(t - block) as usize] += 1.0;
            }
            t >= end
        });
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn liar_row(declared: usize, actual: u64, resolution: Resolution, timing: Timing) -> Row {
    Row::new(
        "liar",
        move || Liar::boxed(declared, actual, resolution, timing),
        note(),
        Detect::Exact(vec![1.0]),
    )
}

/// The panic message of `f`, or `None` when it passed.
fn failure(f: impl FnOnce()) -> Option<String> {
    let err = catch_unwind(AssertUnwindSafe(f)).err()?;
    Some(
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    )
}

/// `row` fails `path` with a message naming a mistimed response.
fn fails(row: &Row, path: Path, what: &str) {
    let msg = failure(|| row.check(path)).unwrap_or_else(|| panic!("{what} passed {path:?}"));
    assert!(
        msg.contains("the contract puts it at"),
        "{what}, {path:?}: {msg}"
    );
}

/// A node one frame off its declaration, either way, fails the direct path,
/// behind PDC and under the random schedule, naming both frames; the honest
/// one (declared = actual) passes all three.
///
/// Mutation (run): in `Row::assert_responses`, accept a response wherever
/// it starts (drop the start check) → the liars pass → fails.
#[test]
fn a_node_off_its_declared_latency_fails() {
    for path in [Path::Direct, Path::BehindPdc, Path::BlocksRandom] {
        liar_row(5, 5, Resolution::Sample, Timing::Exact).check(path);
        for actual in [4, 6] {
            fails(
                &liar_row(5, actual, Resolution::Sample, Timing::Exact),
                path,
                &format!("a node {actual} frames late against 5 declared"),
            );
        }
    }
}

/// A node that declares `Sample` but applies every event at its block's
/// first frame fails the direct path at any non-zero offset.
///
/// Mutation (run): drop the start check in `Row::assert_responses` → the
/// node passes → fails. (With only offset 0 in `OFFSETS` it would pass too;
/// the first assertion pins that the sweep has others.)
#[test]
fn a_node_ignoring_offsets_fails() {
    assert!(OFFSETS.iter().any(|&k| k != 0));
    fails(
        &liar_row(0, 0, Resolution::Sample, Timing::BlockStart),
        Path::Direct,
        "an offset-ignoring node",
    );
}

/// Resolution is held as declared (doc 013 §6): within `n - 1` frames of
/// the exact frame for `Frames(n)`, either way.
///
/// - Declaring `Sample` but quantizing to 8 frames fails.
/// - Declaring `Frames(8)` and landing 9 frames late fails; 7 late passes,
///   and so does quantizing to 8 (up to 7 late).
/// - A node finer than it declares (exact, declaring `Frames(8)` or
///   `Block`) passes.
///
/// Mutation (run): hold `Frames(n)` to the exact frame (tolerance 0 in
/// `Row::run`) → the 7-late and the quantizing nodes fail → this test and
/// every `chunked_pulse` path fail. Hold `Block` to
/// the exact frame → the `block_pulse` rows fail.
#[test]
fn resolution_is_held_as_declared() {
    for path in [Path::Direct, Path::BlocksRandom] {
        fails(
            &liar_row(0, 0, Resolution::Sample, Timing::Grid(8)),
            path,
            "a Sample node quantizing to 8",
        );
        fails(
            &liar_row(0, 9, Resolution::Frames(8), Timing::Exact),
            path,
            "a Frames(8) node 9 frames late",
        );
        liar_row(0, 7, Resolution::Frames(8), Timing::Exact).check(path);
        liar_row(0, 0, Resolution::Frames(8), Timing::Grid(8)).check(path);
        liar_row(0, 0, Resolution::Frames(8), Timing::Exact).check(path);
        liar_row(0, 0, Resolution::Block, Timing::Exact).check(path);
    }
}

/// A row pinned to a latency other than the node declares fails before it
/// renders, and an audio row asked for an event-only path refuses rather
/// than passing a path it did not run.
///
/// Mutation (run): skip the `expect_latency` comparison in `Row::run` → the
/// first half passes → fails. Drop the `needs_events` assert in
/// `Row::check` → the second half fails on another message → fails.
#[test]
fn a_row_cannot_pass_what_it_does_not_run() {
    let msg = failure(|| {
        latent_pulse_row()
            .expect_latency(Samples(36))
            .check(Path::Direct)
    })
    .expect("a mispinned latency passed");
    assert!(
        msg.contains("declares a latency other than the row pins"),
        "{msg}"
    );
    for path in [Path::EventFanIn, Path::ScheduledFrame, Path::ScheduledBeat] {
        let msg = failure(|| lookahead_row().check(path)).expect("an audio row ran an event path");
        assert!(msg.contains("needs an event excitation"), "{msg}");
    }
}

/// Records the first UMP word of every event it is handed, in order.
struct Tags(Arc<Mutex<Vec<(u64, u32)>>>);

impl Node for Tags {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut log = self.0.lock().unwrap();
        for e in io.events(0).iter() {
            if let EventKind::Midi(Ump(w)) = e.kind {
                log.push((cx.env.frame_at(e.offset).get(), w[0]));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Fan-in at **equal** offsets: source order decides, at every offset of a
/// block (doc 013 decision 5), three sources deep.
///
/// Mutation (run): in `merge_into`, break ties toward the later source
/// (`<` → `<=`) → the order reverses → fails.
#[test]
fn a_fan_in_tie_goes_by_source_order() {
    for k in OFFSETS {
        let at = Frame(512 + k as u64);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let sink = g.add(Tags(Arc::clone(&log)));
        for tag in [3u32, 1, 2] {
            let e = g.add(Emitter::new(at, EventKind::Midi(Ump([tag, 0, 0, 0]))));
            g.event_connect(e, 0, sink, 0);
        }
        g.connect_output(sink, 0, 0);
        let mut r = g
            .renderer(Prepare::new(SAMPLE_RATE, Samples(MAX_BLOCK)))
            .expect("builds");
        r.render(1_024);
        assert_eq!(
            *log.lock().unwrap(),
            vec![(at.get(), 3), (at.get(), 1), (at.get(), 2)],
            "offset {k}: the order the sources were connected in"
        );
    }
}
