//! `GraphBuilder`: what it builds is what a host writing the `GraphSpec` by
//! hand builds, and its fan-out calls mean exactly what fundsp's `Net` calls
//! of the same name mean — which is what lets doc 013's PR 8 port the
//! `Net` fixtures call for call.

mod common;

use std::sync::{Arc, Mutex};

use common::{prepare, Kind, TestNode};
use fundsp::net::{Net, NodeId, Source as NetSource};
use fundsp::prelude32::{lowpass_hz, mul, pass};
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, GraphBuilder, Renderer, Transport};
use tutti_node::buffer::BufferVec;
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{Beat, ChannelLayout, Frame, NodeKey, SampleRate, Samples};

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 48.0)
        .collect()
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// `ins` inputs, `outs` outputs; output `c` is `c + 1` plus the sum of the
/// inputs. Any width, so the fan-out rules can be walked over a grid —
/// fundsp's own units fix their width in the type.
#[derive(Clone)]
struct Width {
    ins: usize,
    outs: usize,
}

impl AudioUnit for Width {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let sum: f32 = input.iter().sum();
        for (c, o) in output.iter_mut().enumerate() {
            *o = (c + 1) as f32 + sum;
        }
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        for i in 0..size {
            let sum: f32 = (0..self.ins).map(|c| input.channel_f32(c)[i]).sum();
            for c in 0..self.outs {
                output.channel_f32_mut(c)[i] = (c + 1) as f32 + sum;
            }
        }
    }
    fn inputs(&self) -> usize {
        self.ins
    }
    fn outputs(&self) -> usize {
        self.outs
    }
    fn route(
        &mut self,
        _input: &tutti_node::signal::SignalFrame,
        _frequency: f64,
    ) -> tutti_node::signal::SignalFrame {
        let mut out = tutti_node::signal::SignalFrame::new(self.outs);
        for c in 0..self.outs {
            out.set(c, tutti_node::signal::Signal::Latency(0.0));
        }
        out
    }
    fn get_id(&self) -> u64 {
        0x5749_4454
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

fn width(ins: usize, outs: usize) -> Box<dyn AudioUnit> {
    Box::new(Width { ins, outs })
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(n as u16)
}

/// `Net`'s source, in `Topology` terms, given which `NodeId` is which key.
fn from_net(s: NetSource, ids: &[(NodeId, NodeKey)]) -> Source {
    match s {
        NetSource::Local(id, port) => Source::Node(OutPort {
            node: ids.iter().find(|(i, _)| *i == id).expect("known id").1,
            port: port as u16,
        }),
        NetSource::Global(i) => Source::Global(i as u16),
        NetSource::Zero => Source::Zero,
    }
}

/// What the builder feeds `node`'s input `port`. An absent edge reads
/// silence, as `Net`'s default `Zero` does.
fn source_of(g: &GraphBuilder, node: NodeKey, port: u16) -> Source {
    match g.spec().topology.edges.get(&InPort { node, port }) {
        Some(Edge::Direct(s)) => *s,
        Some(Edge::Feedback(_)) => panic!("no feedback in these graphs"),
        None => Source::Zero,
    }
}

/// Every input of every node, and every global output, is fed from the
/// same place in both.
fn assert_wired_like(net: &Net, g: &GraphBuilder, ids: &[(NodeId, NodeKey)], case: &str) {
    for &(id, key) in ids {
        for c in 0..net.inputs_in(id) {
            assert_eq!(
                source_of(g, key, c as u16),
                from_net(net.source(id, c), ids),
                "{case}: {key:?} input {c}"
            );
        }
    }
    assert_eq!(net.outputs(), g.outputs(), "{case}: global outputs");
    for c in 0..net.outputs() {
        assert_eq!(
            g.spec().topology.outputs[c],
            from_net(net.output_source(c), ids),
            "{case}: global output {c}"
        );
    }
}

/// Render `net` for `frames` of silent input, in 64-frame chunks, planar.
fn render_net(net: &mut Net, frames: usize) -> Vec<Vec<f32>> {
    net.set_sample_rate(SampleRate(48_000.0));
    net.allocate();
    let ibuf = BufferVec::new(net.inputs());
    let mut obuf = BufferVec::new(net.outputs());
    let mut out = vec![Vec::with_capacity(frames); net.outputs()];
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(MAX_BUFFER_SIZE);
        net.process(n, &ibuf.buffer_ref(), &mut obuf.buffer_mut());
        for (c, o) in out.iter_mut().enumerate() {
            o.extend_from_slice(&obuf.channel_f32_mut(c)[..n]);
        }
        done += n;
    }
    out
}

