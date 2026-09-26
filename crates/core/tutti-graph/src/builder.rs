//! [`GraphBuilder`]: a `Net`-shaped way to write a [`GraphSpec`] and its
//! units, and [`Renderer`], which drives the result block by block.
//!
//! # Not a second graph model
//!
//! Everything here is a spelling of what already exists. A builder holds a
//! [`GraphSpec`] and the units it names, and nothing else: every wiring call
//! writes one entry of that value, and [`build`](GraphBuilder::build) hands
//! the units to an [`Editor`] through [`Editor::insert`], copies the wiring
//! across and [`commit`](Editor::commit)s it — the public path any host
//! takes. There is no second validation, no second compile and no state the
//! editor does not also hold, so a graph that builds here is exactly a graph
//! a host could have written by hand (`tests/builder.rs` pins that: the two
//! produce equal specs, equal plans and bit-identical renders).
//!
//! # Why it exists
//!
//! The engine's tests, examples and simple hosts were written against
//! fundsp's `Net` (`push`, `connect`, `pipe_output`, …). Doc 013's Phase 3
//! moves them to the native graph (PR 8 for `tutti-export`), and that port
//! should be mechanical: same call, same meaning, including `Net`'s
//! fan-out rules, which are quoted on each method below. Writing a
//! [`GraphSpec`] by hand instead is several `BTreeMap` inserts per edge.
//!
//! # Keys
//!
//! Nodes get [`NodeKey`]s in the order they are added, from 0. Deterministic,
//! so two builders fed the same calls produce equal specs.
//!
//! # Widths are read when a node is added
//!
//! `pipe`, `pipe_input`, `pipe_output` and `chain` fan out over a node's port
//! counts, which the builder reads from [`Node::shape`] at
//! [`add`](GraphBuilder::add), before the node is prepared. A node whose
//! port count depends on its [`Prepare`] would need wiring by
//! [`connect`](GraphBuilder::connect) instead (none does today: `Legacy`
//! takes its widths from `AudioUnit::inputs`/`outputs`, which never move).
//! Latency and tail can move with the rate, and [`Editor::insert`] rewrites
//! them from the prepared shape — so [`GraphBuilder::spec`] shows the
//! unprepared figures and the built editor's spec the prepared ones.
//!
//! # Panics, not errors
//!
//! Like `Net`, the wiring calls assert their port indices: a builder is for
//! code whose graph is fixed in its source, where an out-of-range port is a
//! typo to fix, not a condition to handle. Anything that needs the shapes
//! *and* the edges together (a cycle, a feedback delay shorter than the
//! block) is found by [`build`](GraphBuilder::build) and returned as the
//! editor's own [`CommitError`].

use tutti_node::AudioUnit;
use tutti_types::graph::{Edge, FeedbackFrom, InPort, NodeSpec, OutPort, Source};
use tutti_types::{ChannelLayout, Frame, NodeKey, Samples};

use crate::editor::{CommitError, Editor};
use crate::exec::Executor;
use crate::legacy::Legacy;
use crate::node::{IntoNode, NodeParts, Prepare, Shape, Transport};
use crate::spec::{EventEdge, EventIn, EventOut, GraphSpec};

/// One unit waiting for [`GraphBuilder::build`], with its fork source, so a
/// built graph forks like one inserted by hand ([`Editor::fork`]).
struct Pending {
    key: NodeKey,
    kind: String,
    unit: NodeParts<()>,
}

