//! **A live plugin keeps in step with the device.** The reference CLAP
//! plugin in a real `plugin-server`, rendered by tutti-core's `Engine` exactly
//! as a device drives it — one call per callback, the callbacks paced to real
//! time — at 480-, 1024- and 441-frame device quanta.
//!
//! Doc 013 reversed its decision 8: a live plugin's pipeline chunk is the
//! device's callback (`Prepare::quantum`), not a fixed 64 frames. With 64, a
//! chunk completed mid-callback was collected microseconds after it was
//! submitted, and read as silence; with the callback as the chunk, the server
//! has a whole device period to answer. Asserted here: at least 99% of the
//! blocks after warm-up carry audio, every block is either the input delayed
//! by exactly the declared latency (137 + quantum) or silence, and the graph
//! declares that latency.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv};

use std::time::{Duration, Instant};

use tutti_core::{ChannelLayout, Engine, InterleavedMut, Transport};
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{NodeKey, SampleRate, Samples, Tail};

/// What the probe declares and delays by (`REPORTED_LATENCY_SAMPLES`).
const PROBE_LATENCY: usize = 137;

/// Input frame `t`: distinct, never zero, exact in `f32`.
fn ramp(t: u64) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

/// The ramp as a graph source, read off each block's `Env`.
struct RampSource;

impl Node for RampSource {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let first = cx.env.frame.get();
        for (i, s) in io.output(0).iter_mut().enumerate() {
            *s = ramp(first + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// What a live run saw.
struct Run {
    declared: Samples,
    /// Blocks after warm-up.
    blocks: usize,
    /// Of those, entirely silent.
    silent: usize,
    /// Of those, neither silent nor the input delayed by `declared`.
    wrong: Vec<usize>,
}

/// Render `blocks` callbacks of `quantum` frames at `rate`, paced to real
/// time, through an engine whose graph is the ramp into the probe.
fn run_live(rate: f64, quantum: usize, blocks: usize) -> Run {
    let probe = load_probe(rate);
    let inputs = probe.client.inputs();
    let prepare = Prepare::new(SampleRate(rate), Samples(quantum)).with_quantum(Samples(quantum));
    let (mut ed, exec) = Editor::new(prepare);
    let (src, key) = (NodeKey(1), NodeKey(2));
    ed.insert(src, "ramp", RampSource);
    let _controls = ed.insert(key, "plugin", probe.client.bind());
    for port in 0..inputs {
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: key,
                port: port as u16,
            },
            Edge::Direct(Source::Node(OutPort { node: src, port: 0 })),
        );
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("the graph compiles");
    let declared = ed.spec().topology.nodes[&key].latency;

    let transport = Transport::new(rate);
    let engine = Engine::new(&transport, &mut ed, exec).expect("the engine builds");
    let period = Duration::from_secs_f64(quantum as f64 / rate);
    let warm_up = 8 + declared.get().div_ceil(quantum);
    let mut buf = vec![0.0f32; quantum];
    let (mut silent, mut wrong) = (0, Vec::new());
    let mut next = Instant::now();
    for b in 0..warm_up + blocks {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
        ed.collect();
        if b >= warm_up {
            let first = (b * quantum) as u64;
            if buf.iter().all(|&s| s == 0.0) {
                silent += 1;
            } else if buf
                .iter()
                .enumerate()
                .any(|(i, &s)| s != ramp(first + i as u64 - declared.get() as u64))
            {
                wrong.push(b);
            }
        }
        next += period;
        if let Some(rest) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
    }
    drop(probe.handle);
    Run {
        declared,
        blocks,
        silent,
        wrong,
    }
}

fn assert_live(rate: f64, quantum: usize) {
    let run = run_live(rate, quantum, 200);
    eprintln!(
        "{quantum}-frame callbacks: declared {:?}, {} of {} blocks silent, {} wrong",
        run.declared,
        run.silent,
        run.blocks,
        run.wrong.len()
    );
    assert!(
        run.silent * 100 <= run.blocks,
        "{quantum}-frame callbacks: {} of {} blocks were silent — the server was \
         not given the device period to answer",
        run.silent,
        run.blocks
    );
    assert!(
        run.wrong.is_empty(),
        "{quantum}-frame callbacks: blocks {:?} are neither silent nor the input \
         delayed by {:?}",
        run.wrong,
        run.declared
    );
    assert_eq!(
        run.declared,
        Samples(PROBE_LATENCY + quantum),
        "a {quantum}-frame device: the plugin's latency plus one callback"
    );
}

/// 480-frame callbacks at 48 kHz: not a multiple of 64, so the reversed
/// 64-frame pipeline collected most chunks mid-callback.
///
/// Mutation: cap the chunk at 64 in `Batcher::prepare` (the reversed
/// decision) → most blocks are silent → fails (measured: see doc 013).
/// Ignoring `Prepare::quantum` is invisible here (the graph's `MaxBlock` is
/// the same 480); the batcher's unit test pins it.
#[test]
fn a_480_frame_device_hears_its_plugin() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(48_000.0, 480);
}

/// 1024-frame callbacks at 48 kHz: sixteen 64-frame passes per callback, of
/// which the reversed pipeline could fill only the first.
#[test]
fn a_1024_frame_device_hears_its_plugin() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(48_000.0, 1024);
}

/// 441-frame callbacks at 44.1 kHz (10 ms): odd, so no power-of-two chunk
/// could line up with it.
#[test]
fn a_441_frame_device_hears_its_plugin() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(44_100.0, 441);
}