/// The same graph written twice — through the builder, and by hand through
/// `Editor::insert` and `spec_mut` — gives equal specs, equal plans and a
/// bit-identical render. It covers every kind of wiring the builder writes:
/// a global input, a node-to-node edge, a feedback edge, a global output
/// fed by `set_output` and one by `connect_output`, and an event edge.
///
/// Mutation: drop the `outputs` copy in `GraphBuilder::build` → every
/// output is `Zero` → specs differ → fails. Mutation: write
/// `Edge::Direct(Source::Node(from))` in `feedback` → a cycle → build
/// fails. Mutation: drop the `events` copy in `build` → the event edge is
/// gone → specs differ → fails.
#[test]
fn builder_builds_what_a_hand_written_spec_builds() {
    let pre = prepare(64);
    let fb_delay = Samples(64);

    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
    let mix = g.add_unit(Box::new(pass() + mul(0.5)));
    let lp = g.add_unit(Box::new(lowpass_hz(1_200.0, 0.9)));
    let emit = g.add(TestNode::new(Kind::Emitter {
        period: 50,
        phase: 7,
    }));
    let fold = g.add(TestNode::new(Kind::Consumer { inputs: 1 }));
    g.connect_input(0, mix, 0)
        .feedback(lp, 0, mix, 1, fb_delay)
        .connect(mix, 0, lp, 0)
        .set_output(0, Source::Node(OutPort { node: lp, port: 0 }))
        .connect_output(fold, 0, 1)
        .event_connect(emit, 0, fold, 0);
    assert_eq!(
        [mix, lp, emit, fold],
        [NodeKey(0), NodeKey(1), NodeKey(2), NodeKey(3)],
        "keys in the order nodes were added"
    );
    let mut built = g.renderer(pre).expect("builds");

    let (mut ed, exec) = Editor::new(pre);
    let test_node = std::any::type_name::<TestNode>();
    ed.insert(
        NodeKey(0),
        "legacy",
        tutti_graph::Legacy::new(pass() + mul(0.5)),
    );
    ed.insert(
        NodeKey(1),
        "legacy",
        tutti_graph::Legacy::new(lowpass_hz(1_200.0, 0.9)),
    );
    ed.insert(
        NodeKey(2),
        test_node,
        TestNode::new(Kind::Emitter {
            period: 50,
            phase: 7,
        }),
    );
    ed.insert(
        NodeKey(3),
        test_node,
        TestNode::new(Kind::Consumer { inputs: 1 }),
    );
    let spec = ed.spec_mut();
    spec.topology.inputs = ChannelLayout::MONO;
    let at = |node: u64, port: u16| InPort {
        node: NodeKey(node),
        port,
    };
    let from = |node: u64, port: u16| OutPort {
        node: NodeKey(node),
        port,
    };
    spec.topology
        .edges
        .insert(at(0, 0), Edge::Direct(Source::Global(0)));
    spec.topology.edges.insert(
        at(0, 1),
        Edge::Feedback(FeedbackFrom::new(from(1, 0), fb_delay)),
    );
    spec.topology
        .edges
        .insert(at(1, 0), Edge::Direct(Source::Node(from(0, 0))));
    spec.topology.outputs = vec![Source::Node(from(1, 0)), Source::Node(from(3, 0))];
    spec.connect_events(
        EventIn {
            node: NodeKey(3),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(2),
            port: 0,
        }),
    );
    ed.commit().expect("commits");
    let mut by_hand = Renderer::new(ed, exec);
    by_hand.render(1); // installs the plan the builder installed eagerly

    assert_eq!(built.editor().spec(), by_hand.editor().spec());
    assert_eq!(
        **built.executor().plan().expect("installed"),
        **by_hand.executor().plan().expect("installed")
    );

    // The hand-built side has rendered one silent frame to install its
    // plan; render the same frame on the built side, so both clocks (and so
    // the emitter's phase) line up before the comparison.
    built.render_input(&[&[0.0]]);
    let input = signal(2_000);
    let a = built.render_input(&[&input]);
    let b = by_hand.render_input(&[&input]);
    assert_eq!(a.len(), 2);
    for c in 0..2 {
        assert_eq!(bits(&a[c]), bits(&b[c]), "channel {c}");
        assert!(a[c].iter().any(|&x| x != 0.0), "channel {c} is not silent");
    }
}

