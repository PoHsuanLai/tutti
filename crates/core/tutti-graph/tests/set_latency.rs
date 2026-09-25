//! `Editor::set_latency`: a runtime latency change — a plugin whose latency
//! atomic moved — recompiles and moves PDC, without touching the unit.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{prepare, Kind, TestNode};
use tutti_graph::{CommitError, Editor, Executor, Legacy, Node, Reference, Transport};
use tutti_node::AudioUnit;
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::latency::MAX_NODE_LATENCY;
use tutti_types::{ChannelLayout, Latency, NodeKey, Samples};

/// The delay the plugin actually runs.
const TRUE_LATENCY: usize = 16;

/// A plugin that delays its input by [`TRUE_LATENCY`] frames all along, but
/// *reports* whatever its latency cell holds — 0 at load, as a plugin that
/// only learns its latency after activation does. Counts its calls, so a
/// replacement would show.
#[derive(Clone)]
struct Plugin {
    reported: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    ring: [f32; TRUE_LATENCY],
    pos: usize,
}

impl Plugin {
    fn new() -> Self {
        Self {
            reported: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            ring: [0.0; TRUE_LATENCY],
            pos: 0,
        }
    }
}

impl AudioUnit for Plugin {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.ring[self.pos];
        self.ring[self.pos] = input[0];
        self.pos = (self.pos + 1) % TRUE_LATENCY;
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        for i in 0..size {
            let x = input.channel_f32(0)[i];
            output.channel_f32_mut(0)[i] = self.ring[self.pos];
            self.ring[self.pos] = x;
            self.pos = (self.pos + 1) % TRUE_LATENCY;
        }
    }
    fn reset(&mut self) {
        self.ring = [0.0; TRUE_LATENCY];
        self.pos = 0;
    }
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(
        &mut self,
        _input: &tutti_node::signal::SignalFrame,
        _frequency: f64,
    ) -> tutti_node::signal::SignalFrame {
        let mut out = tutti_node::signal::SignalFrame::new(1);
        out.set(
            0,
            tutti_node::signal::Signal::Latency(self.reported.load(Ordering::Relaxed) as f64),
        );
        out
    }
    fn get_id(&self) -> u64 {
        0x504c_5547
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        0
    }
}

const PLUGIN: NodeKey = NodeKey(1);
const DRY: NodeKey = NodeKey(2);

/// Global input → `PLUGIN` → output 0, and global input → a unity gain
/// (`DRY`, latency 0) → output 1. The two outputs are aligned by PDC.
fn graph(plugin: Plugin) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(prepare(64));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(PLUGIN, "plugin", Legacy::new(plugin));
    ed.insert(
        DRY,
        "gain",
        TestNode::new(Kind::Gain {
            gain: 1.0,
            width: 1,
        }),
    );
    let t = &mut ed.spec_mut().topology;
    for k in [PLUGIN, DRY] {
        t.edges
            .insert(InPort { node: k, port: 0 }, Edge::Direct(Source::Global(0)));
    }
    t.outputs = vec![
        Source::Node(OutPort {
            node: PLUGIN,
            port: 0,
        }),
        Source::Node(OutPort { node: DRY, port: 0 }),
    ];
    (ed, exec)
}

/// Units for a [`Reference`] of the same graph.
fn fresh(plugin: Plugin) -> BTreeMap<NodeKey, Box<dyn Node>> {
    let mut m: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
    m.insert(PLUGIN, Box::new(Legacy::new(plugin)));
    m.insert(
        DRY,
        Box::new(TestNode::new(Kind::Gain {
            gain: 1.0,
            width: 1,
        })),
    );
    m
}

/// One block of `input` through `exec` and `reference`; both outputs of
/// each, which must agree bit for bit.
fn block(exec: &mut Executor, reference: &mut Reference, input: &[f32]) -> [Vec<f32>; 2] {
    let n = input.len();
    let (mut a, mut b) = (vec![0.0f32; n], vec![0.0f32; n]);
    exec.process(n, &Transport::default(), &[input], &mut [&mut a, &mut b]);
    let (mut ra, mut rb) = (vec![0.0f32; n], vec![0.0f32; n]);
    reference.process(n, &Transport::default(), &[input], &mut [&mut ra, &mut rb]);
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&a), bits(&ra), "executor and reference, output 0");
    assert_eq!(bits(&b), bits(&rb), "executor and reference, output 1");
    [a, b]
}

/// A one at `at`, zeros elsewhere.
fn impulse(n: usize, at: usize) -> Vec<f32> {
    let mut v = vec![0.0; n];
    v[at] = 1.0;
    v
}

fn peak_at(v: &[f32]) -> Option<usize> {
    v.iter().position(|&x| x == 1.0)
}

