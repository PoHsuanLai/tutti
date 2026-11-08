//! A forked unit that fails while the render runs never replaces the user's
//! file.
//!
//! The graph is one forkable node whose fork carries a health probe the test
//! flips from the render's own tick, as a hosted plugin's server dying
//! mid-render would. The render finishes (a dead unit renders silence), and
//! the only place the failure shows is the probe.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_export::{
    EncodeConfig, Error, ExportConfig, Flow, Format, FrozenClock, GRAPH_MAX_BLOCK, Normalize, Pcm,
    RenderConfig, RenderGraph, render_normalized_to_file, render_to_buffers_with, render_to_file,
    render_to_file_with,
};
use tutti_graph::{
    Cx, Editor, ForkCause, ForkFaultKind, ForkHealth, ForkMode, ForkSource, ForkTarget, Forked,
    IntoNode, Io, Node, NodeParts, Prepare, Shape, Status,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Db, NodeKey, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCKS: usize = 4;

#[derive(Clone)]
struct Dc;

impl Node for Dc {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.25);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The probe: a latch the test sets.
struct Crashed(AtomicBool);

impl ForkHealth for Crashed {
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        #[derive(Debug)]
        struct Died;
        impl std::fmt::Display for Died {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the server died")
            }
        }
        impl std::error::Error for Died {}
        self.0
            .load(Ordering::SeqCst)
            .then(|| (ForkFaultKind::Crashed, ForkCause::new(Died)))
    }
}

struct Fork(Arc<Crashed>);

impl ForkSource for Fork {
    fn fork(&self, _: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(Dc)).with_health(Arc::clone(&self.0) as Arc<dyn ForkHealth>))
    }
}

struct Watched(Arc<Crashed>);

impl IntoNode for Watched {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(Dc), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(Dc),
            controls: (),
            fork: Some(Box::new(Fork(self.0))),
        }
    }
}

/// A fork of a live graph holding the watched node, and the latch.
fn fork() -> (RenderGraph, Arc<Crashed>, Editor) {
    let probe = Arc::new(Crashed(AtomicBool::new(false)));
    let (mut live, _exec) = Editor::new(RenderGraph::prepare(RATE));
    live.insert(NodeKey(1), "watched", Watched(Arc::clone(&probe)));
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    let graph = RenderGraph::fork(&live, ForkTarget::Master, ForkMode::Offline, RATE)
        .expect("the node forks");
    (graph, probe, live)
}

fn config() -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: (BLOCKS * GRAPH_MAX_BLOCK.get()) as f64 / RATE.get(),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Float32),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

fn is_crash(err: &Error) -> bool {
    matches!(
        err,
        Error::ForkFailed { key, kind: ForkFaultKind::Crashed, .. } if *key == NodeKey(1)
    )
}

/// A unit that dies mid-render fails the export and leaves the file at the
/// path byte for byte as it was, with no temporary beside it; the same render
/// with a healthy unit replaces it.
///
/// Mutation: remove `src.check_health()?` from `encode_to_file` (the
/// `with_source` fallback still reports `ForkFailed`, but only after the
/// encoder renamed the new file into place) → the file differs → fails.
#[test]
fn a_fork_that_dies_mid_render_does_not_replace_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let before = b"the previous export".to_vec();
    std::fs::write(&path, &before).unwrap();

    let (graph, probe, _live) = fork();
    let mut blocks = 0;
    let err = render_to_file_with(graph, &config(), &FrozenClock, &path, &mut |_| {
        blocks += 1;
        if blocks == 2 {
            probe.0.store(true, Ordering::SeqCst);
        }
        Flow::Continue
    })
    .expect_err("the unit died");
    assert!(is_crash(&err), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "the file is intact");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "no temporary is left beside it");

    let (graph, _probe, _live) = fork();
    render_to_file(graph, &config(), &FrozenClock, &path).expect("healthy");
    assert_ne!(std::fs::read(&path).unwrap(), before);
}

/// The planes of a render a unit died in are not handed back, and the
/// normalizing path leaves the file alone too.
///
/// Mutation: remove the `src.check_health()?` in `render_pass` AND the
/// fallback in `with_source` → both return `Ok` → fails.
#[test]
fn a_fork_that_dies_fails_the_buffer_and_normalized_renders() {
    let (graph, probe, _live) = fork();
    probe.0.store(true, Ordering::SeqCst);
    let err = render_to_buffers_with(graph, &config(), &FrozenClock, &mut |_| Flow::Continue)
        .expect_err("the unit died");
    assert!(is_crash(&err), "{err:?}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    std::fs::write(&path, b"old").unwrap();
    let (graph, probe, _live) = fork();
    probe.0.store(true, Ordering::SeqCst);
    let err = render_normalized_to_file(
        graph,
        &config(),
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .expect_err("the unit died");
    assert!(is_crash(&err), "{err:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
}

/// A render the caller cancels is not judged: a fault that came before the
/// cancel does not turn `Cancelled` into `ForkFailed`, and the file is intact.
///
/// Mutation (run): `with_source` running `check_health` for a `Cancelled`
/// outcome too → `ForkFailed` → fails.
#[test]
fn a_cancel_wins_over_a_fork_fault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    std::fs::write(&path, b"old").unwrap();
    let (graph, probe, _live) = fork();
    let outcome = render_to_file_with(graph, &config(), &FrozenClock, &path, &mut |_| {
        probe.0.store(true, Ordering::SeqCst);
        Flow::Stop
    })
    .expect("a cancelled render is not an error");
    assert_eq!(outcome, tutti_export::Outcome::Cancelled);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
}