/// `pipe` connects ports in order, with `Net::pipe_all`'s fan-out — over a
/// grid of widths, including a source with no outputs, and checked against
/// `Net` itself.
///
/// Mutation: `c % outs` → `(outs - 1).min(c)` (clamp) in `pipe` → mono
/// source fine, 2 → 3 differs at input 2 → fails. Mutation: reverse the
/// port order → fails at the first 2-wide case.
#[test]
fn pipe_connects_ports_in_order_as_net_pipe_all() {
    for outs in 0..=3 {
        for ins in 1..=3 {
            let case = format!("{outs} outputs → {ins} inputs");
            let mut net = Net::new(0, 1);
            let (na, nb) = (net.push(width(0, outs)), net.push(width(ins, 1)));
            net.pipe_all(na, nb);
            net.pipe_output(nb);

            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
            let (a, b) = (g.add_unit(width(0, outs)), g.add_unit(width(ins, 1)));
            g.pipe(a, b).pipe_output(b);
            assert_wired_like(&net, &g, &[(na, a), (nb, b)], &case);

            // And it sounds the same.
            let want = render_net(&mut net, 100);
            let got = g.renderer(prepare(64)).expect("builds").render(100);
            assert_eq!(got, want, "{case}");
        }
    }
    // The plain case, spelled out: port c → port c.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (a, b) = (g.add_unit(width(0, 3)), g.add_unit(width(3, 1)));
    g.pipe(a, b);
    for c in 0..3 {
        assert_eq!(
            source_of(&g, b, c),
            Source::Node(OutPort { node: a, port: c })
        );
    }
}

/// `pipe_output` fans a node out over the global outputs exactly as
/// `Net::pipe_output` does: output `c` reads port `c % width`. Mono feeds
/// every channel; stereo into six **wraps** (L R L R L R), it does not clamp
/// to the last channel; a wider node's extra ports go unused; a node with no
/// outputs feeds silence. Checked structurally and by render against `Net`.
///
/// Mutation: clamp (`c.min(outs - 1)`) instead of `c % outs` → stereo into
/// six differs from `Net` → fails. Mutation: feed a zero-output node's
/// channels from port 0 → out of range → panics.
#[test]
fn pipe_output_fans_out_as_net_does() {
    for (w, outs) in [(0, 2), (1, 1), (1, 2), (1, 6), (2, 6), (3, 2), (6, 2)] {
        let case = format!("{w}-wide node → {outs} outputs");
        let mut net = Net::new(0, outs);
        let id = net.push(width(0, w));
        net.pipe_output(id);

        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(outs));
        let key = g.add_unit(width(0, w));
        g.pipe_output(key);
        assert_wired_like(&net, &g, &[(id, key)], &case);

        let want = render_net(&mut net, 70);
        let got = g.renderer(prepare(64)).expect("builds").render(70);
        assert_eq!(got, want, "{case}");
    }
    // Stereo into six, spelled out.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(6));
    let key = g.add_unit(width(0, 2));
    g.pipe_output(key);
    let ports: Vec<Source> = (0..6)
        .map(|c| {
            Source::Node(OutPort {
                node: key,
                port: c % 2,
            })
        })
        .collect();
    assert_eq!(g.spec().topology.outputs, ports);
}

/// `pipe_input` reads global input `c % inputs`, and silence with no global
/// inputs — `Net::pipe_input`.
///
/// Mutation: `Source::Global(c)` without the modulo → out of range → panics
/// at 1 global input into 2 ports → fails. (Explicit `Zero` against an
/// absent edge with no globals is not checked: the two read the same
/// silence, and `source_of` maps absent to `Zero` on purpose.)
#[test]
fn pipe_input_wraps_as_net_does() {
    for globals in 0..=3 {
        for ins in 1..=3 {
            let case = format!("{globals} globals → {ins} inputs");
            let mut net = Net::new(globals, 1);
            let id = net.push(width(ins, 1));
            net.pipe_input(id);
            net.pipe_output(id);

            let mut g = GraphBuilder::new(ch(globals), ChannelLayout::MONO);
            let key = g.add_unit(width(ins, 1));
            g.pipe_input(key).pipe_output(key);
            assert_wired_like(&net, &g, &[(id, key)], &case);
        }
    }
}