/// Builds a [`GraphSpec`] and its units with `Net`'s calls, then hands both
/// to an [`Editor`]. See the `builder` module docs (`src/builder.rs`): it is a
/// spelling of the spec, not a second graph model.
///
/// # Example
///
/// The port of a typical `Net` test fixture — `Net::new(0, 2)`, `push`, and
/// `pipe_output`:
///
/// ```
/// # use fundsp::prelude32::dc;
/// use tutti_graph::{GraphBuilder, Prepare};
/// use tutti_types::{ChannelLayout, SampleRate, Samples};
///
/// let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
/// let src = g.add_unit(Box::new(dc(0.25)));
/// g.pipe_output(src); // mono → both channels, as `Net::pipe_output` does
///
/// let mut r = g
///     .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
///     .expect("builds");
/// let out = r.render(100); // two 64-frame blocks, the second short
/// assert_eq!(out.len(), 2);
/// assert!(out.iter().all(|ch| ch == &vec![0.25; 100]));
/// ```
///
/// Explicit wiring — `connect` between nodes, `connect_input` /
/// `connect_output` at the edges, `chain` for a series:
///
/// ```
/// # use fundsp::prelude32::{dc, mul, pass};
/// use tutti_graph::{GraphBuilder, Prepare};
/// use tutti_types::{ChannelLayout, SampleRate, Samples};
///
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
/// let half = g.chain_unit(Box::new(mul(0.5))); // global in → half → both outs
/// let gain = g.add_unit(Box::new(mul(4.0)));
/// g.connect(half, 0, gain, 0).connect_output(gain, 0, 1); // right = 2 × input
///
/// let mut r = g
///     .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
///     .expect("builds");
/// let out = r.render_input(&[&[1.0; 10]]);
/// assert_eq!(out, vec![vec![0.5; 10], vec![2.0; 10]]);
/// ```
pub struct GraphBuilder {
    spec: GraphSpec,
    /// Every added node's shape, read at `add` — the widths the fan-out
    /// calls use.
    shapes: Vec<Shape>,
    units: Vec<Pending>,
}

impl GraphBuilder {
    /// An empty graph with `inputs` global input channels and `outputs`
    /// global output channels, every output silent until wired — `Net::new`.
    pub fn new(inputs: ChannelLayout, outputs: ChannelLayout) -> Self {
        let mut spec = GraphSpec::default();
        spec.topology.inputs = inputs;
        spec.topology.outputs = vec![Source::Zero; outputs.count() as usize];
        Self {
            spec,
            shapes: Vec::new(),
            units: Vec::new(),
        }
    }

    /// Add a node, unwired. Its kind (the [`NodeSpec::kind`] diagnostics
    /// print) is its type name ([`IntoNode::kind`]: the wrapped node's, for
    /// a fork wrapper).
    ///
    /// For a node with controls use
    /// [`add_with_controls`](Self::add_with_controls), so the handles are
    /// not dropped unseen.
    pub fn add<N: IntoNode<Controls = ()>>(&mut self, node: N) -> NodeKey {
        self.add_with_controls(node).0
    }

    /// Add a node, unwired, and return its controls with its key.
    pub fn add_with_controls<N: IntoNode>(&mut self, node: N) -> (NodeKey, N::Controls) {
        let NodeParts {
            node,
            controls,
            fork,
        } = node.into_parts();
        let unit = NodeParts {
            node,
            controls: (),
            fork,
        };
        (self.insert(N::kind(), unit), controls)
    }

    /// Add a fundsp `AudioUnit`, unwired, running through [`Legacy`] — the
    /// port of `Net::push(Box::new(unit))`. Its kind is `"legacy"`.
    pub fn add_unit(&mut self, unit: Box<dyn AudioUnit>) -> NodeKey {
        self.insert("legacy", Legacy::from_box(unit).into_parts())
    }

    /// [`add_unit`](Self::add_unit) for a unit whose output is a function of
    /// its audio inputs alone (a filter, a gain), through
    /// [`Legacy::pure`]: its silence is reported, so it may be skipped. A
    /// unit fed any other way belongs in `add_unit`, which never skips it.
    pub fn add_pure_unit(&mut self, unit: Box<dyn AudioUnit>) -> NodeKey {
        self.insert("legacy", Legacy::from_box(unit).assume_pure().into_parts())
    }

    fn insert(&mut self, kind: &str, unit: NodeParts<()>) -> NodeKey {
        let key = NodeKey(self.units.len() as u64);
        let shape = unit.node.shape();
        self.spec.topology.nodes.insert(
            key,
            NodeSpec::new(kind, shape.audio_in, shape.audio_out)
                .with_latency(shape.latency.samples())
                .with_tail(shape.tail),
        );
        self.shapes.push(shape);
        self.units.push(Pending {
            key,
            kind: kind.to_string(),
            unit,
        });
        key
    }

