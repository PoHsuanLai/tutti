//! Stems (doc 019 section 7): a node's own output written beside the master
//! in the same pass, through a plan tap on the master's fork.
//!
//! What is pinned here is what only a stem has: it is the node's output and
//! not what is downstream of it, at the node's own width and in the tapped
//! order; it leaves the plan at the master's time (so the stems sum to the
//! master sample for sample, with the latency trimmed or not); every target
//! has the same length; the stems take the master's gain or their own; and a
//! stem that cannot be rendered, or two files at one path, are refused by name
//! before anything is written.

use std::collections::HashMap;
use std::path::Path;

use tutti_codec::{Format, Pcm};
use tutti_export::{Error, Normalize, Produced, Resample, TargetError};
use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{
    Bounce, BounceConfigError, BounceError, BounceLatency, BounceTail, ExportClock, InPort,
    OutPort, PreparedGraph, Range, Root, Session, Stem, StemGain,
};
use tutti_session::{Bounced, BufferMaster, Master};
use tutti_types::ChannelLayout;
use tutti_types::{Db, SampleRate, Samples, Seconds};

const RATE: SampleRate = SampleRate(48_000.0);

/// A detached stereo session at the render's rate.
fn session() -> Session {
    Session::builder()
        .inputs(0)
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("a detached session builds")
}

