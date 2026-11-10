//! An export on a worker pool renders bit for bit what the serial export
//! does (doc 013, "The parallel executor").
//!
//! The graph is a fan-out: eight independent branches (each a sine of its own
//! frequency with its own phase, Heavy because they declare no cost) summed in
//! a fixed order by one node. Exported to memory with
//! `render_workers(1)` (no pool: the caller alone) and `render_workers(4)`
//! (the export's own pool), the planes must be the same bits.
//!
//! The no-allocation gate of the parallel path is not applied here, on
//! purpose: the export builds its pool inside the session (`Graph::render_pool`
//! from `render_workers`), so a test cannot hand it a gated one, and
//! `GraphSource::fill` allocates its slice lists every block by design (the
//! executor's own walk inside `render_graph` is what stays allocation-free).
//! The gate that runs every participant inside `assert_no_alloc`, helpers
//! included, is `tutti-engine`'s `rt_no_alloc_engine` (`GatedPool`), over the
//! same executor.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use tutti_export::{
    ChannelLayout, EncodeConfig, ExportConfig, Format, Outcome, Pcm, RenderConfig, Rendered,
};
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{
    ExportClock, ExportOutput, ExportRequest, ExportSource, ExportTarget, GraphSource, Session,
};
use tutti_types::SampleRate;

const RATE: SampleRate = SampleRate(48_000.0);
const BRANCHES: usize = 8;

type Threads = Arc<Mutex<HashSet<ThreadId>>>;

/// A mono sine with its own phase, a few harmonics deep so a block costs
/// enough for a helper to take it, and a record of the threads that ran it.
#[derive(Clone)]
struct Branch {
    hz: f32,
    phase: f32,
    dt: f32,
    threads: Threads,
}

impl Node for Branch {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.dt = self.hz / p.sample_rate().get() as f32;
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.threads
            .lock()
            .unwrap()
            .insert(std::thread::current().id());
        for s in io.output(0).iter_mut() {
            let mut v = 0.0f32;
            for k in 1..=8 {
                v += (std::f32::consts::TAU * self.phase * k as f32).sin() / k as f32;
            }
            *s = 0.05 * v;
            self.phase = (self.phase + self.dt).fract();
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// The inputs summed in index order: `((a + b) + c) + ...`.
#[derive(Clone)]
struct Sum;

impl Node for Sum {
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::try_from(BRANCHES).expect("a few channels"),
            ChannelLayout::MONO,
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        let out = outs.get(0);
        out.copy_from_slice(ins.get(0));
        for c in 1..BRANCHES {
            for (o, i) in out.iter_mut().zip(ins.get(c)) {
                *o += i;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The fan-out exported to memory on a session with `workers`, and the number
/// of distinct threads that ran a branch.
fn export(workers: usize) -> (Rendered, usize) {
    let mut session = Session::builder()
        .render_workers(workers)
        .build_detached(RATE)
        .unwrap();
    let threads = Threads::default();
    let (sum, _) = session.graph().add(ForkByClone(Sum));
    for i in 0..BRANCHES {
        let (branch, _) = session.graph().add(ForkByClone(Branch {
            hz: 110.0 * (i + 1) as f32 + 0.37 * i as f32,
            phase: 0.0,
            dt: 0.0,
            threads: Arc::clone(&threads),
        }));
        session
            .graph()
            .set_source(sum, i, GraphSource::Node(branch, 0));
    }
    session.graph().set_outputs_from(sum);

    let config = ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: 0.5,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Float32),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    };
    let request = ExportRequest::new(
        ExportSource::Mix,
        ExportTarget::Buffers,
        config,
        ExportClock::frozen(),
    );
    let Outcome::Finished(ExportOutput::Buffers(rendered)) =
        session.export_blocking(request).unwrap()
    else {
        panic!("the export finishes into memory")
    };
    let ran_on = threads.lock().unwrap().len();
    (rendered, ran_on)
}

/// The export on four workers is the export on one, to the bit, and it did
/// run on more than one thread (so the comparison is of a parallel render).
///
/// Mutation (run): `Sum` adding its inputs in reverse index order on the
/// export's pool threads only (a branch on the thread's name) → the planes
/// differ → fails. The thread-count assertions are what make the comparison one
/// of a serial against a parallel render: with no pool, `ran_on` is 1 for
/// both.
#[test]
fn a_parallel_export_is_bit_identical_to_the_serial_one() {
    let (serial, serial_threads) = export(1);
    let (parallel, parallel_threads) = export(4);

    assert_eq!(serial_threads, 1, "one worker renders on the caller alone");
    assert!(
        parallel_threads > 1,
        "four workers must spread the branches over threads, ran on {parallel_threads}"
    );

    assert_eq!(serial.frames(), parallel.frames());
    assert_eq!(serial.frames().get(), 24_000);
    assert_eq!(serial.channels(), 2);
    let silent = serial.planes[0].iter().all(|s| *s == 0.0);
    assert!(!silent, "the fan-out is audible");
    for (c, (a, b)) in serial.planes.iter().zip(&parallel.planes).enumerate() {
        let (a, b): (Vec<u32>, Vec<u32>) = (
            a.iter().map(|s| s.to_bits()).collect(),
            b.iter().map(|s| s.to_bits()).collect(),
        );
        assert!(
            a == b,
            "channel {c} differs between the serial and parallel render"
        );
    }
}
