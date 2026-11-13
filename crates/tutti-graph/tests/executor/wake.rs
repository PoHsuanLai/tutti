//! A node that parked itself on `Status::Idle` is called again on the next
//! block after its `Wake` is raised from any thread, and not before.

use tutti_graph::testing as common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{Pulse, WORKERS, bits, prepare, test_pool};
use tutti_graph::{
    Cx, Editor, Executor, ForkByClone, ForkMode, ForkTarget, Io, Node, NodeKey, OutPort, Prepare,
    Shape, Source, Status, TransportPosition, Unforkable, Wake,
};
use tutti_types::ChannelLayout;

/// Every `Wake` a placement of a [`Napper`] was given, with that placement's
/// call counter, in the order the editor placed them.
type Placements = Arc<Mutex<Vec<(Wake, Arc<AtomicUsize>)>>>;

/// A node with an event input that returns `Status::Idle` from every call, so
/// the executor parks it until something wakes it. It counts its calls in a
/// counter of its own (a clone counts in a fresh one) and records the `Wake`
/// it is given.
struct Napper {
    calls: Arc<AtomicUsize>,
    placements: Placements,
}

impl Napper {
    fn new(placements: &Placements) -> Self {
        Self {
            calls: Arc::default(),
            placements: Arc::clone(placements),
        }
    }
}

impl Clone for Napper {
    fn clone(&self) -> Self {
        Self::new(&self.placements)
    }
}

impl Node for Napper {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(1, 0)
    }
    fn bind_wake(&mut self, wake: Wake) {
        self.placements
            .lock()
            .expect("the registry")
            .push((wake, Arc::clone(&self.calls)));
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Status::Idle
    }
    fn reset(&mut self) {}
}

const NAP: NodeKey = NodeKey(1);
const BLOCK: usize = 32;

fn placed(placements: &Placements, n: usize) -> (Wake, Arc<AtomicUsize>) {
    placements.lock().expect("the registry")[n].clone()
}

