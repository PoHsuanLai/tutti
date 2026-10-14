//! A node behind a latent path reads the transport its input was rendered
//! at: `Cx::transport_at(offset)` is the transport `Cx::arrival` frames
//! earlier, through block boundaries, seeks, transport changes inside a
//! block, loop wraps, stops and ragged blocks, in the executor and the
//! reference alike.
//!
//! The truth is the blocks themselves: the transport at timeline frame `F`
//! is what `Env::transport_at` said for `F` in the block that held it. The
//! one exception is a frame older than every piece the executor's ring
//! keeps (before the first block, after a recompile raised the reach past
//! what the old ring kept, and after a sample-rate change): the oldest piece
//! kept, unmoved. Where that boundary falls is modelled here from the
//! ring's documented rules: before a block, the front piece goes while the
//! next one covers the frame `reach` before the block; at most
//! `reach + 1 + MAX_TRANSPORT_CHANGES` pieces, the newest kept; cleared on a
//! rate change.

mod common;

use std::collections::{BTreeMap, VecDeque};

use common::prepare;
use tutti_graph::{
    Cx, Editor, Env, Executor, Io, LoopRange, Node, Offset, Prepare, Reference, Shape, Status,
    Transport, TransportChanges, Unforkable, MAX_TRANSPORT_CHANGES,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, Frame, Latency, NodeKey, SampleRate, Samples};

const MAX: usize = 64;
const LATENT: NodeKey = NodeKey(1);
const PROBE: NodeKey = NodeKey(2);
/// Frames per beat at 120 BPM and the test rate (48 kHz).
const FPB: f64 = 24_000.0;