    /// The shape `node` declared when it was added.
    ///
    /// # Panics
    ///
    /// If `node` was not added by this builder.
    pub fn shape(&self, node: NodeKey) -> Shape {
        *usize::try_from(node.0)
            .ok()
            .and_then(|i| self.shapes.get(i))
            .unwrap_or_else(|| panic!("{node:?} was not added by this builder"))
    }

    /// Audio inputs of `node` — `Net::inputs_in`.
    pub fn inputs_in(&self, node: NodeKey) -> usize {
        self.shape(node).audio_in.count() as usize
    }

    /// Audio outputs of `node` — `Net::outputs_in`.
    pub fn outputs_in(&self, node: NodeKey) -> usize {
        self.shape(node).audio_out.count() as usize
    }

    /// Global input channels.
    pub fn inputs(&self) -> usize {
        self.spec.topology.inputs.count() as usize
    }

    /// Global output channels.
    pub fn outputs(&self) -> usize {
        self.spec.topology.outputs.len()
    }

    /// How many nodes were added — `Net::size`.
    pub fn size(&self) -> usize {
        self.units.len()
    }

    /// The graph value so far. Latency and tail are as each node declared
    /// them *before* it was prepared (see the module docs).
    pub fn spec(&self) -> &GraphSpec {
        &self.spec
    }

    /// The graph value, for what the calls here do not spell — a
    /// [`require_resolution`](GraphSpec::require_resolution) mark, a
    /// parameter value. Nodes are added through [`add`](Self::add), never
    /// here, so every node has a unit.
    pub fn spec_mut(&mut self) -> &mut GraphSpec {
        &mut self.spec
    }

    fn check_in(&self, node: NodeKey, port: usize) -> InPort {
        let n = self.inputs_in(node);
        assert!(
            port < n,
            "{node:?} has {n} audio inputs; port {port} is out of range"
        );
        InPort {
            node,
            port: port as u16,
        }
    }

    fn check_out(&self, node: NodeKey, port: usize) -> OutPort {
        let n = self.outputs_in(node);
        assert!(
            port < n,
            "{node:?} has {n} audio outputs; port {port} is out of range"
        );
        OutPort {
            node,
            port: port as u16,
        }
    }

    fn check_source(&self, source: Source) -> Source {
        match source {
            Source::Node(p) => Source::Node(self.check_out(p.node, p.port as usize)),
            Source::Global(i) => {
                let n = self.inputs();
                assert!(
                    (i as usize) < n,
                    "the graph has {n} global inputs; input {i} is out of range"
                );
                source
            }
            Source::Zero => source,
        }
    }

    /// Feed `node`'s input `port` from `source` — `Net::set_source`.
    ///
    /// # Panics
    ///
    /// If the port or the source is out of range, or `source` is an output
    /// of `node` itself (a self-loop is a cycle; see
    /// [`feedback`](Self::feedback)).
    pub fn set_source(&mut self, node: NodeKey, port: usize, source: Source) -> &mut Self {
        assert!(
            !matches!(source, Source::Node(p) if p.node == node),
            "{node:?} cannot feed itself directly; use `feedback`"
        );
        let at = self.check_in(node, port);
        let source = self.check_source(source);
        self.spec.topology.edges.insert(at, Edge::Direct(source));
        self
    }

    /// Feed `to`'s input `to_port` from `from`'s output `from_port` —
    /// `Net::connect`. Replaces whatever fed that input: one source per
    /// input port, as in `Net`.
    pub fn connect(
        &mut self,
        from: NodeKey,
        from_port: usize,
        to: NodeKey,
        to_port: usize,
    ) -> &mut Self {
        let source = Source::Node(self.check_out(from, from_port));
        self.set_source(to, to_port, source)
    }