/// The mix into memory for `seconds`: nothing trimmed, no tail, no musical
/// time.
fn base(seconds: f64) -> Bounce {
    Bounce::project()
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

/// [`base`] into memory.
fn bounce(seconds: f64) -> Bounce<NodeKey, BufferMaster> {
    base(seconds).master_buffers()
}

/// The planes of every target a finished bounce produced, master first.
fn planes(out: Result<Bounced<BufferMaster>, BounceError>) -> Vec<Vec<Vec<f32>>> {
    let (master, stems) = out.expect("the bounce renders").into_parts();
    std::iter::once(master.into_planes())
        .chain(stems.into_iter().map(|p| match p {
            Produced::Buffers(rendered) => rendered.into_planes(),
            other => panic!("expected planes, got {other:?}"),
        }))
        .collect()
}

/// What a bounce is refused with, started and run on this thread.
fn refused<M: Master>(session: &mut Session, bounce: Bounce<NodeKey, M>) -> BounceError {
    match session.bounce(bounce) {
        Err(e) => e,
        Ok(out) => panic!("expected a refusal, got {:?}", out.stems().len()),
    }
}

/// Every sample of a WAV file written by a bounce (32-bit float, so read back
/// exactly), plane after plane, and its frame count.
fn read_wav(path: &Path) -> (Vec<Vec<f32>>, usize) {
    let planes = tutti_fixtures::signal::read_wav_f32(path);
    let frames = planes.first().map_or(0, Vec::len);
    (planes, frames)
}

fn float_wav() -> Format {
    Format::Wav(Pcm::Float32)
}

/// A source that is silent for `LATE` frames and then 1.0, and declares `LATE`
/// frames of latency: a look-ahead processor reduced to its bookkeeping. The
/// graph delays every path that does not have this latency by `LATE` frames to
/// meet it at the outputs.
#[derive(Clone)]
struct Late {
    pos: usize,
}

const LATE: usize = 100;

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_latency(tutti_types::Latency::new(Samples(LATE)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = if self.pos >= LATE { 1.0 } else { 0.0 };
            self.pos += 1;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.pos = 0;
    }
}

/// Half of its mono input: what a bus or a fader downstream of a node does.
#[derive(Clone)]
struct Half;

impl Node for Half {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = 0.5 * i;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **Stems leave the plan at the master's time, so they sum to it.** A
/// constant 0.5 (no latency) and the `Late` step (latency [`LATE`]) both feed
/// the two outputs. The plan delays the constant by [`LATE`] frames to meet the
/// step, so with the latency kept: the constant's stem is 0 for [`LATE`] frames
/// and then 0.5 (not 0.5 from frame 0, which is the node as it leaves the node),
/// the step's is 0 then 1, and the master is their sum, 0 then 1.5, sample for
/// sample; with the latency trimmed every one starts on its programme, and all
/// of them have the same length.
#[test]
fn stems_are_aligned_with_the_master_and_sum_to_it() {
    let mut session = session();
    let (steady, _) = session.graph().insert(Const::mono(0.5));
    let (late, _) = session.graph().insert(ForkByClone(Late { pos: 0 }));
    for channel in 0..2 {
        session
            .graph()
            .connect(OutPort::new(steady, 0), Root::output(channel))
            .unwrap();
        session
            .graph()
            .connect(OutPort::new(late, 0), Root::output(channel))
            .unwrap();
    }
    let frames = 2_000;
    let seconds = Samples(frames).to_seconds(RATE).get();
    let stems = |bounce: Bounce<NodeKey, BufferMaster>| {
        bounce
            .stem_with(Stem::buffers(steady.erase()))
            .stem_with(Stem::buffers(late.erase()))
    };

    // BounceLatency kept: the programme starts `LATE` frames in.
    let kept = planes(session.bounce(stems(bounce(seconds))));
    let [master, constant, step] = kept.as_slice() else {
        panic!("the master and two stems")
    };
    assert_eq!(constant.len(), 1, "a mono node's stem is mono");
    for (what, plane, before, after) in [
        ("constant", &constant[0], 0.0, 0.5),
        ("step", &step[0], 0.0, 1.0),
        ("master", &master[0], 0.0, 1.5),
    ] {
        assert_eq!(
            plane.len(),
            frames + LATE,
            "{what}: length, the kept latency on top"
        );
        assert!(
            plane[..LATE].iter().all(|&s| s == before),
            "{what}: before the programme"
        );
        assert!(
            plane[LATE..].iter().all(|&s| s == after),
            "{what}: the programme from frame {LATE}"
        );
    }
    for (i, ((m, a), b)) in master[0].iter().zip(&constant[0]).zip(&step[0]).enumerate() {
        assert_eq!(m.to_bits(), (a + b).to_bits(), "the stems sum at {i}");
    }
    assert_eq!(master[0], master[1], "both master channels hear both");

    // BounceLatency trimmed: the programme from frame 0, and the same length.
    let trimmed = planes(session.bounce(stems(bounce(seconds).latency(BounceLatency::Graph))));
    for (what, plane, level) in [
        ("master", &trimmed[0][0], 1.5),
        ("constant", &trimmed[1][0], 0.5),
        ("step", &trimmed[2][0], 1.0),
    ] {
        assert_eq!(plane.len(), frames, "{what}: trimmed length");
        assert!(plane.iter().all(|&s| s == level), "{what}: from frame 0");
    }
}

/// **A stem is the node's own output, at the node's own width and in the order
/// tapped; what is downstream of it is not in it.** A constant 0.5 feeds a
/// node that halves it, which feeds the outputs: the master and the halver's
/// stem read 0.25, and the constant's stem 0.5. A three-channel node heard on
/// two output channels is a three-channel stem, and `channels` taps some of
/// them in the order given.
#[test]
fn a_stem_is_the_nodes_own_output_and_not_what_is_downstream() {
    let mut session = session();
    let (source, _) = session.graph().insert(Const::mono(0.5));
    let (half, _) = session.graph().insert(ForkByClone(Half));
    session
        .graph()
        .set_sources(InPort::new(half, 0), [OutPort::new(source, 0)])
        .unwrap();
    session.graph().connect(half, Root).unwrap();
    let (wide, _) = session.graph().insert(Const::frame(&[0.25, -0.5, 0.75]));
    for channel in 0..2 {
        session
            .graph()
            .connect(OutPort::new(wide, channel), Root::output(channel))
            .unwrap();
    }

    let out = planes(
        session.bounce(
            bounce(0.01)
                .stem_with(Stem::buffers(source.erase()))
                .stem_with(Stem::buffers(half.erase()))
                .stem_with(Stem::buffers(wide.erase()))
                .stem_with(Stem::buffers(wide.erase()).channels([2, 0])),
        ),
    );
    let [master, source_stem, half_stem, wide_stem, picked] = out.as_slice() else {
        panic!("the master and four stems")
    };
    assert_eq!(master.len(), 2, "the master is the root's width");
    assert!(master[0].iter().all(|&s| s == 0.25 + 0.25), "0.25 and 0.25");
    assert!(
        source_stem[0].iter().all(|&s| s == 0.5),
        "before the halver"
    );
    assert!(half_stem[0].iter().all(|&s| s == 0.25), "the halver's own");
    assert_eq!(wide_stem.len(), 3, "the node's width, not the root's");
    for (plane, level) in wide_stem.iter().zip([0.25, -0.5, 0.75]) {
        assert!(plane.iter().all(|&s| s == level), "channel at {level}");
    }
    assert_eq!(picked.len(), 2, "two channels tapped");
    assert!(picked[0].iter().all(|&s| s == 0.75), "channel 2 first");
    assert!(picked[1].iter().all(|&s| s == 0.25), "then channel 0");
}

/// **A stem whose channels cannot be placed on its layout is refused when it
/// renders, never truncated**: three discrete channels onto a mono file have
/// no placement, and a topology of another width than the tapped channels is
/// not them. Nothing is written for either.
#[test]
fn a_stem_that_cannot_be_placed_is_refused_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = session();
    let (wide, _) = session.graph().insert(Const::frame(&[0.25, -0.5, 0.75]));
    for channel in 0..2 {
        session
            .graph()
            .connect(OutPort::new(wide, channel), Root::output(channel))
            .unwrap();
    }
    let to_mono = Stem::new(wide.erase(), dir.path().join("mono.wav")).layout(ChannelLayout::MONO);
    let wrong_width = Stem::new(wide.erase(), dir.path().join("two.wav")).topology(
        tutti_types::ChannelTopology::for_width(ChannelLayout::STEREO),
    );
    for stem in [to_mono, wrong_width] {
        let b = bounce(0.01).format(float_wav()).stem_with(stem);
        match refused(&mut session, b) {
            BounceError::Export(Error::Target { output: 1, why }) => {
                assert!(
                    matches!(why, TargetError::Placement(_) | TargetError::Lanes { .. }),
                    "{why:?}"
                );
            }
            other => panic!("expected the stem's target refused, got {other:?}"),
        }
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "untouched"
    );
}

/// **Every target is the same length after the same resample**, and a stem
/// takes its own format: the master as a 16-bit file, a stem as 32-bit float,
/// both converted to 44.1 kHz from 48 kHz, are the same number of frames, and
/// the float stem holds the node's level exactly.
#[test]
fn every_target_has_the_same_length_and_a_stem_its_own_format() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = session();
    let (node, _) = session.graph().insert(Const::mono(0.5));
    session.graph().connect(node, Root).unwrap();
    let master = dir.path().join("master.wav");
    let stem = dir.path().join("stem.wav");
    let done = session
        .bounce(
            base(0.25)
                .format(Format::Wav(Pcm::Int16))
                .resample(Resample::to(44_100_u32))
                .master(&master)
                .stem_with(Stem::new(node.erase(), &stem).format(float_wav())),
        )
        .expect("renders");
    assert_eq!(done.stems().len(), 1);

    let master_spec = hound::WavReader::open(&master).unwrap().spec();
    assert_eq!(
        (master_spec.bits_per_sample, master_spec.sample_rate),
        (16, 44_100)
    );
    let (planes, frames) = read_wav(&stem);
    let master_frames = hound::WavReader::open(&master).unwrap().duration() as usize;
    assert_eq!(frames, master_frames, "one length for every target");
    assert_eq!(planes.len(), 1, "the node's width");
    // The resampler's edges ring; its middle is the node's constant.
    let middle = &planes[0][frames / 4..3 * frames / 4];
    assert!(middle.iter().all(|&s| (s - 0.5).abs() < 1e-3));
}

