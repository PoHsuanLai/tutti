//! The play clock (doc 014 §4.3) as the executor keeps it: one per frame,
//! rolling or stopped; restarted at 0 in a fresh epoch on the frame of every
//! stopped→playing transition, between blocks or inside one; untouched by a
//! seek or a stop; read at a node's input time behind a latent path
//! (`Cx::play_at`); published to the control side (`Editor::play_reading`);
//! and started at 0 in a fork, whatever the live graph's count.
//!
//! The reference interpreter keeps a clock of its own, written apart from
//! the executor's: the counts are compared frame for frame. Epochs are not,
//! since each interpreter draws its own from the process-wide counter; each
//! is checked against the restarts it must mark.

mod common;

use std::collections::BTreeMap;

use common::prepare;
use tutti_graph::{
    Cx, Editor, Executor, ForkByClone, ForkMode, ForkTarget, Io, Node, Offset, PlayReading,
    Prepare, Reference, Shape, Status, Transport, TransportChanges, Unforkable,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, Frame, Latency, NodeKey, Samples};

const MAX: usize = 64;
const LATENT: NodeKey = NodeKey(1);
const PROBE: NodeKey = NodeKey(2);

/// A source declaring `lat` frames of latency, so the probe after it
/// arrives that late.
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

/// Writes, per frame, the play clock at its input time (`Cx::play_at`):
/// the count on output 0 and the epoch on output 1 (both exact in `f32`:
/// these tests count far below 2^24 frames, and an epoch is 24 bits).
#[derive(Clone)]
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (i, k) in cx.env.offsets().enumerate() {
            let r = cx.play_at(k);
            io.output(0)[i] = r.play.get() as f32;
            io.output(1)[i] = r.epoch as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn transport(playing: bool, beat: f64) -> Transport {
    Transport::new(playing, Bpm(120.0), Beat(beat), None)
}

fn changes(list: &[(usize, Transport)]) -> TransportChanges {
    let mut c = TransportChanges::NONE;
    for &(at, t) in list {
        c.push(Offset::new(at, Samples(MAX)).expect("in a block"), t)
            .expect("fits");
    }
    c
}

/// The probe after a `lat`-frame source, in an editor and in the
/// reference.
struct Rig {
    ed: Editor,
    exec: Executor,
    reference: Reference,
}

fn rig(lat: usize, forkable: bool) -> Rig {
    let (mut ed, exec) = Editor::new(prepare(MAX));
    if forkable {
        ed.insert(LATENT, "latent", ForkByClone(Latent(lat)));
        ed.insert(PROBE, "probe", ForkByClone(Probe));
    } else {
        ed.insert(LATENT, "latent", Unforkable(Latent(lat)));
        ed.insert(PROBE, "probe", Unforkable(Probe));
    }
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
    t.outputs = (0..2)
        .map(|port| Source::Node(OutPort { node: PROBE, port }))
        .collect();
    ed.commit().expect("commits");
    let mut reference = Reference::new(prepare(MAX));
    let mut units: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
    units.insert(LATENT, Box::new(Latent(lat)));
    units.insert(PROBE, Box::new(Probe));
    reference.set_graph(&ed.spec().validate().expect("valid"), units);
    Rig {
        ed,
        exec,
        reference,
    }
}

impl Rig {
    /// One block through both: the executor's readings, per frame, after
    /// asserting the reference counted the same frames.
    fn block(&mut self, n: usize, t: Transport, ch: &[(usize, Transport)]) -> Vec<PlayReading> {
        let ch = changes(ch);
        let (mut a0, mut a1) = (vec![0.0; n], vec![0.0; n]);
        let (mut b0, mut b1) = (vec![0.0; n], vec![0.0; n]);
        self.exec
            .process_with_changes(n, &t, &ch, &[], &mut [&mut a0, &mut a1]);
        self.reference
            .process_with_changes(n, &t, &ch, &[], &mut [&mut b0, &mut b1]);
        self.ed.collect();
        assert_eq!(a0, b0, "the reference counts the same frames");
        readings(&a0, &a1)
    }
}

fn readings(play: &[f32], epoch: &[f32]) -> Vec<PlayReading> {
    play.iter()
        .zip(epoch)
        .map(|(&p, &e)| PlayReading {
            epoch: e as u32,
            play: Frame(p as u64),
        })
        .collect()
}

/// `n` readings counting on from `from`.
fn run(from: PlayReading, n: usize) -> Vec<PlayReading> {
    (0..n).map(|i| from.after(Samples(i))).collect()
}

/// The clock counts while stopped, restarts at 0 in a new epoch on a start
/// between blocks and on a start inside one (on its frame), and a seek or a
/// stop restarts nothing. The control side reads the next block's first
/// reading.
///
/// Mutations (run): `PlayState::begin` restarting whenever the block plays
/// (ignoring the last block's state) → the third block restarts → fails.
/// `Env::play_at` restarting on a seek (any change to playing) → the seek
/// at 5 restarts → fails. `PlayState::end` not publishing → the control
/// reading stays at 0 → fails. The reference's `play_clock` ignoring a
/// start inside a block (its next block counting on from the old start)
/// → the fifth block's counts differ from the executor's → fails.
#[test]
fn the_clock_restarts_on_each_play_start_and_nowhere_else() {
    let mut r = rig(0, false);
    let first = r.ed.play_reading();
    assert_eq!(first.play, Frame(0), "a new executor starts at 0");

    let b1 = r.block(64, transport(false, 0.0), &[]);
    assert_eq!(b1, run(first, 64), "stopped, it counts");
    assert_eq!(r.ed.play_reading(), first.after(Samples(64)));

    // A start between blocks: 0 on the block's first frame, a new epoch.
    let b2 = r.block(64, transport(true, 0.0), &[]);
    let e1 = b2[0].epoch;
    assert_ne!(e1, first.epoch, "a start takes a new epoch");
    assert_eq!(
        b2,
        run(
            PlayReading {
                epoch: e1,
                play: Frame(0)
            },
            64
        )
    );

    // A seek between blocks and one inside: neither restarts.
    let b3 = r.block(64, transport(true, 40.0), &[(5, transport(true, 3.0))]);
    assert_eq!(b3, run(b2[0].after(Samples(64)), 64));

    // A stop at 10 and a start at 30, inside one block.
    let b4 = r.block(
        64,
        transport(true, 3.5),
        &[(10, transport(false, 3.6)), (30, transport(true, 0.0))],
    );
    assert_eq!(b4[..30], run(b3[0].after(Samples(64)), 30)[..]);
    let e2 = b4[30].epoch;
    assert!(e2 != e1 && e2 != first.epoch, "another epoch: {e2}");
    assert_eq!(
        b4[30..],
        run(
            PlayReading {
                epoch: e2,
                play: Frame(0)
            },
            34
        )[..]
    );
    assert_eq!(
        r.ed.play_reading(),
        PlayReading {
            epoch: e2,
            play: Frame(34)
        }
    );
    assert_eq!(r.exec.play_reading(), r.ed.play_reading());
    // And on from there.
    let b5 = r.block(64, transport(true, 4.0), &[]);
    assert_eq!(b5, run(b4[63].after(Samples(1)), 64));
}

/// Behind a 100-frame path, the probe reads the clock its input was
/// rendered at: 100 frames back, through block boundaries and across a play
/// start, from the executor's history.
///
/// Mutations (run): `Cx::play_at` reading the block's own time (no arrival)
/// → the first playing block reads 0 where it must read the old epoch's
/// count → fails. `TransportHistory::observe` storing `PlayClock::ZERO`'s
/// reading in each piece → reads before the block read 0 → fails.
#[test]
fn a_latent_node_reads_the_clock_at_its_input_time() {
    const LAT: usize = 100;
    let mut r = rig(LAT, false);
    let first = r.ed.play_reading();
    for _ in 0..4 {
        r.block(64, transport(false, 0.0), &[]);
    }
    // Starts at frame 256, a block start.
    let a = r.block(64, transport(true, 0.0), &[]);
    // Frames 256..320 read 156..220: the stopped epoch, still counting.
    assert_eq!(a, run(first.after(Samples(256 - LAT)), 64));
    let b = r.block(64, transport(true, 0.5), &[]);
    // Frames 320..384 read 220..284: the start at 256 lands at frame 36.
    assert_eq!(b[..36], run(first.after(Samples(320 - LAT)), 36)[..]);
    let e = b[36].epoch;
    assert_ne!(e, first.epoch);
    assert_eq!(
        b[36..],
        run(
            PlayReading {
                epoch: e,
                play: Frame(0)
            },
            28
        )[..]
    );
    // The control side reads the clock at the block's own time.
    assert_eq!(
        r.ed.play_reading(),
        PlayReading {
            epoch: e,
            play: Frame(128)
        }
    );
}

/// A fork's clock starts at 0, in an epoch of its own, however long the
/// live graph has played: an export's free-running phase starts from its
/// own first frame (doc 014 §4.3), and the fork's readings never name a
/// live play start.
///
/// Mutation (run): one clock shared by every executor (`PlayState::new`
/// seeding its count from a process-wide copy of the newest reading `end`
/// published) → the fork's first frame reads 192 → fails.
#[test]
fn a_fork_starts_its_clock_at_zero() {
    let mut r = rig(0, true);
    for _ in 0..3 {
        r.block(64, transport(true, 0.0), &[]);
    }
    let live = r.ed.play_reading();
    assert!(live.play.get() > 0);
    let (mut fed, mut fexec) =
        r.ed.fork(ForkTarget::Master, ForkMode::Live, prepare(MAX))
            .expect("forks");
    let fresh = fed.play_reading();
    assert_eq!(fresh.play, Frame(0));
    assert_ne!(fresh.epoch, live.epoch);
    let (mut o0, mut o1) = (vec![0.0; 64], vec![0.0; 64]);
    fexec.process(64, &transport(false, 0.0), &[], &mut [&mut o0, &mut o1]);
    fed.collect();
    assert_eq!(readings(&o0, &o1), run(fresh, 64));
    assert_eq!(r.ed.play_reading(), live, "the live clock is untouched");
}