    /// Feed `to`'s input `to_port` from `from`'s output `from_port` as it
    /// was `delay` frames ago — a declared cycle
    /// ([`FeedbackFrom`](tutti_types::graph::FeedbackFrom)). `Net` has no
    /// counterpart. `delay` must be at least the maximum block the graph is
    /// built for; [`build`](Self::build) refuses a shorter one.
    pub fn feedback(
        &mut self,
        from: NodeKey,
        from_port: usize,
        to: NodeKey,
        to_port: usize,
        delay: Samples,
    ) -> &mut Self {
        let from = self.check_out(from, from_port);
        let at = self.check_in(to, to_port);
        self.spec
            .topology
            .edges
            .insert(at, Edge::Feedback(FeedbackFrom::new(from, delay)));
        self
    }

    /// Feed `node`'s input `port` from silence — `Net::disconnect`. An
    /// explicit [`Source::Zero`], which reads the same as an unconnected
    /// port but says it was meant.
    pub fn disconnect(&mut self, node: NodeKey, port: usize) -> &mut Self {
        self.set_source(node, port, Source::Zero)
    }

    /// Feed `to`'s input `to_port` from global input `global` —
    /// `Net::connect_input`.
    pub fn connect_input(&mut self, global: usize, to: NodeKey, to_port: usize) -> &mut Self {
        self.set_source(to, to_port, Source::Global(global as u16))
    }

    /// Feed global output `global` from `from`'s output `from_port` —
    /// `Net::connect_output`.
    pub fn connect_output(&mut self, from: NodeKey, from_port: usize, global: usize) -> &mut Self {
        let source = Source::Node(self.check_out(from, from_port));
        self.set_output(global, source)
    }

    /// Feed global output `channel` from `source` —
    /// `Net::set_output_source`.
    ///
    /// # Panics
    ///
    /// If `channel` or `source` is out of range.
    pub fn set_output(&mut self, channel: usize, source: Source) -> &mut Self {
        let n = self.outputs();
        assert!(
            channel < n,
            "the graph has {n} global outputs; output {channel} is out of range"
        );
        self.spec.topology.outputs[channel] = self.check_source(source);
        self
    }

    /// Feed global output `output` straight from global input `input` —
    /// `Net::pass_through`.
    pub fn pass_through(&mut self, input: usize, output: usize) -> &mut Self {
        self.set_output(output, Source::Global(input as u16))
    }

    /// Feed every input of `to` from the outputs of `from`, in order —
    /// `Net::pipe_all`, with its fan-out rule: input `c` reads output
    /// `c % outputs`, so a mono source feeds every channel of a wider sink
    /// and a wider source's extra channels go unused. A source with no
    /// outputs feeds silence.
    pub fn pipe(&mut self, from: NodeKey, to: NodeKey) -> &mut Self {
        let outs = self.outputs_in(from);
        for c in 0..self.inputs_in(to) {
            let source = if outs > 0 {
                Source::Node(OutPort {
                    node: from,
                    port: (c % outs) as u16,
                })
            } else {
                Source::Zero
            };
            self.set_source(to, c, source);
        }
        self
    }

    /// Feed every input of `node` from the global inputs, in order —
    /// `Net::pipe_input`, with its rule: input `c` reads global input
    /// `c % inputs`, and with no global inputs at all every input reads
    /// silence.
    pub fn pipe_input(&mut self, node: NodeKey) -> &mut Self {
        let globals = self.inputs();
        for c in 0..self.inputs_in(node) {
            let source = if globals > 0 {
                Source::Global((c % globals) as u16)
            } else {
                Source::Zero
            };
            self.set_source(node, c, source);
        }
        self
    }

    /// Feed every global output from the outputs of `node`, in order —
    /// `Net::pipe_output`, with its rule: global output `c` reads output
    /// `c % outputs`. So a mono node feeds every channel, a stereo node
    /// feeding six channels repeats L R L R L R (it wraps, it does not
    /// clamp to the last channel), a node wider than the graph has its
    /// extra channels unused, and a node with no outputs feeds silence.
    pub fn pipe_output(&mut self, node: NodeKey) -> &mut Self {
        let outs = self.outputs_in(node);
        for c in 0..self.outputs() {
            let source = if outs > 0 {
                Source::Node(OutPort {
                    node,
                    port: (c % outs) as u16,
                })
            } else {
                Source::Zero
            };
            self.set_output(c, source);
        }
        self
    }

