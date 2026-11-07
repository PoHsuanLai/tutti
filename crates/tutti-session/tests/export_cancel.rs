//! Stopping an export and reading its progress (`ExportHandle`), on an
//! offline session. No hardware and no sleeps: a node in the graph cancels
//! the job from inside the render, at a block the test picks.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tutti_export::{
    ChannelLayout, EncodeConfig, ExportConfig, Format, GRAPH_MAX_BLOCK, Normalize, Outcome, Pcm,
    RenderConfig,
};
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{
    Error, ExportClock, ExportHandle, ExportOutput, ExportRequest, ExportSource, ExportTarget,
    Session,
};
use tutti_types::{Db, SampleRate, Samples, Tail};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCKS: usize = 20;

/// A node that cancels the export it renders in when it processes its
/// `at`th block (counting across exports), and writes DC. Forked by clone,
/// so the fork shares the counter and the handle.
#[derive(Clone)]
struct CancelAt {
    at: usize,
    blocks: Arc<AtomicUsize>,
    handle: Arc<OnceLock<ExportHandle>>,
}

impl Node for CancelAt {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
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
    blocks: Arc<AtomicUsize>,
    handle: Arc<OnceLock<ExportHandle>>,
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
        blocks,
        handle,
    }
}

fn config() -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: (BLOCKS * GRAPH_MAX_BLOCK.get()) as f64 / RATE.get(),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Int16),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

fn file_request(path: &Path, normalize: Option<Normalize>) -> ExportRequest {
    ExportRequest::new(
        ExportSource::Mix,
        ExportTarget::File {
            path: path.to_path_buf(),
            normalize,
        },
        config(),
        ExportClock::frozen(),
    )
}

const TOTAL: Samples = Samples(BLOCKS * 1024);

/// A cancel from inside the render stops it at the end of that block, the
/// outcome is `Cancelled`, the partial file is gone, the token is released
/// (a following export starts and completes), and the reading froze where
/// the render stopped.
///
/// Mutation (run): `ExportHandle::tick` ignoring the stop request -> all 20
/// blocks render and the outcome is `Finished`; fails.
/// Mutation (run): `ExportRendered` kept in `finish_export` (token not
/// dropped) -> the following export is refused; fails.
#[test]
fn a_cancel_stops_the_render_and_frees_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");
    let mut r = rig(3);

    let job = r.session.export(file_request(&path, None)).unwrap();
    let handle = job.handle();
    r.handle.set(handle.clone()).ok();
    assert!(
        handle.progress().is_none(),
        "nothing before the first block"
    );
    assert!(matches!(
        r.session.export(file_request(&path, None)),
        Err(Error::ExportInFlight)
    ));

    let rendered = job.run();
    assert_eq!(
        r.blocks.load(Ordering::SeqCst),
        3,
        "stopped at the third block"
    );
    assert!(
        r.session.export_in_flight(),
        "the token lives until the main thread finishes the job"
    );
    let outcome = r.session.finish_export(rendered).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert!(!r.session.export_in_flight(), "the token is released");
    assert!(!path.exists(), "no partial file");
    let seen = handle.progress().unwrap();
    assert_eq!(seen.produced, Samples(3 * 1024));
    assert_eq!(seen.total, TOTAL);

    // The next export is not refused, and runs to its end.
    let again = r
        .session
        .export_blocking(file_request(&path, None))
        .unwrap();
    assert!(matches!(again, Outcome::Finished(ExportOutput::File(_))));
    assert!(path.exists());
    assert_eq!(r.blocks.load(Ordering::SeqCst), 3 + BLOCKS);
}

/// A job cancelled before it runs renders nothing and builds nothing.
///
/// Mutation (run): `run` checking the stop request neither before the build
/// nor after it -> the node processes a block; fails.
#[test]
fn a_job_cancelled_before_it_runs_renders_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.wav");
    let mut r = rig(usize::MAX);
    let job = r.session.export(file_request(&path, None)).unwrap();
    job.handle().cancel();
    let outcome = r.session.finish_export(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(r.blocks.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    assert!(!r.session.export_in_flight());
}

/// A rendering to memory stops the same way.
#[test]
fn a_buffer_export_cancels() {
    let mut r = rig(2);
    let job = r
        .session
        .export(ExportRequest::new(
            ExportSource::Mix,
            ExportTarget::Buffers,
            config(),
            ExportClock::frozen(),
        ))
        .unwrap();
    r.handle.set(job.handle()).ok();
    let outcome = r.session.finish_export(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert_eq!(r.blocks.load(Ordering::SeqCst), 2);
}

/// A render nobody cancels reports each block and ends on its total; a
/// normalized one ends on the second pass of two.
///
/// Mutation (run): `ExportHandle::tick` recording every reading as `Pass::ONLY`
/// -> the pass assertion fails.
#[test]
fn progress_ends_on_the_total_of_the_last_pass() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = rig(usize::MAX);

    let job = r
        .session
        .export(file_request(&dir.path().join("p.wav"), None))
        .unwrap();
    let handle = job.handle();
    let outcome = r.session.finish_export(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Finished(_)));
    let last = handle.progress().unwrap();
    assert_eq!((last.pass.index(), last.pass.count()), (0, 1));
    assert_eq!((last.produced, last.total), (TOTAL, TOTAL));

    let job = r
        .session
        .export(file_request(
            &dir.path().join("n.wav"),
            Some(Normalize::peak(Db(-6.0))),
        ))
        .unwrap();
    let handle = job.handle();
    let outcome = r.session.finish_export(job.run()).unwrap();
    assert!(matches!(outcome, Outcome::Finished(_)));
    let last = handle.progress().unwrap();
    assert_eq!((last.pass.index(), last.pass.count()), (1, 2));
    assert_eq!((last.produced, last.total), (TOTAL, TOTAL));
}
