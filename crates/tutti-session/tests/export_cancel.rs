//! Stopping a bounce and reading its progress (`BounceHandle`), on an
//! offline session. No hardware and no sleeps: a node in the graph cancels
//! the job from inside the render, at a block the test picks.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tutti_codec::{Format, Pcm};
use tutti_export::{GRAPH_MAX_BLOCK, Normalize, Outcome, Stage};
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{Bounce, BounceError, BounceHandle, ExportClock, Range, Session, Tail};
use tutti_types::ChannelLayout;
use tutti_types::{Db, Frame, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCKS: usize = 20;

/// A node that cancels the export it renders in when it processes its
/// `at`th block (counting across exports), and writes DC. Forked by clone,
/// so the fork shares the counter and the handle.
#[derive(Clone)]
struct CancelAt {
    at: usize,
    blocks: Arc<AtomicUsize>,
    handle: Arc<OnceLock<BounceHandle>>,
}

impl Node for CancelAt {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(tutti_types::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.25);
        if self.blocks.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
            self.handle.get().expect("the handle was set").cancel();
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Rig {
    session: Session,
    node: tutti_graph::NodeKey,
    blocks: Arc<AtomicUsize>,
    handle: Arc<OnceLock<BounceHandle>>,
}

fn rig(at: usize) -> Rig {
    let mut session = Session::builder().build_detached(RATE).unwrap();
    let blocks = Arc::new(AtomicUsize::new(0));
    let handle = Arc::new(OnceLock::new());
    let node = CancelAt {
        at,
        blocks: Arc::clone(&blocks),
        handle: Arc::clone(&handle),
    };
    let (node, _) = session.graph().add(ForkByClone(node));
    session.graph().set_outputs_from(node);
    Rig {
        session,
        node,
        blocks,
        handle,
    }
}

/// A bounce of `BLOCKS` graph blocks: 16-bit stereo, nothing trimmed and no
/// tail (the node reports an unbounded one), on a frozen clock.
fn bounce() -> Bounce {
    Bounce::project()
        .format(Format::Wav(Pcm::Int16))
        .channels(ChannelLayout::STEREO)
        .range(Range::Frames(
            Frame(0),
            Frame((BLOCKS * GRAPH_MAX_BLOCK.get()) as u64),
        ))
        .sample_rate(RATE)
        .clock(ExportClock::frozen())
        .tail(Tail::None)
}

fn file_request(path: &Path, normalize: Option<Normalize>) -> Bounce {
    let bounce = bounce().master(path);
    match normalize {
        Some(n) => bounce.normalize(n),
        None => bounce,
    }
}

const TOTAL: Samples = Samples(BLOCKS * 1024);

/// A cancel from inside the render stops it at the end of that block, the
/// outcome is `Cancelled`, the partial file is gone, the token is released
/// (a following export starts and completes), and the reading froze where
/// the render stopped (the render's entry: a reading comes every 4096 frames,
/// and three blocks are fewer).
#[test]
fn a_cancel_stops_the_render_and_frees_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");
    let mut r = rig(3);

    let job = r.session.bounce_start(file_request(&path, None)).unwrap();
    let handle = job.handle();
    r.handle.set(handle.clone()).ok();
    assert!(
        handle.progress().is_none(),
        "nothing before the first block"
    );
    assert!(matches!(
        r.session.bounce_start(file_request(&path, None)),
        Err(BounceError::InFlight)
    ));

    let rendered = job.run();
    assert_eq!(
        r.blocks.load(Ordering::SeqCst),
        3,
        "stopped at the third block"
    );
    assert!(
        r.session.bounce_in_flight(),
        "the token lives until the main thread finishes the job"
    );
    let outcome = r.session.finish_bounce(rendered).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert!(!r.session.bounce_in_flight(), "the token is released");
    assert!(!path.exists(), "no partial file");
    let seen = handle.progress().unwrap();
    assert_eq!(seen.stage(), Stage::Rendering);
    assert_eq!(seen.frames(), Some((Samples(0), TOTAL)));

    // The next export is not refused, and runs to its end.
    let again = r.session.bounce(file_request(&path, None)).unwrap();
    assert!(matches!(again, Outcome::Finished(_)));
    assert!(path.exists());
    assert_eq!(r.blocks.load(Ordering::SeqCst), 3 + BLOCKS);
}

/// A job cancelled before it runs renders nothing and builds nothing.
#[test]
fn a_job_cancelled_before_it_runs_renders_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.wav");
    let mut r = rig(usize::MAX);
    let job = r.session.bounce_start(file_request(&path, None)).unwrap();
    job.handle().cancel();
    let outcome = r.session.finish_bounce(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(r.blocks.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    assert!(!r.session.bounce_in_flight());
}

/// A rendering to memory stops the same way.
#[test]
fn a_buffer_export_cancels() {
    let mut r = rig(2);
    let job = r.session.bounce_start(bounce().master_buffers()).unwrap();
    r.handle.set(job.handle()).ok();
    let outcome = r.session.finish_bounce(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(r.blocks.load(Ordering::SeqCst), 2);
}

/// A render nobody cancels reports its stage and ends on 1.0 in the
/// finalizing stage, with and without normalize; the handle holds what the
/// pipeline last delivered, the one reading the session does not weight
/// itself.
#[test]
fn progress_ends_at_one_in_the_finalizing_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = rig(usize::MAX);

    for (name, normalize) in [("p.wav", None), ("n.wav", Some(Normalize::peak(Db(-6.0))))] {
        let job = r
            .session
            .bounce_start(file_request(&dir.path().join(name), normalize))
            .unwrap();
        let handle = job.handle();
        let outcome = r.session.finish_bounce(job.run()).unwrap();
        assert!(matches!(outcome, Outcome::Finished(_)));
        let last = handle.progress().unwrap();
        assert_eq!(
            (last.stage(), last.fraction()),
            (Stage::Finalizing, 1.0),
            "{name}"
        );
    }
}

/// A bounce of the master and a stem of the node into `dir`, normalized when
/// `normalize` is set (the stem then takes the master's gain, so the pass
/// keeps both for its measurement).
fn with_stem(r: &Rig, dir: &Path, normalize: bool) -> Bounce {
    let bounce = bounce()
        .master(dir.join("master.wav"))
        .stem(r.node, dir.join("stem.wav"));
    if normalize {
        bounce
            .normalize(Normalize::peak(Db(-6.0)))
            .normalize_stems(tutti_session::StemGain::Shared)
    } else {
        bounce
    }
}

/// **A cancel stops a bounce with stems like one without, and leaves no file
/// of any target**: neither the master nor the stem, and no temporary or
/// spill beside them, for a cancel inside the render (streamed and
/// normalized: the pass keeps the audio in the second) and for one before the
/// job runs. The token is released, and a bounce after it is not refused.
#[test]
fn a_cancel_leaves_no_file_of_any_target() {
    for normalize in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut r = rig(3);
        let job = r
            .session
            .bounce_start(with_stem(&r, dir.path(), normalize))
            .unwrap();
        r.handle.set(job.handle()).ok();
        let outcome = r.session.finish_bounce(job.run()).unwrap();
        assert!(
            matches!(outcome, Outcome::Cancelled),
            "normalize {normalize}"
        );
        assert_eq!(r.blocks.load(Ordering::SeqCst), 3, "stopped at block 3");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "normalize {normalize}: nothing is left beside the targets"
        );
        assert!(!r.session.bounce_in_flight());
    }

    let dir = tempfile::tempdir().unwrap();
    let mut r = rig(usize::MAX);
    let job = r
        .session
        .bounce_start(with_stem(&r, dir.path(), false))
        .unwrap();
    job.handle().cancel();
    let outcome = r.session.finish_bounce(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(r.blocks.load(Ordering::SeqCst), 0, "nothing rendered");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    // And one nobody cancels lands both files.
    let done = r.session.bounce(with_stem(&r, dir.path(), false)).unwrap();
    assert!(matches!(done, Outcome::Finished(_)));
    assert!(dir.path().join("master.wav").exists());
    assert!(dir.path().join("stem.wav").exists());
}