    /// Add `node` at the end of the chain the global outputs describe —
    /// `Net::chain`. The first node added to an empty builder reads the
    /// global inputs ([`pipe_input`](Self::pipe_input), when there are
    /// any); a later one reads whatever fed the global outputs, input `c`
    /// taking output source `c % outputs`. Either way the node then feeds
    /// every global output ([`pipe_output`](Self::pipe_output)).
    pub fn chain<N: IntoNode<Controls = ()>>(&mut self, node: N) -> NodeKey {
        let key = self.insert(N::kind(), node.into_parts());
        self.link(key);
        key
    }

    /// [`chain`](Self::chain) for a fundsp `AudioUnit` through [`Legacy`] —
    /// the port of `Net::chain(Box::new(unit))`.
    pub fn chain_unit(&mut self, unit: Box<dyn AudioUnit>) -> NodeKey {
        let key = self.add_unit(unit);
        self.link(key);
        key
    }

    fn link(&mut self, key: NodeKey) {
        if self.size() == 1 {
            if self.inputs() > 0 {
                self.pipe_input(key);
            }
        } else {
            let outputs = self.spec.topology.outputs.clone();
            for c in 0..self.inputs_in(key) {
                let source = if outputs.is_empty() {
                    Source::Zero
                } else {
                    outputs[c % outputs.len()]
                };
                self.set_source(key, c, source);
            }
        }
        self.pipe_output(key);
    }

    /// Add `from`'s event output `from_port` to the sources of `to`'s event
    /// input `to_port` ([`GraphSpec::connect_events`]). Event ports take
    /// fan-in: events at equal offsets arrive in source order, the source
    /// port's `(NodeKey, port)` — for nodes this builder added, the order
    /// they were added in (keys are handed out increasing).
    ///
    /// # Panics
    ///
    /// If either event port is out of range.
    pub fn event_connect(
        &mut self,
        from: NodeKey,
        from_port: usize,
        to: NodeKey,
        to_port: usize,
    ) -> &mut Self {
        let (outs, ins) = (self.shape(from).event_out, self.shape(to).event_in);
        assert!(
            from_port < outs as usize,
            "{from:?} has {outs} event outputs; port {from_port} is out of range"
        );
        assert!(
            to_port < ins as usize,
            "{to:?} has {ins} event inputs; port {to_port} is out of range"
        );
        self.spec.connect_events(
            EventIn {
                node: to,
                port: to_port as u16,
            },
            EventEdge::Direct(EventOut {
                node: from,
                port: from_port as u16,
            }),
        );
        self
    }

    /// Prepare every unit for `prepare`, commit the graph, and install it:
    /// the returned executor is already running the plan, and the editor
    /// has nothing in flight.
    ///
    /// The path is the public one — [`Editor::new`], one [`Editor::insert`]
    /// per node in the order they were added, the wiring copied into
    /// [`Editor::spec_mut`], [`Editor::commit`] — so an error is the
    /// editor's own.
    pub fn build(self, prepare: Prepare) -> Result<(Editor, Executor), CommitError> {
        let (mut editor, mut executor) = Editor::new(prepare);
        for Pending { key, kind, unit } in self.units {
            editor.insert(key, &kind, unit);
        }
        // `insert` wrote each node's spec from its prepared shape; the
        // builder's copies carry the unprepared figures, so only the wiring
        // moves across.
        let GraphSpec {
            topology,
            events,
            generations: _,
            required_resolution,
            params,
        } = self.spec;
        let live = editor.spec_mut();
        live.topology.edges = topology.edges;
        live.topology.outputs = topology.outputs;
        live.topology.inputs = topology.inputs;
        live.events = events;
        live.required_resolution = required_resolution;
        live.params = params;
        // Parameter values set through `spec_mut` ride along too. `insert`
        // keeps params already recorded for a key, but these were recorded
        // on the builder's copy, not the editor's.
        for (key, node) in topology.nodes {
            if let Some(n) = live.topology.nodes.get_mut(&key) {
                n.params = node.params;
            }
        }
        editor.commit()?;
        executor.apply_pending();
        editor.collect();
        Ok((editor, executor))
    }

