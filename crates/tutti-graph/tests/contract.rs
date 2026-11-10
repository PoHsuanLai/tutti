//! The sample-accuracy contract suite, for the
//! graph's own nodes: an event at offset `k` produces output at frame
//! `k + arrival + latency`, exactly, on every path — direct, behind PDC,
//! through an event fan-in, across a recompile (an unrelated edit, and a
//! generation bump upstream), across ragged blocks (1, 63, 64, 65, `MaxBlock`,
//! random), and delivered by a scheduled `At::Frame` or `At::Beat` command.
//!
//! The harness is `tutti_graph::contract`; the mutation each path was seen
//! to fail under is recorded on its `Path` variant. The node crates carry
//! their own rows (`tutti-nodes`, `tutti-spatial`: the latency-bearing
//! nodes), and `tutti-engine` the engine-level ones.
//!
//! The last tests here are the harness's own: rows that break the contract
//! on purpose, which it must fail — a harness that cannot fail is worse than
//! none.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tutti_graph::contract::{
    Detect, Emitter, Excite, Lookahead, MAX_BLOCK, OFFSETS, ParamEcho, Path, Pulse, Row,
    SAMPLE_RATE,
};
use tutti_graph::{
    Cx, EventKind, GraphBuilder, Io, Node, Prepare, Resolution, Shape, Status, Unforkable,
    contract_tests,
};
use tutti_types::{ChannelLayout, Frame, Latency, Samples, Tail, UnitParam};
use tutti_ump::Ump;

/// A MIDI 1.0 volume controller (CC 7): the event every event row is excited
/// by. It is not a held key, so a recompile that replaces the feeder closes
/// nothing at the sink.
///
/// Mutation (run): a note-on here -> the four `recompile_upstream_generation`
/// rows fail, the sink hearing the close the replaced feeder owes.
const PULSE: EventKind = EventKind::Midi(Ump([0x20b0_0764, 0, 0, 0]));

fn pulse() -> Excite {
    Excite::Event {
        port: 0,
        kind: PULSE,
    }
}

fn pulse_row() -> Row {
    Row::new(
        "Pulse",
        || Box::new(Pulse::new(Latency::ZERO)),
        pulse(),
        Detect::Exact(vec![1.0]),
    )
}

fn latent_pulse_row() -> Row {
    Row::new(
        "Pulse, 37 frames of latency",
        || Box::new(Pulse::new(Latency::new(Samples(37)))),
        pulse(),
        Detect::Exact(vec![1.0]),
    )
    .with_expected_latency(Samples(37))
}

/// Declares `Frames(8)` (a SoundFont's resolution) and quantizes on its own
/// 8-frame grid, offset from every block schedule: an event takes effect at
/// the start of its next chunk, up to 7 frames late. `Frames(n)`
/// is honoured within `n - 1` frames either way, so it passes on every
/// path, ragged schedules included.
fn chunked_pulse_row() -> Row {
    Row::new(
        "Pulse at Frames(8), own grid",
        || Box::new(Pulse::new(Latency::ZERO).with_resolution(Resolution::Frames(8))),
        pulse(),
        Detect::Exact(vec![1.0]),
    )
}