/// `chain` extends a series as `Net::chain` does: the first node reads the
/// global inputs, each later one reads what fed the global outputs
/// (wrapping), and every one takes over the outputs. Across width changes,
/// with and without global inputs, and bit-identical in render.
///
/// Mutation: skip `pipe_input` for the first node in `link` → it reads
/// silence where `Net` reads the global input → fails. Mutation: read
/// `outputs[c]` without the modulo → the 2-input node after a mono-output
/// graph is out of range → panics → fails.
#[test]
fn chain_extends_the_series_as_net_does() {
    for globals in [0, 1, 2] {
        let widths = [(1, 1), (1, 3), (2, 2), (3, 1), (1, 2)];
        let case = format!("{globals} globals");
        let mut net = Net::new(globals, 2);
        let mut g = GraphBuilder::new(ch(globals), ChannelLayout::STEREO);
        let mut ids = Vec::new();
        for (ins, outs) in widths {
            ids.push((net.chain(width(ins, outs)), g.chain_unit(width(ins, outs))));
        }
        assert_wired_like(&net, &g, &ids, &case);

        let want = render_net(&mut net, 70);
        let got = g.renderer(prepare(64)).expect("builds").render(70);
        assert_eq!(got, want, "{case}");
    }
}

/// `chain` takes native nodes too, and `add_with_controls` hands back what
/// `IntoNode` returns.
///
/// Mutation: skip `pipe_input` for the first node in `link` → the chain
/// reads silence → 0 instead of 6 → fails.
#[test]
fn chain_and_add_take_native_nodes() {
    struct Tagged;
    impl tutti_graph::IntoNode for Tagged {
        type Controls = u32;
        fn into_node(self) -> (Box<dyn tutti_graph::Node>, u32) {
            (
                Box::new(TestNode::new(Kind::Gain {
                    gain: 2.0,
                    width: 1,
                })),
                7,
            )
        }
    }
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let first = g.chain(TestNode::new(Kind::Gain {
        gain: 3.0,
        width: 1,
    }));
    let (second, controls) = g.add_with_controls(Tagged);
    g.pipe(first, second).pipe_output(second);
    assert_eq!((first, second, controls), (NodeKey(0), NodeKey(1), 7));
    let out = g
        .renderer(prepare(64))
        .expect("builds")
        .render_input(&[&[1.0; 10]]);
    assert_eq!(out, vec![vec![6.0; 10]]);
}

/// An out-of-range port is a panic at the call, as in `Net`, not a graph
/// that fails later.
///
/// Mutation: drop the assert in `check_in` → the edge is written and
/// nothing panics → fails.
#[test]
#[should_panic(expected = "port 1 is out of range")]
fn an_out_of_range_port_panics_at_the_call() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (a, b) = (g.add_unit(width(0, 1)), g.add_unit(width(1, 1)));
    g.connect(a, 0, b, 1);
}

/// A graph the editor refuses is refused by `build` with the editor's own
/// error: a feedback delay shorter than the block.
///
/// Mutation: `build` ignores `commit`'s result → `Ok` → fails.
#[test]
fn build_returns_the_editors_error() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let a = g.add_unit(width(1, 1));
    g.feedback(a, 0, a, 0, Samples(16)).pipe_output(a);
    assert!(matches!(
        g.build(prepare(64)),
        Err(tutti_graph::CommitError::Compile(
            tutti_graph::CompileError::FeedbackTooShort { .. }
        ))
    ));
}

/// The renderer asks for the transport once per block, at the block's first
/// frame, in blocks of the chosen length (the last one short), and
/// interleaves frame by frame.
///
/// Mutation: pass `Frame(0)` to the transport function → frames differ →
/// fails. Mutation: interleave channel-major → fails. Mutation: ignore
/// `set_block` → one 250-frame block → fails.
#[test]
fn renderer_drives_blocks_and_interleaves() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let key = g.add_unit(width(0, 2));
    g.pipe_output(key);
    let mut r = g.renderer(prepare(256)).expect("builds");
    r.set_block(Samples(100));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    r.set_transport_fn(move |frame| {
        log.lock().unwrap().push(frame);
        Transport {
            playing: true,
            beat: Beat(frame.0 as f64),
            ..Transport::default()
        }
    });
    let out = r.render_interleaved(250);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Frame(0), Frame(100), Frame(200)]
    );
    assert_eq!(out.len(), 500);
    assert!(out.chunks(2).all(|f| f == [1.0, 2.0]));

    // `render_into` writes where it is told, and advances the same clock.
    let (mut l, mut rr) = (vec![0.0; 30], vec![0.0; 30]);
    r.render_into(&mut [&mut l, &mut rr]);
    assert_eq!((l, rr), (vec![1.0; 30], vec![2.0; 30]));
    assert_eq!(r.executor().frame(), Frame(280));
}