    /// [`build`](Self::build), wrapped in a [`Renderer`].
    pub fn renderer(self, prepare: Prepare) -> Result<Renderer, CommitError> {
        let (editor, executor) = self.build(prepare)?;
        Ok(Renderer::new(editor, executor))
    }
}

/// What a [`Renderer`] reads the transport from, per block.
type TransportFn = Box<dyn FnMut(Frame) -> Transport + Send>;

/// Drives an [`Executor`] a block at a time, for tests and offline tools:
/// hands it the transport, silence or the given input, and collects its
/// output planar or interleaved.
///
/// Blocks are the prepared maximum unless [`set_block`](Self::set_block)
/// says otherwise; the last one is short. After each block it drains what
/// the executor sent back ([`Editor::collect`]), so retired units are freed
/// as a host would free them.
///
/// Allocates (its output buffers, and the per-block slice lists); the
/// executor it drives does not. Not for an audio callback.
///
/// # Example
///
/// Render a planar input in 200-frame blocks, then silence, interleaved:
///
/// ```
/// # use fundsp::prelude32::pass;
/// use tutti_graph::{GraphBuilder, Prepare, Transport};
/// use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate, Samples};
///
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
/// g.chain_unit(Box::new(pass()));
///
/// let mut r = g
///     .renderer(Prepare::new(SampleRate(48_000.0), Samples(256)))
///     .expect("builds");
/// r.set_block(Samples(200));
/// let input: Vec<f32> = (0..500).map(|i| i as f32).collect();
/// let out = r.render_input(&[&input]);
/// assert_eq!(out, vec![input.clone(), input]);
///
/// // A rolling transport is a function of the block's first frame.
/// r.set_transport_fn(|frame| {
///     // 120 BPM at 48 kHz: 24 000 frames a beat.
///     Transport::new(true, Bpm(120.0), Beat(frame.0 as f64 / 24_000.0), None)
/// });
/// assert_eq!(r.render_interleaved(2), vec![0.0; 4]); // L R L R
/// ```
pub struct Renderer {
    editor: Editor,
    executor: Executor,
    block: usize,
    transport: TransportFn,
}

impl Renderer {
    /// Drive `executor`, draining back into `editor`. Transport stopped at
    /// beat zero ([`Transport::default`]); blocks at the prepared maximum.
    ///
    /// # Panics
    ///
    /// If the two were not built together by [`Editor::new`].
    pub fn new(editor: Editor, executor: Executor) -> Self {
        assert!(
            editor.is_paired_with(&executor),
            "a renderer drains back into the editor that feeds its executor"
        );
        let block = executor.prepare().max_block().get();
        Self {
            editor,
            executor,
            block,
            transport: Box::new(|_| Transport::default()),
        }
    }

    /// Render in blocks of `frames` (the last of a render may be shorter).
    ///
    /// # Panics
    ///
    /// If `frames` is zero or longer than the prepared maximum block.
    pub fn set_block(&mut self, frames: Samples) -> &mut Self {
        let max = self.executor.prepare().max_block().get();
        assert!(
            frames.get() > 0 && frames.get() <= max,
            "a block of {} frames against a max of {max}",
            frames.get()
        );
        self.block = frames.get();
        self
    }

    /// Hand every block `transport`, unchanged. It does not advance: a
    /// playing transport whose beat should move is a
    /// [`set_transport_fn`](Self::set_transport_fn).
    pub fn set_transport(&mut self, transport: Transport) -> &mut Self {
        self.transport = Box::new(move |_| transport);
        self
    }

    /// Ask `f` for each block's transport, given the block's first frame
    /// ([`Executor::frame`]).
    pub fn set_transport_fn(
        &mut self,
        f: impl FnMut(Frame) -> Transport + Send + 'static,
    ) -> &mut Self {
        self.transport = Box::new(f);
        self
    }