/// Declares `Block` and takes every event at its block's first frame.
fn block_pulse_row() -> Row {
    Row::new(
        "Pulse at Block",
        || Box::new(Pulse::new(Latency::ZERO).with_resolution(Resolution::Block)),
        pulse(),
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
    .with_expected_latency(Samples(45))
}

/// Compiler-owned param modulation: a step in a modulator
/// at frame `F` reaches the node's param at `F + arrival`, exactly — on the
/// PDC path through the compiler's delay of the early param source.
///
/// Mutations (re-run with the source an event pulse): in
/// `compile`, never lower a param source's delay (no
/// `DelayKey::ParamEvent`) → the PDC paths fail. In `render_ramps`, take
/// each event one frame late → every path fails.
fn param_row() -> Row {
    Row::new(
        "ParamEcho on Cutoff",
        || Box::new(ParamEcho::new(UnitParam::Cutoff)),
        Excite::Param {
            param: UnitParam::Cutoff.into(),
            amplitude: 0.5,
        },
        Detect::Exact(vec![0.5]),
    )
}

contract_tests!(audio param_echo => param_row());
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
/// latency-lie shape when `actual` and `declared` differ, and a resolution lie
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
        pulse(),
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

/// A row's lead is held exactly: a node whose response starts one frame
/// after its excitation passes with a lead of one and fails with none or
/// two, on the direct path, behind PDC and under the random schedule. (What
/// lets an instrument whose note starts from a zero sample have a row.)
///
/// Mutation (run): `assert_responses` ignoring the lead (`let e = e;`) →
/// the row with a lead of one fails ("puts it at 5", the response at 6) →
/// fails.
#[test]
fn a_rows_lead_is_held_exactly() {
    for path in [Path::Direct, Path::BehindPdc, Path::BlocksRandom] {
        // Declares 5 frames and responds at 6: one frame of lead.
        liar_row(5, 6, Resolution::Sample, Timing::Exact)
            .with_lead(Samples(1))
            .check(path);
        for lead in [0, 2] {
            fails(
                &liar_row(5, 6, Resolution::Sample, Timing::Exact).with_lead(Samples(lead)),
                path,
                &format!("a one-frame lead checked as {lead}"),
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

/// Resolution is held as declared: within `n - 1` frames of
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
/// Mutation (run): skip the `with_expected_latency` comparison in `Row::run` → the
/// first half passes → fails. Drop the `needs_events` assert in
/// `Row::check` → the second half fails on another message → fails.
#[test]
fn a_row_cannot_pass_what_it_does_not_run() {
    let msg = failure(|| {
        latent_pulse_row()
            .with_expected_latency(Samples(36))
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
/// block, three sources deep.
///
/// Mutation (run): in `merge_into`, break ties toward the later source
/// (`<` → `<=`) → the order reverses → fails.
#[test]
fn a_fan_in_tie_goes_by_source_order() {
    for k in OFFSETS {
        let at = Frame(512 + k as u64);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let sink = g.add(Unforkable(Tags(Arc::clone(&log))));
        for tag in [3u32, 1, 2] {
            let e = g.add(Unforkable(Emitter::new(
                at,
                EventKind::Midi(Ump([tag, 0, 0, 0])),
            )));
            g.connect_events(e, 0, sink, 0);
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

/// Writes, per frame, the beat of the transport it reads from its `Env`
/// (`Env::transport_at`): a node that follows the transport, for the
/// harness entry points that hand it one.
#[derive(Clone)]
struct BeatEcho;

impl Node for BeatEcho {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for o in cx.env.offsets() {
            io.output(0)[o.index()] = cx.env.transport_at(o).beat().get() as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A rolling transport at beat 2, 120 BPM at 48 kHz: 24 000 frames a beat.
fn rolling_at_two() -> tutti_graph::Transport {
    tutti_graph::Transport::new(
        true,
        tutti_types::Bpm::of(120.0),
        tutti_types::Beat(2.0),
        None,
    )
}

/// `drive_input`, `Direct::block_in` and `BlockRig::block_in` hand the node
/// the transport they are given (and `drive_input` its changes), where
/// `Direct::block` and `BlockRig::block` hand it a stopped one at beat 0.
///
/// Mutation (run): `drive_input` building its `Cx` over a stopped `Env` of its
/// own → frame 0 reads 0, not 2 → fails. Mutation (run): `Direct::block_in`
/// ignoring its `env` → fails. Mutation (run): `BlockRig::block_in` passing
/// `Transport::default()` to the executor → fails.
#[test]
fn the_transport_harnesses_hand_the_node_their_transport() {
    use tutti_graph::contract::{BlockRig, Direct, drive_input, prepared};
    use tutti_graph::{Env, Offset, TransportChanges};
    use tutti_types::{Beat, Bpm, SampleRate};
    let rate = SampleRate(48_000.0);
    let per_frame = 1.0 / 24_000.0;

    let mut node = prepared(BeatEcho, rate, 4);

    // A seek to beat 10 on frame 2, inside the block.
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::new(2, Samples(4)).unwrap(),
            tutti_graph::Transport::new(true, Bpm::of(120.0), Beat(10.0), None),
        )
        .unwrap();
    let env = Env {
        frame: Frame(0),
        sample_rate: rate,
        block_len: Samples(4),
        transport: rolling_at_two(),
        changes,
        play: tutti_graph::PlayClock::ZERO,
    };
    let out = drive_input(&mut node, &env, &[], &[], &[]).audio;
    let want = [2.0, 2.0 + per_frame, 10.0, 10.0 + per_frame];
    for (got, want) in out[0].iter().zip(want) {
        assert_eq!(*got, want as f32);
    }

    let mut direct = Direct::new(BeatEcho, rate, 4);
    let env = Env {
        changes: TransportChanges::NONE,
        ..env
    };
    direct.block_in(&env);
    assert_eq!(direct.output(0)[0], 2.0);
    direct.block();
    assert_eq!(direct.output(0)[0], 0.0);
    // `outputs_mut` is the buffer the node writes and `output` reads, so a
    // frame the node leaves unwritten shows what it was filled with.
    direct.outputs_mut()[0].fill(7.0);
    assert_eq!(direct.output(0)[3], 7.0);
    direct.block_in(&env);
    assert_eq!(direct.output(0)[3], (2.0 + 3.0 * per_frame) as f32);

    let (mut rig, ()) = BlockRig::new(Unforkable(BeatEcho), rate, 4);
    rig.block_in(&rolling_at_two());
    assert_eq!(rig.output(0)[0], 2.0);
    assert_eq!(rig.output(0)[1], (2.0 + per_frame) as f32);
    rig.block();
    assert_eq!(rig.output(0)[0], 0.0);
}

/// The tail contract: a node that reports its ring (or less than its ceiling
/// and no less than its ring) passes, mono and stereo, with and without
/// latency; one that reports too short is caught, and so is a ceiling below
/// the ring.
///
/// Mutation (run): in `assert_honest_tail`, `silent_from = ... + 1 + 100000`
/// (check far too late) → the short-report and low-ceiling cases do not
/// panic → fails. A `Ringer` reporting half its ring (150 of 300) panics with
/// "the reported tail is too short".
#[test]
fn the_tail_contract_catches_a_report_that_is_too_short() {
    use common::Ringer;
    use tutti_graph::contract::{assert_honest_tail, prepared};
    let rate = tutti_graph::contract::SAMPLE_RATE;
    let ceiling = Tail::Finite(Samples(1000));
    let check = |r: Ringer| {
        let mut node = prepared(r, rate, 64);
        catch_unwind(AssertUnwindSafe(|| assert_honest_tail(&mut node, rate))).is_ok()
    };
    assert!(check(Ringer::new(ceiling, 300, 300)));
    assert!(check(Ringer::new(ceiling, 300, 400)), "long errs safe");
    assert!(
        check(Ringer::new(ceiling, 300, 299)),
        "exact: sound through frame T"
    );
    assert!(check(Ringer::new(ceiling, 300, 300).wide(2)));
    assert!(check(
        Ringer::new(ceiling, 300, 300).with_latency(Samples(37))
    ));
    assert!(check(Ringer::new(
        Tail::Unbounded,
        300,
        common::REPORT_UNBOUNDED
    )));
    assert!(
        !check(Ringer::new(ceiling, 300, 150)),
        "reports half its ring"
    );
    assert!(!check(Ringer::new(ceiling, 300, 298)), "one frame short");
    assert!(
        !check(Ringer::new(
            Tail::Finite(Samples(100)),
            300,
            common::NO_REPORT
        )),
        "a ceiling below the ring"
    );
}