/// **A latency change moves the other path's compensation on the next
/// commit, and nothing else.** The plugin reports 0 at load though it runs
/// 16 frames late, so its output trails the dry path by 16. `set_latency`
/// and a commit later, the dry path is delayed by 16 and the two line up;
/// the plugin's own output — whose compensation stays 0, since it is the
/// latest path — is bit-identical to a run that never changed anything (no
/// click), and the unit is the same one (never retired, called every block).
/// A [`Reference`] handed the same spec renders the same numbers.
///
/// Mutation: make `set_latency` return `Ok(())` without writing anything →
/// the dry path stays early → the alignment assertion fails. Mutation: write
/// only the spec's latency, not the shape → the commit is a
/// `LatencyMismatch` → fails. Mutation: read the reference's latency from
/// `unit.shape()` again (`reference.rs`) → the reference never delays the
/// dry path → the executor/reference comparison fails.
#[test]
fn a_latency_change_moves_the_other_paths_compensation() {
    let plugin = Plugin::new();
    let (reported, calls) = (Arc::clone(&plugin.reported), Arc::clone(&plugin.calls));
    let (mut ed, mut exec) = graph(plugin.clone());
    ed.commit().expect("commits");
    let mut reference = Reference::new(prepare(64));
    // Its own call counter, so `calls` counts the executor's unit alone.
    let mut ref_plugin = plugin.clone();
    ref_plugin.calls = Arc::new(AtomicUsize::new(0));
    reference.set_graph(&ed.spec().validate().expect("valid"), fresh(ref_plugin));

    // The untouched twin: the same graph, never changed.
    let (mut twin_ed, mut twin) = graph(Plugin::new());
    twin_ed.commit().expect("commits");
    let mut twin_ref = Reference::new(prepare(64));
    twin_ref.set_graph(
        &twin_ed.spec().validate().expect("valid"),
        fresh(Plugin::new()),
    );

    // Misaligned while the plugin under-reports.
    let [wet, dry] = block(&mut exec, &mut reference, &impulse(64, 8));
    let [twin_wet, _] = block(&mut twin, &mut twin_ref, &impulse(64, 8));
    assert_eq!(peak_at(&wet), Some(8 + TRUE_LATENCY));
    assert_eq!(peak_at(&dry), Some(8), "no compensation yet");
    assert_eq!(wet, twin_wet);

    // The plugin's latency atomic moves; the host tells the editor.
    reported.store(TRUE_LATENCY, Ordering::Relaxed);
    ed.set_latency(PLUGIN, Latency::new(Samples(TRUE_LATENCY)))
        .expect("the node exists");
    assert_eq!(
        ed.spec().topology.nodes[&PLUGIN].latency,
        Samples(TRUE_LATENCY)
    );
    ed.commit().expect("recompiles");
    reference.set_graph(&ed.spec().validate().expect("valid"), BTreeMap::new());

    let mut wet_all = wet;
    let mut twin_all = twin_wet;
    for b in 0..4 {
        let input = impulse(64, 20);
        let [wet, dry] = block(&mut exec, &mut reference, &input);
        let [twin_wet, _] = block(&mut twin, &mut twin_ref, &input);
        if b > 0 {
            assert_eq!(
                peak_at(&dry),
                peak_at(&wet),
                "block {b}: the dry path now lines up with the plugin"
            );
            assert_eq!(peak_at(&dry), Some(20 + TRUE_LATENCY));
        }
        wet_all.extend(wet);
        twin_all.extend(twin_wet);
    }
    assert_eq!(
        wet_all, twin_all,
        "the plugin's path is untouched by the recompile: no click"
    );
    assert!(ed.collect().is_empty(), "no unit was retired");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        5,
        "the same unit, every block"
    );
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().iter().any(|d| d.len == Samples(TRUE_LATENCY)),
        "a PDC delay of the new latency"
    );
}

/// Refusals: a key with no node, a latency past what PDC compensates, and
/// between a re-prepare's two commits.
///
/// Mutation: drop the `repreparing` check in `set_latency` → `Ok` → fails.
/// Mutation: drop the `MAX_NODE_LATENCY` check → `Ok` → fails.
#[test]
fn set_latency_refuses_what_it_cannot_honour() {
    let (mut ed, _exec) = graph(Plugin::new());
    ed.commit().expect("commits");
    assert_eq!(
        ed.set_latency(NodeKey(99), Latency::new(Samples(4))),
        Err(CommitError::NoSuchNode { node: NodeKey(99) })
    );
    // At the limit is fine; one past it is refused, not clamped, and
    // nothing is written.
    let limit = Latency::new(MAX_NODE_LATENCY);
    ed.set_latency(PLUGIN, limit).expect("at the limit");
    let past = Latency::new(Samples(MAX_NODE_LATENCY.get() + 1));
    assert_eq!(
        ed.set_latency(PLUGIN, past),
        Err(CommitError::LatencyTooLong {
            node: PLUGIN,
            latency: past,
            limit
        })
    );
    assert_eq!(ed.spec().topology.nodes[&PLUGIN].latency, MAX_NODE_LATENCY);
    ed.reprepare(prepare(128)).expect("re-prepares");
    assert_eq!(
        ed.set_latency(PLUGIN, Latency::new(Samples(4))),
        Err(CommitError::Repreparing)
    );
}