/// **Shared gain keeps the balance; each measures its own.** Two constants
/// 0.5 and 0.25 feed the outputs (a master of 0.75). With `Shared` the stems
/// are written with the master's gain: the files are 0.5 and 0.25 times it,
/// still summing to the master's samples, and all three report one gain. With
/// `Each` each stem is measured alone: the quieter one is given twice the
/// loud one's gain (the same shape at half the level), and both land on the
/// same peak. `None` writes them as rendered.
#[test]
fn shared_gain_keeps_the_balance_and_each_measures_its_own() {
    let mut session = session();
    let (loud, _) = session.graph().insert(Const::mono(0.5));
    let (quiet, _) = session.graph().insert(Const::mono(0.25));
    for channel in 0..2 {
        session
            .graph()
            .connect(OutPort::new(loud, 0), Root::output(channel))
            .unwrap();
        session
            .graph()
            .connect(OutPort::new(quiet, 0), Root::output(channel))
            .unwrap();
    }
    let target = Normalize::peak(Db(-6.0));
    let dir = tempfile::tempdir().unwrap();
    // The three files' samples, and the gain each reports.
    let run = |session: &mut Session, tag: &str, gain: StemGain| {
        let paths = ["master", "loud", "quiet"].map(|n| dir.path().join(format!("{tag}-{n}.wav")));
        let done = session
            .bounce(
                base(0.1)
                    .format(float_wav())
                    .master(&paths[0])
                    .normalize(target)
                    .stem(loud.erase(), &paths[1])
                    .stem(quiet.erase(), &paths[2])
                    .normalize_stems(gain),
            )
            .expect("renders");
        let (master, stems) = done.into_parts();
        let gains: Vec<Option<Db>> = std::iter::once(master.gain)
            .chain(stems.into_iter().map(|p| match p {
                Produced::File(written) => written.gain,
                other => panic!("expected a file, got {other:?}"),
            }))
            .collect();
        let levels = paths.clone().map(|p| {
            let plane = read_wav(&p).0.swap_remove(0);
            plane[plane.len() / 2]
        });
        (paths, gains, levels)
    };
    let amp = |db: Db| db.to_amplitude().get();

    let (paths, gains, [master, loud_level, quiet_level]) =
        run(&mut session, "shared", StemGain::Shared);
    let g = gains[0].expect("the master was normalized");
    assert_eq!(
        gains,
        [Some(g); 3],
        "one gain for the master and both stems"
    );
    assert!((master - 0.75 * amp(g)).abs() < 1e-5, "the master's gain");
    assert!((loud_level - 0.5 * amp(g)).abs() < 1e-5, "the loud stem's");
    assert!(
        (quiet_level - 0.25 * amp(g)).abs() < 1e-5,
        "the quiet stem's"
    );
    let [m, a, b] = paths.map(|p| read_wav(&p).0.swap_remove(0));
    for ((m, a), b) in m.iter().zip(&a).zip(&b) {
        assert!((m - (a + b)).abs() < 1e-6, "the stems sum to the master");
    }

    let (_, gains, [_, loud_level, quiet_level]) =
        run(&mut session, "each", StemGain::Each(target));
    let (loud_gain, quiet_gain) = (gains[1].unwrap(), gains[2].unwrap());
    assert!(
        (quiet_gain.get() - loud_gain.get() - 6.0206).abs() < 0.01,
        "half the level, twice the gain: {loud_gain:?} {quiet_gain:?}"
    );
    assert!(
        (loud_level - quiet_level).abs() < 1e-4,
        "both land on one peak: {loud_level} {quiet_level}"
    );

    let (_, gains, [_, loud_level, quiet_level]) = run(&mut session, "raw", StemGain::None);
    assert_eq!(gains[1..], [None, None]);
    assert_eq!((loud_level, quiet_level), (0.5, 0.25));
}

