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

use tutti_graph::contract::{Detect, Excite, Lookahead, Path, Pulse, Row, OFFSETS};
use tutti_graph::{
    contract_tests, Cx, EventKind, Io, Node, Prepare, Resolution, Shape, Status, Ump,
};
use tutti_types::{ChannelLayout, Latency, Samples, Tail};

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

/// Declares `Frames(8)` — a SoundFont's resolution — and honours exactly
/// that: the contract puts its response on the first frame of the 8-frame
/// chunk the event falls in.
fn chunked_pulse_row() -> Row {
    Row::new(
        "Pulse at Frames(8)",
        || Box::new(Pulse::new(Latency::ZERO).with_resolution(Resolution::Frames(8))),
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
contract_tests!(audio lookahead => lookahead_row());

// ---- the harness can fail ----------------------------------------------------

/// Declares `declared` frames of latency and delivers its impulse
/// `actual` frames after each event — the D1–D3 shape when the two differ.
/// With `ignore_offsets` it also renders every event at its block's first
/// frame, while still declaring `Resolution::Sample`.
struct Liar {
    declared: usize,
    actual: usize,
    ignore_offsets: bool,
    pending: Option<u64>,
}

impl Liar {
    fn boxed(declared: usize, actual: usize, ignore_offsets: bool) -> Box<dyn Node> {
        Box::new(Liar {
            declared,
            actual,
            ignore_offsets,
            pending: None,
        })
    }
}

impl Node for Liar {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_latency(Latency::new(Samples(self.declared)))
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let block = cx.env.frame.get();
        for e in io.events(0).iter() {
            let at = if self.ignore_offsets {
                0
            } else {
                e.offset.get() as u64
            };
            self.pending = Some(block + at + self.actual as u64);
        }
        let out = io.output(0);
        out.fill(0.0);
        if let Some(t) = self.pending {
            if (block..block + out.len() as u64).contains(&t) {
                out[(t - block) as usize] = 1.0;
                self.pending = None;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
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

/// A node one frame early against its declaration fails the direct path,
/// naming both frames; the honest one (declared = actual) passes it.
///
/// Mutation (run): in `Row::assert_response`, accept a response wherever it
/// starts (drop the window check) → both liars pass → fails.
#[test]
fn a_node_off_its_declared_latency_fails() {
    let row = |actual| {
        Row::new(
            "liar",
            move || Liar::boxed(5, actual, false),
            note(),
            Detect::Exact(vec![1.0]),
        )
    };
    row(5).check(Path::Direct);
    for actual in [4, 6] {
        let msg = failure(|| row(actual).check(Path::Direct))
            .unwrap_or_else(|| panic!("a node {actual} frames late against 5 declared passed"));
        assert!(msg.contains("the contract puts it at"), "{msg}");
    }
}

/// A node that declares `Sample` but applies every event at its block's
/// first frame fails the direct path at any non-zero offset.
///
/// Mutation (run): drop the window check in `Row::assert_response` → the
/// node passes → fails. (With only offset 0 in `OFFSETS` it would pass too;
/// the first assertion pins that the sweep has others.)
#[test]
fn a_node_ignoring_offsets_fails() {
    assert!(OFFSETS.iter().any(|&k| k != 0));
    let row = Row::new(
        "offset-ignorer",
        || Liar::boxed(0, 0, true),
        note(),
        Detect::Exact(vec![1.0]),
    );
    let msg = failure(|| row.check(Path::Direct)).expect("an offset-ignoring node passed");
    assert!(msg.contains("the contract puts it at"), "{msg}");
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