fn render(exec: &mut Executor, outs: usize) {
    let mut bufs = vec![vec![0.0f32; BLOCK]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
}

/// One napper, wired to the master.
fn one_napper() -> (Editor, Executor, Placements) {
    let placements = Placements::default();
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(NAP, "napper", ForkByClone(Napper::new(&placements)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: NAP, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, placements)
}

fn raise_from_another_thread(wake: &Wake) {
    let wake = wake.clone();
    std::thread::spawn(move || wake.raise())
        .join()
        .expect("the raiser");
}

/// **A raise from another thread between blocks calls the parked node on
/// exactly the next block**, and not on the one after: it returns `Idle`
/// again with no raise, so it parks again.
#[test]
fn a_raise_from_another_thread_calls_a_parked_node_on_exactly_the_next_block() {
    let (_ed, mut exec, placements) = one_napper();
    let (wake, calls) = placed(&placements, 0);
    let calls = || calls.load(Ordering::Relaxed);

    render(&mut exec, 1);
    assert_eq!(calls(), 1, "called once, returned Idle");
    render(&mut exec, 1);
    assert_eq!(calls(), 1, "parked");

    raise_from_another_thread(&wake);
    render(&mut exec, 1);
    assert_eq!(calls(), 2, "called on the block after the raise");
    render(&mut exec, 1);
    render(&mut exec, 1);
    assert_eq!(calls(), 2, "Idle again with no raise: parked again");

    wake.raise();
    wake.raise();
    render(&mut exec, 1);
    assert_eq!(calls(), 3, "two raises before a block wake it once");
    render(&mut exec, 1);
    assert_eq!(calls(), 3);
}

/// **With no raise, a parked node is never called**: the control that makes
/// the wake test able to fail.
#[test]
fn a_parked_node_nobody_wakes_is_never_called_again() {
    let (_ed, mut exec, placements) = one_napper();
    let (_wake, calls) = placed(&placements, 0);
    for _ in 0..50 {
        render(&mut exec, 1);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1, "called once, then parked");
}

/// **A raise on a node that is not parked is consumed by the block that
/// calls it**: it does not wake the node again when it parks afterwards.
#[test]
fn a_raise_on_an_unparked_node_does_not_wake_it_after_it_parks() {
    let (_ed, mut exec, placements) = one_napper();
    let (wake, calls) = placed(&placements, 0);
    // Raised before the first block: the node has never parked.
    wake.raise();
    render(&mut exec, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    for _ in 0..5 {
        render(&mut exec, 1);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1, "the raise did not leak");
}

/// **A fork's node has a `Wake` of its own**: a raise on the live node's wakes
/// the live node alone, and a raise on the fork's wakes the fork's alone.
#[test]
fn a_raise_on_the_live_wake_does_not_wake_the_forks_node() {
    let (live_ed, mut live, placements) = one_napper();
    let (mut fork_ed, mut fork) = live_ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(BLOCK))
        .expect("the napper forks by clone");
    fork.apply_pending();
    fork_ed.collect();
    assert_eq!(placements.lock().unwrap().len(), 2, "live, then the fork");
    let ((live_wake, live_calls), (fork_wake, fork_calls)) =
        (placed(&placements, 0), placed(&placements, 1));
    let calls = |c: &Arc<AtomicUsize>| c.load(Ordering::Relaxed);

    render(&mut live, 1);
    render(&mut fork, 1);
    assert_eq!((calls(&live_calls), calls(&fork_calls)), (1, 1));

    live_wake.raise();
    render(&mut live, 1);
    render(&mut fork, 1);
    assert_eq!(
        (calls(&live_calls), calls(&fork_calls)),
        (2, 1),
        "the live raise woke the live node only"
    );

    fork_wake.raise();
    render(&mut live, 1);
    render(&mut fork, 1);
    assert_eq!(
        (calls(&live_calls), calls(&fork_calls)),
        (2, 2),
        "the fork's raise woke the fork's node only"
    );
}

/// The raise schedule of the parallel test: before which blocks each of the
/// three nappers is raised.
const RAISES: [&[usize]; 3] = [&[3, 4, 9], &[1, 6], &[]];
const BLOCKS: usize = 14;

/// Renders `BLOCKS` blocks of three nappers and a pulse, raising per
/// `RAISES` between blocks: what each block's output was, and which nappers
/// each block called.
fn run(workers: usize) -> (Vec<Vec<f32>>, Vec<[usize; 3]>) {
    let placements = Placements::default();
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    let keys = [NodeKey(10), NodeKey(11), NodeKey(12)];
    for key in keys {
        ed.insert(key, "napper", Unforkable(Napper::new(&placements)));
    }
    let pulse = NodeKey(20);
    ed.insert(pulse, "pulse", Unforkable(Pulse::new(5)));
    let outputs: Vec<Source> = keys
        .iter()
        .chain([&pulse])
        .map(|&node| Source::Node(OutPort { node, port: 0 }))
        .collect();
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);

    let nappers: Vec<_> = (0..3).map(|n| placed(&placements, n)).collect();
    let mut seen = [0usize; 3];
    let (mut audio, mut called) = (vec![Vec::new(); 4], Vec::new());
    for block in 0..BLOCKS {
        for (n, (wake, _)) in nappers.iter().enumerate() {
            if RAISES[n].contains(&block) {
                wake.raise();
            }
        }
        let mut bufs = vec![vec![0.0f32; BLOCK]; 4];
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
        exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
        for (a, b) in audio.iter_mut().zip(bufs) {
            a.extend(b);
        }
        let now: [usize; 3] = std::array::from_fn(|n| nappers[n].1.load(Ordering::Relaxed));
        called.push(std::array::from_fn(|n| now[n] - seen[n]));
        seen = now;
    }
    (audio, called)
}

/// **The serial and the parallel executors make the same park decision for
/// the same raises between blocks**: each block calls the same nappers, and
/// the render is bit for bit the same.
#[test]
fn serial_and_parallel_renders_with_raises_between_blocks_are_bit_identical() {
    let (serial_audio, serial_calls) = run(1);
    // Block 0 calls every napper; then only a raised one, on the block
    // after its raise.
    let expect: Vec<[usize; 3]> = (0..BLOCKS)
        .map(|b| std::array::from_fn(|n| usize::from(b == 0 || RAISES[n].contains(&b))))
        .collect();
    assert_eq!(serial_calls, expect, "the serial park decision");
    assert!(
        serial_audio[3].iter().any(|&x| x != 0.0),
        "the render is not silent"
    );
    for workers in WORKERS {
        let (audio, calls) = run(workers);
        assert_eq!(calls, serial_calls, "{workers} workers: calls per block");
        assert_eq!(bits(&audio), bits(&serial_audio), "{workers} workers");
    }
}