/// **A request that cannot work is refused by name before any fork**: a
/// shared gain with no normalized master, a normalize of a stem held in
/// memory, a stem file with no format anywhere, and two targets at one path
/// (the master and a stem, or two stems).
#[test]
fn a_request_that_cannot_work_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    let mut session = session();
    let (node, _) = session.graph().insert(Const::mono(0.5));
    session.graph().connect(node, Root).unwrap();
    fn why<M: Master>(session: &mut Session, b: Bounce<NodeKey, M>) -> BounceConfigError {
        match refused(session, b) {
            BounceError::Config(why) => why,
            other => panic!("expected a refusal by name, got {other:?}"),
        }
    }

    assert_eq!(
        why(
            &mut session,
            base(0.01)
                .format(float_wav())
                .master(path("m.wav"))
                .stem(node.erase(), path("s.wav"))
                .normalize_stems(StemGain::Shared),
        ),
        BounceConfigError::SharedGainWithoutMaster
    );
    assert_eq!(
        why(
            &mut session,
            bounce(0.01)
                .stem_with(Stem::buffers(node.erase()))
                .normalize_stems(StemGain::Each(Normalize::peak(Db(-1.0)))),
        ),
        BounceConfigError::NormalizeBuffers
    );
    assert_eq!(
        why(&mut session, bounce(0.01).stem(node.erase(), path("s.wav"))),
        tutti_session::BounceConfigError::Missing(tutti_session::Field::Format)
    );
    assert_eq!(
        why(
            &mut session,
            base(0.01)
                .format(float_wav())
                .master(path("one.wav"))
                .stem(node.erase(), path("one.wav")),
        ),
        BounceConfigError::DuplicatePath(path("one.wav"))
    );
    assert_eq!(
        why(
            &mut session,
            base(0.01)
                .format(float_wav())
                .master(path("m.wav"))
                .stem(node.erase(), path("two.wav"))
                .stem(node.erase(), path("two.wav")),
        ),
        BounceConfigError::DuplicatePath(path("two.wav"))
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "untouched"
    );
}

