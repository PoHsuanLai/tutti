//! `GraphBuilder`: what it builds is what a host writing the `GraphSpec` by
//! hand builds, and its fan-out calls mean exactly what fundsp's `Net` calls
//! of the same name meant — which is what let doc 013's PR 8 port the `Net`
//! fixtures call for call. `Net` itself is gone (doc 013 Phase 5), so the
//! fan-out rules it followed are written out below ([`net_rule`]) and each
//! render is pinned to the figure those rules give in closed form.

mod common;

use std::sync::{Arc, Mutex};

use common::{prepare, Kind, TestNode};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventOut, ForkByClone, GraphBuilder, Io, Node, Prepare,
    Renderer, Shape, Status, Transport, Unforkable,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{Beat, ChannelLayout, Frame, NodeKey, Samples};

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 48.0)
        .collect()
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// `ins` inputs, `outs` outputs; output `c` is `c + 1` plus the sum of the
/// inputs. Any width, so the fan-out rules can be walked over a grid, and
/// every output is a small integer while the global inputs are silent, so
/// what a wiring renders is known exactly ([`net_rule::constants`]).
#[derive(Clone)]
struct Width {
    ins: usize,
    outs: usize,
}

impl Node for Width {
    fn shape(&self) -> Shape {
        Shape::audio(ch(self.ins), ch(self.outs))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        let (ins, mut outs) = io.split();
        for i in 0..frames {
            let sum: f32 = (0..self.ins).map(|c| ins.get(c)[i]).sum();
            for c in 0..self.outs {
                outs.get(c)[i] = (c + 1) as f32 + sum;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// [`Width`] as a graph node.
fn node(ins: usize, outs: usize) -> ForkByClone<Width> {
    ForkByClone(Width { ins, outs })
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(n as u16)
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

/// Every input of every node, and every global output, is fed from where
/// `want` (one entry per node, in the order added: its width and what feeds
/// each input) and `outputs` say.
fn assert_wired(
    g: &GraphBuilder,
    want: &[(NodeKey, net_rule::Node)],
    outputs: &[Source],
    case: &str,
) {
    for (key, n) in want {
        for (c, s) in n.sources.iter().enumerate() {
            assert_eq!(
                source_of(g, *key, c as u16),
                *s,
                "{case}: {key:?} input {c}"
            );
        }
    }
    assert_eq!(g.spec().topology.outputs, outputs, "{case}: global outputs");
}

/// Assert `got` holds, on every frame, the constant each output channel of
/// `want`'s wiring carries.
fn assert_renders(
    got: &[Vec<f32>],
    want: &[(NodeKey, net_rule::Node)],
    outputs: &[Source],
    frames: usize,
    case: &str,
) {
    let values = net_rule::constants(want, outputs);
    assert_eq!(got.len(), values.len(), "{case}: channel count");
    for (c, (ch, v)) in got.iter().zip(&values).enumerate() {
        assert_eq!(ch.len(), frames, "{case}: channel {c} length");
        assert!(
            ch.iter().all(|x| x.to_bits() == v.to_bits()),
            "{case}: channel {c} should be {v} on every frame, got {:?}",
            &ch[..ch.len().min(4)]
        );
    }
}

/// fundsp's `Net` fan-out rules, as `fundsp-tutti`'s `net.rs` wrote them
/// before doc 013 Phase 5 deleted it, over [`Width`] nodes.
mod net_rule {
    use super::*;

    /// A node: its output width and what feeds each of its inputs.
    pub struct Node {
        pub outs: usize,
        pub sources: Vec<Source>,
    }

    fn port(node: NodeKey, port: usize) -> Source {
        Source::Node(OutPort {
            node,
            port: port as u16,
        })
    }

    /// `Net::pipe_all(a, b)`: input `c` of `b` reads `a`'s port
    /// `c % a_outs`; silence when `a` has no outputs.
    pub fn pipe_all(a: NodeKey, a_outs: usize, b_ins: usize) -> Vec<Source> {
        (0..b_ins)
            .map(|c| {
                if a_outs > 0 {
                    port(a, c % a_outs)
                } else {
                    Source::Zero
                }
            })
            .collect()
    }

    /// `Net::pipe_output(n)`: global output `c` reads `n`'s port
    /// `c % n_outs`; silence when `n` has no outputs.
    pub fn pipe_output(n: NodeKey, n_outs: usize, outputs: usize) -> Vec<Source> {
        (0..outputs)
            .map(|c| {
                if n_outs > 0 {
                    port(n, c % n_outs)
                } else {
                    Source::Zero
                }
            })
            .collect()
    }

    /// `Net::pipe_input(n)`: input `c` reads global input `c % globals`;
    /// silence when there are none.
    pub fn pipe_input(globals: usize, ins: usize) -> Vec<Source> {
        (0..ins)
            .map(|c| {
                if globals > 0 {
                    Source::Global((c % globals) as u16)
                } else {
                    Source::Zero
                }
            })
            .collect()
    }

    /// `Net::chain` over `widths`: the first node takes `pipe_input` (when
    /// there are global inputs; otherwise its inputs stay silent), each later
    /// one reads what fed global output `i % outputs` (silence with no
    /// outputs), and each takes over the outputs with `pipe_output`.
    pub fn chain(
        globals: usize,
        outputs: usize,
        widths: &[(usize, usize)],
    ) -> (Vec<(NodeKey, Node)>, Vec<Source>) {
        let mut nodes = Vec::new();
        let mut outs = vec![Source::Zero; outputs];
        for (k, &(ins, w)) in widths.iter().enumerate() {
            let key = NodeKey(k as u64);
            let sources = if k == 0 {
                if globals > 0 {
                    pipe_input(globals, ins)
                } else {
                    vec![Source::Zero; ins]
                }
            } else {
                (0..ins)
                    .map(|i| {
                        if outputs > 0 {
                            outs[i % outputs]
                        } else {
                            Source::Zero
                        }
                    })
                    .collect()
            };
            nodes.push((key, Node { outs: w, sources }));
            outs = pipe_output(key, w, outputs);
        }
        (nodes, outs)
    }

    /// What each global output carries with silent global inputs: a
    /// [`Width`] node's output `c` is `c + 1` plus the sum of its inputs,
    /// evaluated in the order the nodes were added (every source precedes
    /// the node it feeds in these graphs).
    pub fn constants(nodes: &[(NodeKey, Node)], outputs: &[Source]) -> Vec<f32> {
        let mut value: Vec<(NodeKey, Vec<f32>)> = Vec::new();
        let read = |value: &[(NodeKey, Vec<f32>)], s: &Source| match s {
            Source::Node(OutPort { node, port }) => {
                value
                    .iter()
                    .find(|(k, _)| k == node)
                    .expect("source precedes")
                    .1[*port as usize]
            }
            _ => 0.0,
        };
        for (key, n) in nodes {
            let sum: f32 = n.sources.iter().map(|s| read(&value, s)).sum();
            let outs = (0..n.outs).map(|c| (c + 1) as f32 + sum).collect();
            value.push((*key, outs));
        }
        outputs.iter().map(|s| read(&value, s)).collect()
    }
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
    let mix = g.add(Unforkable(TestNode::new(Kind::Sum { inputs: 2 })));
    let lp = g.add(Unforkable(TestNode::new(Kind::Smooth)));
    let emit = g.add(Unforkable(TestNode::new(Kind::Emitter {
        period: 50,
        phase: 7,
    })));
    let fold = g.add(Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })));
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
        test_node,
        Unforkable(TestNode::new(Kind::Sum { inputs: 2 })),
    );
    ed.insert(
        NodeKey(1),
        test_node,
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(2),
        test_node,
        Unforkable(TestNode::new(Kind::Emitter {
            period: 50,
            phase: 7,
        })),
    );
    ed.insert(
        NodeKey(3),
        test_node,
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
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
/// grid of widths, including a source with no outputs, checked against the
/// rule written out ([`net_rule::pipe_all`]) and by the figure it renders.
///
/// Mutation: `c % outs` → `(outs - 1).min(c)` (clamp) in `pipe` → mono
/// source fine, 2 → 3 differs at input 2 → fails. Mutation: reverse the
/// port order → fails at the first 2-wide case.
#[test]
fn pipe_connects_ports_in_order_as_net_pipe_all() {
    for outs in 0..=3 {
        for ins in 1..=3 {
            let case = format!("{outs} outputs → {ins} inputs");
            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
            let (a, b) = (g.add(node(0, outs)), g.add(node(ins, 1)));
            g.pipe(a, b).pipe_output(b);
            let want = [
                (
                    a,
                    net_rule::Node {
                        outs,
                        sources: vec![],
                    },
                ),
                (
                    b,
                    net_rule::Node {
                        outs: 1,
                        sources: net_rule::pipe_all(a, outs, ins),
                    },
                ),
            ];
            let outputs = net_rule::pipe_output(b, 1, 1);
            assert_wired(&g, &want, &outputs, &case);

            // And it renders what that wiring carries.
            let got = g.renderer(prepare(64)).expect("builds").render(100);
            assert_renders(&got, &want, &outputs, 100, &case);
        }
    }
    // The plain case, spelled out: port c → port c.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (a, b) = (g.add(node(0, 3)), g.add(node(3, 1)));
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
/// outputs feeds silence. Checked structurally and by the figure it renders.
///
/// Mutation: clamp (`c.min(outs - 1)`) instead of `c % outs` → stereo into
/// six differs from the rule → fails. Mutation: feed a zero-output node's
/// channels from port 0 → out of range → panics.
#[test]
fn pipe_output_fans_out_as_net_does() {
    for (w, outs) in [(0, 2), (1, 1), (1, 2), (1, 6), (2, 6), (3, 2), (6, 2)] {
        let case = format!("{w}-wide node → {outs} outputs");
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(outs));
        let key = g.add(node(0, w));
        g.pipe_output(key);
        let want = [(
            key,
            net_rule::Node {
                outs: w,
                sources: vec![],
            },
        )];
        let outputs = net_rule::pipe_output(key, w, outs);
        assert_wired(&g, &want, &outputs, &case);

        let got = g.renderer(prepare(64)).expect("builds").render(70);
        assert_renders(&got, &want, &outputs, 70, &case);
    }
    // Stereo into six, spelled out.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(6));
    let key = g.add(node(0, 2));
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
            let mut g = GraphBuilder::new(ch(globals), ChannelLayout::MONO);
            let key = g.add(node(ins, 1));
            g.pipe_input(key).pipe_output(key);
            let want = [(
                key,
                net_rule::Node {
                    outs: 1,
                    sources: net_rule::pipe_input(globals, ins),
                },
            )];
            assert_wired(&g, &want, &net_rule::pipe_output(key, 1, 1), &case);
        }
    }
}

/// `chain` extends a series as `Net::chain` does: the first node reads the
/// global inputs, each later one reads what fed the global outputs
/// (wrapping), and every one takes over the outputs. Across width changes,
/// with and without global inputs, and to the bit in render.
///
/// Mutation: skip `pipe_input` for the first node in `link` → it reads
/// silence where the rule reads the global input → fails. Mutation: read
/// `outputs[c]` without the modulo → the 2-input node after a mono-output
/// graph is out of range → panics → fails.
#[test]
fn chain_extends_the_series_as_net_does() {
    for globals in [0, 1, 2] {
        let widths = [(1, 1), (1, 3), (2, 2), (3, 1), (1, 2)];
        let case = format!("{globals} globals");
        let mut g = GraphBuilder::new(ch(globals), ChannelLayout::STEREO);
        for (ins, outs) in widths {
            g.chain(node(ins, outs));
        }
        let (want, outputs) = net_rule::chain(globals, 2, &widths);
        assert_wired(&g, &want, &outputs, &case);

        let got = g.renderer(prepare(64)).expect("builds").render(70);
        assert_renders(&got, &want, &outputs, 70, &case);
    }
}

/// `chain` takes an `IntoNode` with controls too, and `add_with_controls` hands back what
/// `IntoNode` returns.
///
/// Mutation: skip `pipe_input` for the first node in `link` → the chain
/// reads silence → 0 instead of 6 → fails.
#[test]
fn chain_and_add_take_nodes() {
    struct Tagged;
    impl tutti_graph::IntoNode for Tagged {
        type Controls = u32;
        fn into_parts(self) -> tutti_graph::NodeParts<u32> {
            tutti_graph::NodeParts {
                node: Box::new(TestNode::new(Kind::Gain {
                    gain: 2.0,
                    width: 1,
                })),
                controls: 7,
                fork: None,
            }
        }
    }
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let first = g.chain(Unforkable(TestNode::new(Kind::Gain {
        gain: 3.0,
        width: 1,
    })));
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
    let (a, b) = (g.add(node(0, 1)), g.add(node(1, 1)));
    g.connect(a, 0, b, 1);
}

/// A graph the editor refuses is refused by `build` with the editor's own
/// error: a feedback delay shorter than the block.
///
/// Mutation: `build` ignores `commit`'s result → `Ok` → fails.
#[test]
fn build_returns_the_editors_error() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let a = g.add(node(1, 1));
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
    let key = g.add(node(0, 2));
    g.pipe_output(key);
    let mut r = g.renderer(prepare(256)).expect("builds");
    r.set_block(Samples(100));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    r.set_transport_fn(move |frame| {
        log.lock().unwrap().push(frame);
        Transport::new(true, tutti_types::Bpm(120.0), Beat(frame.0 as f64), None)
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