/// A source declaring `lat` frames of latency: all it does is make the probe
/// after it arrive that late.
#[derive(Clone)]
struct Latent(usize);

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(self.0)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Writes, per frame, the transport its input was rendered at
/// (`Cx::transport_at`): the beat on output 0, and the tempo plus flags
/// (1000 playing, 2000 recording, 4000 looping) on output 1. Output 2 is
/// its arrival, so a test can tell the compile moved it.
#[derive(Clone)]
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::from_count(3))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (i, k) in cx.env.offsets().enumerate() {
            let [beat, rest] = encode(&cx.transport_at(k));
            io.output(0)[i] = beat;
            io.output(1)[i] = rest;
            io.output(2)[i] = cx.arrival.samples().get() as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn encode(t: &Transport) -> [f32; 2] {
    let flags = 1000.0 * f32::from(u8::from(t.playing))
        + 2000.0 * f32::from(u8::from(t.recording()))
        + 4000.0 * f32::from(u8::from(t.looping.is_some()));
    [t.beat().get() as f32, t.tempo.get() as f32 + flags]
}

fn topology(ed: &mut Editor) {
    let t = &mut ed.spec_mut().topology;
    t.edges.insert(
        InPort {
            node: PROBE,
            port: 0,
        },
        Edge::Direct(Source::Node(OutPort {
            node: LATENT,
            port: 0,
        })),
    );
    t.outputs = (0..3)
        .map(|port| Source::Node(OutPort { node: PROBE, port }))
        .collect();
}

fn units(lat: usize) -> BTreeMap<NodeKey, Box<dyn Node>> {
    let mut m: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
    m.insert(LATENT, Box::new(Latent(lat)));
    m.insert(PROBE, Box::new(Probe));
    m
}

struct Rig {
    ed: Editor,
    exec: Executor,
    reference: Reference,
    /// Every block rendered, as both interpreters were handed it.
    played: Vec<Env>,
    /// The pieces the executor's ring holds, by its documented size: each
    /// piece's first frame and transport.
    kept: VecDeque<(u64, Transport)>,
    cap: usize,
    /// The plan's longest arrival: the latent node's latency.
    reach: u64,
    /// The rate blocks are rendered at.
    rate: SampleRate,
    /// Frames checked that were older than every piece kept, and after
    /// frame 0.
    clamped: usize,
}

fn capacity(lat: usize) -> usize {
    if lat == 0 {
        0
    } else {
        lat + 1 + MAX_TRANSPORT_CHANGES
    }
}

impl Rig {
    fn new(lat: usize) -> Self {
        let (mut ed, exec) = Editor::new(prepare(MAX));
        ed.insert(LATENT, "latent", Unforkable(Latent(lat)));
        ed.insert(PROBE, "probe", Unforkable(Probe));
        topology(&mut ed);
        ed.commit().expect("commits");
        let mut reference = Reference::new(prepare(MAX));
        reference.set_graph(&ed.spec().validate().expect("valid"), units(lat));
        Self {
            ed,
            exec,
            reference,
            played: Vec::new(),
            kept: VecDeque::new(),
            cap: capacity(lat),
            reach: lat as u64,
            rate: prepare(MAX).sample_rate(),
            clamped: 0,
        }
    }

    /// The latent node's latency moves (a plugin's pipeline changing).
    fn set_latency(&mut self, lat: usize) {
        self.ed
            .set_latency(LATENT, Latency::new(Samples(lat)))
            .expect("the node exists");
        self.ed.commit().expect("recompiles");
        self.reference
            .set_graph(&self.ed.spec().validate().expect("valid"), BTreeMap::new());
        self.cap = capacity(lat);
        self.reach = lat as u64;
        while self.kept.len() > self.cap {
            self.kept.pop_front();
        }
    }

    /// Re-prepares both at `rate`, with no block between the two commits.
    /// The ring is cleared: frames at the old rate name nothing at the new.
    fn reprepare(&mut self, rate: SampleRate) {
        let p = Prepare::new(rate, Samples(MAX));
        self.ed.reprepare(p).expect("reprepares");
        self.exec.apply_pending(); // checks the units out
        self.ed.collect(); // re-prepares them, sends them back
        self.exec.apply_pending(); // installs them
        self.ed.collect();
        self.reference.reprepare(p);
        self.rate = rate;
        self.kept.clear();
    }

    /// One block through both; asserts they agree bit for bit and returns
    /// the executor's outputs.
    fn block(&mut self, n: usize, t: Transport, changes: TransportChanges) -> [Vec<f32>; 3] {
        let env = Env {
            frame: self.exec.frame(),
            sample_rate: self.rate,
            block_len: Samples(n),
            transport: t,
            changes,
        };
        let mut a: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0.0; n]);
        let mut b: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0.0; n]);
        {
            let [a0, a1, a2] = &mut a;
            self.exec
                .process_with_changes(n, &t, &changes, &[], &mut [a0, a1, a2]);
            let [b0, b1, b2] = &mut b;
            self.reference
                .process_with_changes(n, &t, &changes, &[], &mut [b0, b1, b2]);
        }
        self.ed.collect();
        let bits = |v: &[Vec<f32>; 3]| {
            v.iter()
                .map(|c| c.iter().map(|x| x.to_bits()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(&a), bits(&b), "executor and reference");
        self.played.push(env);
        if self.cap > 0 {
            while self.kept.len() >= 2 && self.kept[1].0 + self.reach <= env.frame.get() {
                self.kept.pop_front();
            }
        }
        for (_, piece) in env.segments() {
            self.kept.push_back((piece.frame.get(), piece.transport));
            if self.kept.len() > self.cap {
                self.kept.pop_front();
            }
        }
        a
    }

    /// What the block holding timeline frame `f` said for it; older than
    /// every piece kept (or before frame 0), the oldest kept, unmoved.
    fn truth(&self, f: Option<u64>) -> Transport {
        let (oldest, t) = *self.kept.front().expect("a block was observed");
        let Some(f) = f.filter(|&f| f >= oldest) else {
            return t;
        };
        let e = self
            .played
            .iter()
            .find(|e| e.offset_of(Frame(f)).is_some())
            .expect("a block played that frame");
        e.transport_at(e.offset_of(Frame(f)).expect("inside"))
    }

    /// Every frame of the block just rendered reads the transport `lat`
    /// frames earlier.
    fn check(&mut self, out: &[Vec<f32>; 3], lat: usize, what: &str) {
        let e = *self.played.last().expect("rendered");
        for (i, k) in e.offsets().enumerate() {
            let f = (e.frame.get() + k.get() as u64).checked_sub(lat as u64);
            if f.is_some_and(|f| f < self.kept[0].0) {
                self.clamped += 1;
            }
            let want = encode(&self.truth(f));
            assert_eq!(out[2][i], lat as f32, "{what}: arrival");
            assert_eq!(
                [out[0][i], out[1][i]],
                want,
                "{what}: frame {i} of block at {}",
                e.frame.get()
            );
        }
    }
}

fn rolling(beat: f64) -> Transport {
    Transport::new(true, Bpm(120.0), Beat(beat), None)
}

/// The block-`b` scene for a block of `n` frames after `prev` (the block
/// before, if any): a plain roll continuing it, a seek, a seek and tempo
/// change inside the block, a 40-frame loop starting at the block, the loop
/// continued from where it stood, a stop, and recording.
fn scene(b: usize, n: usize, prev: Option<&Env>) -> (Transport, TransportChanges) {
    let at = |i: usize| Offset::new(i, Samples(n)).expect("inside");
    let mut changes = TransportChanges::NONE;
    let t = match b % 7 {
        0 => prev.map_or(rolling(1.0), |p| {
            rolling(p.transport.beat().get() + p.block_len.get() as f64 / FPB)
        }),
        1 => rolling(16.0 + b as f64),
        2 => {
            if n > 1 {
                changes
                    .push(
                        at(n / 2),
                        Transport::new(true, Bpm(90.0), Beat(64.0 + b as f64), None),
                    )
                    .expect("inside");
            }
            rolling(8.0 + b as f64)
        }
        3 => Transport::new(
            true,
            Bpm(120.0),
            Beat(4.0),
            Some(LoopRange {
                start: Beat(4.0),
                end: Beat(4.0 + 40.0 / FPB),
            }),
        ),
        // The loop, picked up at the previous block's last frame.
        4 => prev.map_or(rolling(2.0), |p| {
            p.transport_at(Offset::new(p.block_len.get() - 1, p.block_len).expect("last"))
        }),
        5 => Transport::new(false, Bpm(100.0), Beat(3.0), None),
        _ => rolling(32.0 + b as f64).with_recording(true),
    };
    (t, changes)
}