/// **A host names nodes its own way and resolves them once**: `try_map`
/// turns a bounce over names into one over keys, and the first node it cannot
/// resolve is its error, with nothing started.
#[test]
fn a_host_resolves_its_own_names_once() {
    let mut session = session();
    let (kick, _) = session.graph().insert(Const::mono(0.5));
    session.graph().connect(kick, Root).unwrap();
    let keys: HashMap<&str, _> = HashMap::from([("kick", kick.erase())]);

    let named = |name| Bounce::<&str>::project().stem_with(Stem::buffers(name));
    let resolved = named("kick")
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(0.01)))
        .clock(ExportClock::frozen())
        .try_map(|name| keys.get(name).copied().ok_or(name))
        .expect("kick is a node");
    let out = planes(session.bounce(resolved));
    assert!(out[1][0].iter().all(|&s| s == 0.5), "the kick's stem");

    let unresolved = named("snare").try_map(|name| keys.get(name).copied().ok_or(name));
    assert_eq!(unresolved.err(), Some("snare"));
}

/// **An edit that removes a stem's node is refused by name**, not rendered as
/// silence: the start's check passed, and the taps open on the edited fork.
#[test]
fn a_node_the_edit_removed_is_refused_where_the_taps_open() {
    let mut session = session();
    let (node, _) = session.graph().insert(Const::mono(0.5));
    session.graph().connect(node, Root).unwrap();
    let b = bounce(0.01)
        .stem_with(Stem::buffers(node.erase()))
        .edit(Box::new(move |prepared: PreparedGraph<'_>| {
            prepared
                .editor
                .spec_mut()
                .topology
                .nodes
                .remove(&node.erase());
        }));
    match refused(&mut session, b) {
        BounceError::Config(BounceConfigError::StemNotInGraph { node: gone }) => {
            assert_eq!(gone, node.erase())
        }
        other => panic!("expected StemNotInGraph, got {other:?}"),
    }
}