    /// Render `frames` with silent global inputs; one `Vec` per global
    /// output channel.
    pub fn render(&mut self, frames: usize) -> Vec<Vec<f32>> {
        let mut out = vec![vec![0.0f32; frames]; self.global_outputs()];
        let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        self.run(frames, None, &mut refs);
        out
    }

    /// Render one frame per input sample, reading `input` (one slice per
    /// global input channel, all the same length); one `Vec` per global
    /// output channel.
    ///
    /// # Panics
    ///
    /// If `input` has the wrong channel count or ragged lengths, or the
    /// graph has no global inputs (there is nothing to count frames by: use
    /// [`render`](Self::render)).
    pub fn render_input(&mut self, input: &[&[f32]]) -> Vec<Vec<f32>> {
        let frames = input
            .first()
            .expect("a graph with no global inputs renders with `render`")
            .len();
        let mut out = vec![vec![0.0f32; frames]; self.global_outputs()];
        let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        self.run(frames, Some(input), &mut refs);
        out
    }

    /// [`render`](Self::render), interleaved: frame by frame, channel by
    /// channel.
    pub fn render_interleaved(&mut self, frames: usize) -> Vec<f32> {
        let planar = self.render(frames);
        let mut out = Vec::with_capacity(frames * planar.len());
        for f in 0..frames {
            out.extend(planar.iter().map(|c| c[f]));
        }
        out
    }

    /// Render into `output` (one slice per global output channel, all the
    /// same length, which is the frame count) with silent global inputs.
    ///
    /// # Panics
    ///
    /// If `output` has the wrong channel count or ragged lengths, or no
    /// channels at all.
    pub fn render_into(&mut self, output: &mut [&mut [f32]]) {
        let frames = output
            .first()
            .expect("a graph with no global outputs has nothing to render into")
            .len();
        self.run(frames, None, output);
    }

    /// Render `input` into `output`, planar. The frame count is the
    /// channels' length.
    ///
    /// # Panics
    ///
    /// If either side has the wrong channel count, the channels are not all
    /// one length, or there are no channels on either side.
    pub fn render_input_into(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let frames = input
            .first()
            .map(|c| c.len())
            .or_else(|| output.first().map(|c| c.len()))
            .expect("no channels to count frames by");
        self.run(frames, Some(input), output);
    }

    /// Render `frames` in blocks, reading `input` (silence when `None`).
    fn run(&mut self, frames: usize, input: Option<&[&[f32]]>, output: &mut [&mut [f32]]) {
        let (ins, outs) = (self.global_inputs(), self.global_outputs());
        let silence = vec![0.0f32; if input.is_none() { frames } else { 0 }];
        let silent = vec![&silence[..]; if input.is_none() { ins } else { 0 }];
        let input = input.unwrap_or(&silent);
        assert_eq!(input.len(), ins, "one input slice per global input");
        assert_eq!(output.len(), outs, "one output slice per global output");
        assert!(
            input.iter().all(|c| c.len() == frames) && output.iter().all(|c| c.len() == frames),
            "every channel must be {frames} frames long"
        );
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(self.block);
            let transport = (self.transport)(self.executor.frame());
            let block_in: Vec<&[f32]> = input.iter().map(|c| &c[done..done + n]).collect();
            let mut block_out: Vec<&mut [f32]> =
                output.iter_mut().map(|c| &mut c[done..done + n]).collect();
            self.executor
                .process(n, &transport, &block_in, &mut block_out);
            self.editor.collect();
            done += n;
        }
    }

    fn global_inputs(&self) -> usize {
        self.editor.spec().topology.inputs.count() as usize
    }

    fn global_outputs(&self) -> usize {
        self.editor.spec().topology.outputs.len()
    }

    /// The editor, to inspect the graph.
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// The editor, to change the graph between renders. A commit reaches
    /// the executor at the start of the next block rendered.
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// The executor, to inspect what it runs.
    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    /// The executor, to drive a block by hand.
    pub fn executor_mut(&mut self) -> &mut Executor {
        &mut self.executor
    }

    /// The pair back.
    pub fn into_parts(self) -> (Editor, Executor) {
        (self.editor, self.executor)
    }
}