/// Ragged blocks behind a 100-frame path: every frame of every block reads
/// what the block that rendered its input said, two and more blocks back,
/// across every kind of transport discontinuity; the first 100 frames read
/// the first block's transport, unmoved. The probe's arrival is 100.
///
/// Mutation: in `Cx::transport_at`, read `self.env.transport_at(offset)`
/// (no arrival) → every frame reads its own block's transport → fails.
/// Mutation: drop `history.observe(&env)` from `Executor::process` → every
/// frame before the block reads the clamp (the executor's oldest piece
/// is from no block at all, so the block's own transport) → fails, and so
/// does the executor/reference comparison. Mutation: in `Executor::apply`,
/// ignore the commit's history (keep the empty `TransportHistory::new()`)
/// → no reach → fails.
#[test]
fn a_node_behind_a_latent_path_reads_the_transport_its_input_was_rendered_at() {
    const LAT: usize = 100;
    let mut rig = Rig::new(LAT);
    let lens = [64usize, 17, 50, 1, 33, 64, 2, 40];
    for b in 0..60 {
        let n = lens[b % lens.len()];
        let (t, c) = scene(b, n, rig.played.last());
        let out = rig.block(n, t, c);
        rig.check(&out, LAT, &format!("block {b}"));
    }
}

/// A recompile that raises the longest arrival hands the executor a larger
/// ring; one that lowers it a smaller one. Each takes the pieces the old
/// ring kept, so a node keeps reading what its blocks said, and the
/// reference, re-sized the same way, agrees bit for bit. Right after the
/// raise, one-frame blocks have left the old ring holding only its last 39
/// frames: the frames further back read its oldest piece, unmoved (the
/// documented boundary, and asserted to be reached), until the new ring has
/// filled.
///
/// Mutation: in `Editor::send`, never send a history (`None`) → the
/// executor keeps its `TransportHistory::new()`, which keeps nothing, so
/// every frame before the block reads the block's own transport → fails
/// (this test and the one above). Mutation: in `take_from`, drop the old
/// pieces instead of moving them → right after each change the executor's
/// ring holds only the new block → fails (this test alone).
#[test]
fn a_recompile_that_moves_the_reach_keeps_the_history() {
    let mut rig = Rig::new(30);
    let blocks =
        |rig: &mut Rig, count: usize, n: &dyn Fn(usize) -> usize, lat: usize, what: &str| {
            for b in 0..count {
                let n = n(b);
                let (t, c) = scene(b, n, rig.played.last());
                let out = rig.block(n, t, c);
                rig.check(&out, lat, &format!("{what}, block {b}"));
            }
        };
    blocks(&mut rig, 200, &|_| 1, 30, "at 30");
    rig.set_latency(300);
    blocks(
        &mut rig,
        400,
        &|b| if b % 5 == 4 { 64 } else { 1 },
        300,
        "raised to 300",
    );
    assert!(
        rig.clamped > 0,
        "the raise left frames older than the old ring"
    );
    rig.set_latency(10);
    blocks(&mut rig, 50, &|b| 1 + b % 3, 10, "lowered to 10");
}

/// A sample-rate change clears the history: the executor's clock is
/// rescaled to the new rate, and frames counted at the old one name nothing
/// there. The first blocks after it read the clamp (the first block at the
/// new rate, unmoved) until it has `reach` frames behind it, in the executor
/// and the reference alike; then every frame reads its block again.
///
/// Mutation: drop `self.history.clear()` from `Executor::apply`'s suspend →
/// the executor rolls the last 48 kHz piece across the rescaled gap and
/// disagrees with the model and the reference → fails. Mutation: drop
/// `self.history.clear()` from `Reference::suspend` → the reference does
/// the same → the executor/reference comparison fails.
#[test]
fn a_sample_rate_change_starts_the_history_over() {
    const LAT: usize = 100;
    let mut rig = Rig::new(LAT);
    for b in 0..12 {
        let (t, c) = scene(b, 64, rig.played.last());
        let out = rig.block(64, t, c);
        rig.check(&out, LAT, &format!("48 kHz, block {b}"));
    }
    rig.reprepare(SampleRate(96_000.0));
    let before = rig.clamped;
    for b in 0..12 {
        let (t, c) = scene(b, 64, rig.played.last());
        let out = rig.block(64, t, c);
        rig.check(&out, LAT, &format!("96 kHz, block {b}"));
    }
    assert!(
        rig.clamped - before >= LAT,
        "the first {LAT} frames after the change read the clamp ({} did)",
        rig.clamped - before
    );
}
