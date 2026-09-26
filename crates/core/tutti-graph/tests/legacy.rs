//! `Legacy`: an unmodified `AudioUnit` runs through the new graph and renders
//! exactly what fundsp's `Net` renders from it.

mod common;

use common::prepare;
use fundsp::net::Net;
use fundsp::prelude32::{limiter, lowpass_hz};
use tutti_graph::{
    Delivery, Editor, GraphBuilder, IntoNode, Legacy, LegacyControls, Transport,
    LEGACY_SETTINGS_CAPACITY,
};
use tutti_node::buffer::BufferVec;
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::NodeKey;
use tutti_types::{ChannelLayout, Latency, SampleRate, Samples, Tail};

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 48.0)
        .collect()
}

/// Global input → two Legacy filters in series → output, rendered in
/// 200-frame blocks (so the adapter sub-chunks 64/64/64/8 and the chunk
/// boundaries drift against `Net`'s), against the same two filters in a `Net`
/// rendered in 64-frame chunks. Bit-identical.
///
/// Mutation: in `Legacy::process`, copy the input with `start` fixed at 0
/// (every chunk re-reads the first 64 frames) → diverges from frame 64 →
/// fails. Mutation: skip `set_sample_rate` in `Legacy::prepare` → the filters
/// run at fundsp's default rate → diverges → fails.
#[test]
fn legacy_nodes_render_what_net_renders() {
    const RATE: f64 = 48_000.0;
    let total = 2_000;
    let input = signal(total);

    // The Net side.
    let mut net = Net::new(1, 1);
    let a = net.push(Box::new(lowpass_hz(700.0, 0.8)));
    let b = net.push(Box::new(lowpass_hz(2_300.0, 1.1)));
    net.pipe_input(a);
    net.connect(a, 0, b, 0);
    net.pipe_output(b);
    net.set_sample_rate(SampleRate(RATE));
    net.allocate();
    let mut want = Vec::with_capacity(total);
    let mut ibuf = BufferVec::new(1);
    let mut obuf = BufferVec::new(1);
    for chunk in input.chunks(MAX_BUFFER_SIZE) {
        ibuf.channel_f32_mut(0)[..chunk.len()].copy_from_slice(chunk);
        net.process(chunk.len(), &ibuf.buffer_ref(), &mut obuf.buffer_mut());
        want.extend_from_slice(&obuf.channel_f32_mut(0)[..chunk.len()]);
    }

    // The graph side, built as the `Net` above is: two units, input piped
    // into the first, `connect`, output piped from the second.
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let fa = g.add_pure_unit(Box::new(lowpass_hz(700.0, 0.8)));
    let fb = g.add_pure_unit(Box::new(lowpass_hz(2_300.0, 1.1)));
    g.pipe_input(fa).connect(fa, 0, fb, 0).pipe_output(fb);
    let mut r = g.renderer(prepare(256)).expect("commits");
    // Aliased in place: the adapter opts in, and this exercises its path.
    assert!(r.executor().plan().unwrap().in_place(fb).get(0));

    r.set_block(Samples(200));
    let got = r.render_input(&[&input]).remove(0);

    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&got), bits(&want));
    assert!(got.iter().any(|&x| x != 0.0));
}

/// A unit reporting a fractional latency through `route`, as fundsp derives
/// it — the only way to pin the rounding, since the library's own latent
/// nodes round internally and report whole frames.
#[derive(Clone)]
struct FractionalLatency;

impl AudioUnit for FractionalLatency {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0];
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        let src = input.channel_f32(0);
        output.channel_f32_mut(0)[..size].copy_from_slice(&src[..size]);
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
        out.set(0, tutti_node::signal::Signal::Latency(2.5));
        out
    }
    fn get_id(&self) -> u64 {
        0x4652_4143
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

/// A fractional latency rounds exactly as `Net`'s `LatencyGraph` impl rounds
/// it (`fundsp-tutti/src/latency/mod.rs:51`): 2.5 frames is 3.
///
/// Mutation: floor instead of round in `Legacy::probe` → 2 → fails.
#[test]
fn legacy_rounds_latency_as_net_does() {
    let (mut node, ()) = Legacy::new(FractionalLatency).into_node();
    node.prepare(&prepare(64));
    assert_eq!(node.shape().latency, Latency::new(Samples(3)));
    let mut net = Net::new(1, 1);
    let id = net.push(Box::new(FractionalLatency));
    net.pipe_input(id);
    net.pipe_output(id);
    assert_eq!(
        tutti_types::latency::plan(&net).total(),
        node.shape().latency.samples()
    );
}

/// The adapter declares the unit's latency and its own tail, **at the
/// prepared rate**: a lookahead is a time, so the same limiter is 445 frames
/// late at 44.1 kHz and 485 at 48 kHz.
///
/// Mutation: drop the re-probe from `Legacy::prepare` → the shape keeps the
/// construction-time (44.1 kHz) figure → fails. Mutation: report
/// `Tail::Unknown` instead of `unit.tail()` → fails.
#[test]
fn legacy_declares_the_units_latency_and_tail() {
    let mut probe = limiter(0.0101, 0.01);
    probe.set_sample_rate(SampleRate(48_000.0));
    let reported = probe.latency().expect("a limiter reports latency");
    let (mut node, ()) = Legacy::new(limiter(0.0101, 0.01)).into_node();
    let before = node.shape().latency;
    node.prepare(&prepare(64));
    let shape = node.shape();
    assert_ne!(before, shape.latency, "the rate moved the latency");
    assert_eq!(
        shape.latency,
        Latency::new(Samples(reported.round() as usize))
    );
    assert!(!shape.latency.is_zero());
    assert_eq!(shape.tail, probe.tail(), "the unit's own tail, unchanged");
    assert_ne!(shape.tail, Tail::Unknown, "and this one does report one");
    assert!(shape.in_place);
}

/// Halves its input, reports `Tail::None`, and counts `process` calls.
#[derive(Clone)]
struct Half(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl AudioUnit for Half {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0] * 0.5;
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let src = input.channel_f32(0);
        for (o, &i) in output.channel_f32_mut(0)[..size]
            .iter_mut()
            .zip(&src[..size])
        {
            *o = i * 0.5;
        }
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
        out.set(0, tutti_node::signal::Signal::Latency(0.0));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::None
    }
    fn get_id(&self) -> u64 {
        0x4841_4c46
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

/// A [`Legacy::pure`] node reports the silence its unit produced, so an
/// `AudioUnit` that declares a tail is skipped on silent input — the skip
/// needs the last output flagged silent. (A default `Legacy` makes no such
/// claim and is never skipped; see the out-of-band test below.)
///
/// Mutation: build `add_pure_unit` without `assume_pure` → fails.
/// Mutation: return `Status::Modified` from `Legacy::process` for pure units
/// too → the unit runs every block → fails.
#[test]
fn a_pure_legacy_reports_silence_so_a_silent_unit_is_skipped() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let key = g.add_pure_unit(Box::new(Half(std::sync::Arc::clone(&calls))));
    g.disconnect(key, 0).pipe_output(key);
    let mut r = g.renderer(prepare(64)).expect("commits");
    r.render_input(&[&[0.0; 640]]);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "called once, found silent, then skipped"
    );
}

/// A unit fed **out of band**: it adds `level` (a shared cell a control
/// thread writes, as a SoundFont's channel or a plugin's MIDI queue would feed
/// it) to its audio inputs, and declares `Tail::None` — honest for what its
/// *inputs* can do, which is exactly why the silence skip would park it.
#[derive(Clone)]
struct OutOfBand {
    inputs: usize,
    outputs: usize,
    level: Arc<AtomicU32>,
    calls: Arc<AtomicUsize>,
}

impl OutOfBand {
    fn new(inputs: usize, outputs: usize) -> Self {
        Self {
            inputs,
            outputs,
            level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl AudioUnit for OutOfBand {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let level = f32::from_bits(self.level.load(Ordering::Relaxed));
        for (c, o) in output.iter_mut().enumerate() {
            *o = level + input.get(c).copied().unwrap_or(0.0);
        }
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let level = f32::from_bits(self.level.load(Ordering::Relaxed));
        for c in 0..self.outputs {
            for i in 0..size {
                let x = if c < self.inputs {
                    input.channel_f32(c)[i]
                } else {
                    0.0
                };
                output.channel_f32_mut(c)[i] = level + x;
            }
        }
    }
    fn inputs(&self) -> usize {
        self.inputs
    }
    fn outputs(&self) -> usize {
        self.outputs
    }
    fn route(
        &mut self,
        _input: &tutti_node::signal::SignalFrame,
        _frequency: f64,
    ) -> tutti_node::signal::SignalFrame {
        let mut out = tutti_node::signal::SignalFrame::new(self.outputs);
        for c in 0..self.outputs {
            out.set(c, tutti_node::signal::Signal::Latency(0.0));
        }
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::None
    }
    fn get_id(&self) -> u64 {
        0x4f4f_4221
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

/// `node` at key 1, its `inputs` wired to silence, its first output the
/// graph's; 64-frame blocks.
fn out_of_band_graph(node: Legacy, inputs: usize) -> (Editor, tutti_graph::Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let key = NodeKey(1);
    ed.insert(key, "oob", node);
    for port in 0..inputs as u16 {
        ed.spec_mut()
            .topology
            .edges
            .insert(InPort { node: key, port }, Edge::Direct(Source::Zero));
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec)
}

/// **A default `Legacy` fed out of band is heard after any amount of
/// silence.** Silent for ten blocks, then its cell is written: the next
/// block carries the level. Run for a unit with an audio input wired to
/// silence (the live hazard: a plugin instrument with a sidechain) and for a
/// 0-input source (a SoundFont, a mic monitor).
///
/// Mutation: make `Legacy::new` pure (`pure: true` in `from_box`), so it
/// reports `Masked { silent }` again → the 1-input unit is parked after its
/// first silent block and stays silent forever → fails. The 0-input case
/// does not fail on that mutation alone — the executor never skips a node
/// without audio inputs — and fails when that rule (`!ain.is_empty()` in
/// `exec.rs`'s `node_op`) is dropped as well: it pins that the two layers
/// agree, not the adapter alone.
#[test]
fn a_default_legacy_fed_out_of_band_is_heard_after_silence() {
    for inputs in [1usize, 0] {
        let unit = OutOfBand::new(inputs, 1);
        let (level, calls) = (Arc::clone(&unit.level), Arc::clone(&unit.calls));
        let (_ed, mut exec) = out_of_band_graph(Legacy::new(unit), inputs);
        let mut out = vec![0.0f32; 64];
        let silence = [0.0f32; 64];
        let ins: &[&[f32]] = &[&silence];
        for _ in 0..10 {
            exec.process(64, &Transport::default(), ins, &mut [&mut out[..]]);
            assert!(out.iter().all(|&x| x == 0.0));
        }
        level.store(0.5f32.to_bits(), Ordering::Relaxed);
        exec.process(64, &Transport::default(), ins, &mut [&mut out[..]]);
        assert!(
            out.iter().all(|&x| x == 0.5),
            "{inputs}-input unit: the out-of-band level must reach the output \
             on the next block; got {:?}",
            &out[..4]
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            11,
            "{inputs}-input unit: called every block"
        );
    }
}

/// The same unit, declared [`Legacy::pure`], **is** parked — which is what
/// the declaration means, and why it is opt-in. Pins that `pure` is the
/// switch, so the test above is not passing because nothing is ever skipped.
///
/// Mutation: ignore `pure` in `Legacy::process` (always `Modified`) → the
/// unit is called every block and the late level is heard → fails.
#[test]
fn a_pure_legacy_fed_out_of_band_is_parked() {
    let unit = OutOfBand::new(1, 1);
    let (level, calls) = (Arc::clone(&unit.level), Arc::clone(&unit.calls));
    let (_ed, mut exec) = out_of_band_graph(Legacy::pure(unit), 1);
    let mut out = vec![0.0f32; 64];
    let silence = [0.0f32; 64];
    let ins: &[&[f32]] = &[&silence];
    for _ in 0..10 {
        exec.process(64, &Transport::default(), ins, &mut [&mut out[..]]);
    }
    level.store(0.5f32.to_bits(), Ordering::Relaxed);
    exec.process(64, &Transport::default(), ins, &mut [&mut out[..]]);
    assert_eq!(calls.load(Ordering::Relaxed), 1, "called once, then parked");
    assert!(out.iter().all(|&x| x == 0.0), "and so never heard again");
}

/// A node with **no outputs** is never parked, even pure with a silent input
/// and `Tail::None`: it is a sink, called for its side effects (a meter, a
/// tap), and "every output silent" is vacuous for it.
///
/// Mutation: drop the `!(aout.is_empty() && eout.is_empty())` term from
/// `last_quiet` in `exec.rs`'s `node_op` → called once, then skipped →
/// fails.
#[test]
fn a_sink_is_never_parked() {
    let unit = OutOfBand::new(1, 0);
    let calls = Arc::clone(&unit.calls);
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let key = NodeKey(1);
    ed.insert(key, "sink", Legacy::pure(unit));
    // `Source::Zero`, which the executor knows is silent. (A global input is
    // not flagged silent, so a sink fed one would run regardless.)
    ed.spec_mut()
        .topology
        .edges
        .insert(InPort { node: key, port: 0 }, Edge::Direct(Source::Zero));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    for _ in 0..10 {
        exec.process(64, &Transport::default(), &[], &mut []);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 10, "a sink runs every block");
}

/// Holds up to four by-value params as **plain fields**, set through
/// `AudioUnit::set` (`Setting::value(v).index(i)`), outputs param 0, and logs
/// every setting it applies — the shape of the sampler voice's `play.gain`,
/// whose write is lost anywhere but on the copy that renders. Only the
/// original logs: a clone (the shadow) does not, so the log is what the
/// rendering copy applied.
struct Params {
    values: [f32; 4],
    log: Option<Arc<std::sync::Mutex<Vec<(usize, f32)>>>>,
}

impl Clone for Params {
    fn clone(&self) -> Self {
        Self {
            values: self.values,
            log: None,
        }
    }
}

impl Params {
    fn new() -> Self {
        Self {
            values: [0.0; 4],
            log: Some(Arc::new(std::sync::Mutex::new(Vec::new()))),
        }
    }
}

fn param(index: usize, value: f32) -> tutti_node::Setting {
    tutti_node::Setting::value(value).index(index)
}

impl AudioUnit for Params {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.values[0];
    }
    fn process(
        &mut self,
        size: usize,
        _input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        output.channel_f32_mut(0)[..size].fill(self.values[0]);
    }
    /// `Value(v)` at `Index(i)` sets param `i`. As a filter's `set` does,
    /// `Center(c)` sets param 0, and a `CenterQ`-shaped setting sets params 0
    /// and 1 — two setting kinds that write one field, the case that makes
    /// coalescing order matter. (`Setting` has no `center_q` constructor
    /// since Phase 0b, so `Biquad(c, q, ..)` stands in for `CenterQ(c, q)`.)
    fn set(&mut self, setting: tutti_node::Setting) {
        let mut apply = |i: usize, v: f32| {
            self.values[i] = v;
            if let Some(log) = &self.log {
                log.lock().unwrap().push((i, v));
            }
        };
        match (setting.parameter(), setting.direction()) {
            (tutti_node::Parameter::Value(v), tutti_node::Address::Index(i)) => apply(i, *v),
            (tutti_node::Parameter::Center(c), _) => apply(0, *c),
            (tutti_node::Parameter::Biquad(c, q, ..), _) => {
                apply(0, *c);
                apply(1, *q);
            }
            _ => {}
        }
    }
    fn inputs(&self) -> usize {
        0
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
        out.set(0, tutti_node::signal::Signal::Latency(0.0));
        out
    }
    fn get_id(&self) -> u64 {
        0x5041_5241
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

/// A `Legacy::controlled` `Params` at key 1, feeding output 0.
fn controlled_graph() -> (
    Editor,
    tutti_graph::Executor,
    LegacyControls<Params>,
    Arc<std::sync::Mutex<Vec<(usize, f32)>>>,
) {
    let unit = Params::new();
    let log = Arc::clone(unit.log.as_ref().expect("the original logs"));
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let (node, controls) = Legacy::controlled(&mut ed, unit);
    let key = NodeKey(1);
    ed.insert(key, "params", node);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, controls, log)
}

/// **A setting lands on the next block, on the copy that renders, and on the
/// shadow at once** — the `Net::set` path, for a unit whose `set` writes a
/// plain field.
///
/// Mutation: drop the drain at the top of `Legacy::process` → the level
/// stays 0 → fails. Mutation: skip `self.shadow().set(..)` in
/// `LegacyControls::set` → the shadow keeps 0 → fails.
#[test]
fn a_controlled_setting_lands_on_the_next_block() {
    let (_ed, mut exec, mut controls, _log) = controlled_graph();
    let mut out = vec![0.0f32; 64];
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert!(out.iter().all(|&x| x == 0.0));

    assert_eq!(controls.set(param(0, 0.5)), Delivery::Queued);
    assert_eq!(
        controls.shadow().values[0],
        0.5,
        "the shadow has it before any block runs"
    );
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert!(out.iter().all(|&x| x == 0.5), "got {:?}", &out[..4]);
}

/// **A full ring holds and coalesces, and never drops or reorders.** With no
/// block run, `LEGACY_SETTINGS_CAPACITY` settings fill the ring; the next ones
/// are held, one per parameter (a second value for param 1 replaces the
/// first). After a block has drained the ring, the next `set` sends what is
/// held *first*, then itself.
///
/// Mutation: in `LegacyControls::set`, return `Held` without keeping the
/// setting → param 2 never arrives → fails. Mutation: push a new held entry
/// instead of replacing the matching one → the unit sees `(1, 10.0)` →
/// fails. Mutation: try the new setting before flushing what is held → it
/// arrives ahead of them → fails.
#[test]
fn a_full_ring_holds_and_coalesces_and_never_drops() {
    let (_ed, mut exec, mut controls, log) = controlled_graph();
    for k in 0..LEGACY_SETTINGS_CAPACITY {
        assert_eq!(controls.set(param(0, k as f32)), Delivery::Queued);
    }
    assert_eq!(controls.set(param(1, 10.0)), Delivery::Held);
    assert_eq!(controls.set(param(1, 11.0)), Delivery::Held);
    assert_eq!(controls.set(param(2, 20.0)), Delivery::Held);
    assert_eq!(controls.held(), 2, "coalesced per parameter");
    assert_eq!(controls.shadow().values, [63.0, 11.0, 20.0, 0.0]);

    let mut out = vec![0.0f32; 64];
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert_eq!(log.lock().unwrap().len(), LEGACY_SETTINGS_CAPACITY);
    assert!(out.iter().all(|&x| x == 63.0));

    assert_eq!(controls.set(param(3, 30.0)), Delivery::Queued);
    assert_eq!(controls.held(), 0);
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    let log = log.lock().unwrap();
    assert_eq!(
        &log[LEGACY_SETTINGS_CAPACITY..],
        &[(1, 11.0), (2, 20.0), (3, 30.0)],
        "held settings first, in order, the replaced value never sent"
    );
}

/// `flush` sends what is held once there is room, and answers `Queued` only
/// when nothing is left.
///
/// Mutation: make `flush` return `Queued` unconditionally → the first
/// `flush`, with the ring still full, says `Queued` → fails.
#[test]
fn flush_sends_what_is_held() {
    let (_ed, mut exec, mut controls, log) = controlled_graph();
    for k in 0..LEGACY_SETTINGS_CAPACITY {
        let _ = controls.set(param(0, k as f32));
    }
    assert_eq!(controls.set(param(1, 1.0)), Delivery::Held);
    assert_eq!(controls.flush(), Delivery::Held, "still no room");
    let mut out = vec![0.0f32; 64];
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert_eq!(controls.flush(), Delivery::Queued);
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert_eq!(log.lock().unwrap().last(), Some(&(1, 1.0)));
}

/// Fill the ring with `LEGACY_SETTINGS_CAPACITY` settings for param 3, so
/// whatever is sent next is held.
fn fill(controls: &mut LegacyControls<Params>) {
    for k in 0..LEGACY_SETTINGS_CAPACITY {
        assert_eq!(controls.set(param(3, k as f32)), Delivery::Queued);
    }
}

/// **Coalescing never reorders: A, B, A delivers B then A.** With the ring
/// full, `A = 1`, `B = 2`, `A = 3` are held; what reaches the unit is each
/// parameter's last value at its last send's position — a subsequence of
/// what was sent.
///
/// Mutation: coalesce by replacing the held entry in place (the old rule) →
/// the unit sees `A = 3` before `B` → fails.
#[test]
fn coalescing_keeps_last_occurrence_order() {
    let (mut ed, mut exec, mut controls, log) = controlled_graph();
    fill(&mut controls);
    for (i, v) in [(0, 1.0), (1, 2.0), (0, 3.0)] {
        assert_eq!(controls.set(param(i, v)), Delivery::Held);
    }
    let mut out = vec![0.0f32; 64];
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    ed.collect();
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    let log = log.lock().unwrap();
    assert_eq!(&log[LEGACY_SETTINGS_CAPACITY..], &[(1, 2.0), (0, 3.0)]);
}

/// **Two setting kinds that write one field end where the shadow ends.**
/// With the ring full, `Center(1000)`, `CenterQ(500, 0.7)` (spelled as a
/// `Biquad`, see below), `Center(2000)`:
/// the shadow applied all three, so its cutoff is 2000. Replacing the held
/// `Center` in place would send `Center(2000)` before `CenterQ`, leaving the
/// unit at 500 for good; moving it to the back keeps the two in step.
///
/// Mutation: coalesce in place → the unit ends at 500 while the shadow says
/// 2000 → fails.
#[test]
fn a_coalesced_setting_leaves_the_unit_where_the_shadow_is() {
    let (mut ed, mut exec, mut controls, _log) = controlled_graph();
    fill(&mut controls);
    let _ = controls.set(tutti_node::Setting::center(1_000.0));
    // `CenterQ(500, 0.7)`: `Setting` lost its `center_q` constructor in
    // Phase 0b, so the test unit reads `Biquad`'s first two fields as the
    // same (cutoff, Q) pair — another kind that writes the cutoff field.
    let _ = controls.set(tutti_node::Setting::biquad(500.0, 0.7, 0.0, 0.0, 0.0));
    let _ = controls.set(tutti_node::Setting::center(2_000.0));
    assert_eq!(controls.shadow().values[0], 2_000.0);
    let mut out = vec![0.0f32; 64];
    for _ in 0..2 {
        exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
        ed.collect();
    }
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert!(
        out.iter().all(|&x| x == 2_000.0),
        "the unit renders the shadow's cutoff; got {}",
        out[0]
    );
}

/// **A burst that fills the ring and then goes quiet is delivered by the
/// editor**, with no further `set` or `flush`: `Editor::collect` flushes
/// every controlled node built for it.
///
/// Mutation: drop the outbox flush from `Editor::collect` → the held
/// settings never arrive → fails. Mutation: skip `register_outbox` in
/// `Legacy::controlled` → fails.
#[test]
fn a_quiet_burst_is_delivered_after_the_next_collect() {
    let (mut ed, mut exec, mut controls, log) = controlled_graph();
    fill(&mut controls);
    for i in 0..3 {
        assert_eq!(controls.set(param(i, 10.0 + i as f32)), Delivery::Held);
    }
    let mut out = vec![0.0f32; 64];
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    ed.collect();
    assert_eq!(controls.held(), 0, "flushed by the editor");
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    let log = log.lock().unwrap();
    assert_eq!(
        &log[LEGACY_SETTINGS_CAPACITY..],
        &[(0, 10.0), (1, 11.0), (2, 12.0)]
    );
}

/// A native node with a `Legacy` node's outward shape (one channel in place,
/// `Block` event resolution), but not `legacy`: what `replace` must refuse
/// to fade a `Legacy` node into.
struct NativeLookalike;

impl tutti_graph::Node for NativeLookalike {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_in_place()
            .with_event_resolution(tutti_graph::Resolution::Block)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &tutti_graph::Cx<'_>, _: tutti_graph::Io<'_>) -> tutti_graph::Status {
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A `Legacy` unit marks its plan**, and nothing else does: its shape is
/// `legacy`, a plan holding one `has_legacy` (the renderers' cue to render
/// chunk-major, doc 013's `Legacy` compatibility mode), a plan without one
/// does not. And a crossfade may not change it: fading a `Legacy` node into
/// a native one of the same ports would leave the outgoing unit running in
/// whole blocks, so `replace` refuses it.
///
/// Mutations (run): `Legacy`'s probe not calling `with_legacy` → the plan
/// does not report it → fails; `Editor::replace` not comparing `legacy` →
/// the lookalike fades in → fails.
#[test]
fn a_legacy_unit_marks_its_plan() {
    let node = Legacy::new(lowpass_hz(1_000.0, 1.0));
    assert!(node.shape().legacy, "a `Legacy` shape is legacy");
    let (mut ed, mut exec) = out_of_band_graph(node, 1);
    assert!(exec.plan().expect("applied").has_legacy());
    assert_eq!(
        ed.replace(
            NodeKey(1),
            NativeLookalike,
            tutti_graph::Fade::new(Samples(64), tutti_graph::CrossfadeCurve::EqualAmplitude),
        )
        .map(|_| ()),
        Err(tutti_graph::CommitError::FadeShape { node: NodeKey(1) })
    );

    ed.remove(NodeKey(1));
    ed.insert(NodeKey(2), "native", NativeLookalike);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(2),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    assert!(
        !exec.plan().expect("applied").has_legacy(),
        "a native node does not mark its plan"
    );
}

/// A gain whose `Volume` the graph may modulate through a `ParamFeed`: its
/// own control is `base`, read once per call; a live feed replaces it per
/// frame. Outputs `input * volume`.
#[derive(Clone)]
struct FedGain {
    base: Arc<AtomicU32>,
    feed: tutti_node::ParamFeed,
    /// Calls that read the feed, and calls that read the base.
    fed: Arc<AtomicUsize>,
    based: Arc<AtomicUsize>,
}

static FED_PARAMS: [tutti_types::UnitParam; 1] = [tutti_types::UnitParam::Volume];

impl FedGain {
    fn new(base: f32) -> Self {
        Self {
            base: Arc::new(AtomicU32::new(base.to_bits())),
            feed: tutti_node::ParamFeed::new(&FED_PARAMS),
            fed: Arc::default(),
            based: Arc::default(),
        }
    }
}

impl AudioUnit for FedGain {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0] * f32::from_bits(self.base.load(Ordering::Relaxed));
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        let src = input.channel_f32(0);
        match self.feed.get(0, size) {
            Some(v) => {
                self.fed.fetch_add(1, Ordering::Relaxed);
                let out = &mut output.channel_f32_mut(0)[..size];
                for ((o, &x), &g) in out.iter_mut().zip(&src[..size]).zip(v) {
                    *o = x * g;
                }
            }
            None => {
                self.based.fetch_add(1, Ordering::Relaxed);
                let g = f32::from_bits(self.base.load(Ordering::Relaxed));
                let out = &mut output.channel_f32_mut(0)[..size];
                for (o, &x) in out.iter_mut().zip(&src[..size]) {
                    *o = x * g;
                }
            }
        }
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
        out.set(0, tutti_node::signal::Signal::Latency(0.0));
        out
    }
    fn get_id(&self) -> u64 {
        0x4645_4447
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn param_feed(&mut self) -> Option<&mut tutti_node::ParamFeed> {
        Some(&mut self.feed)
    }
    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| f32::from_bits(self.base.load(Ordering::Relaxed)))
    }
}

/// `Legacy` bridges compiler-owned modulation to an `AudioUnit`'s
/// `ParamFeed`: the feed's params are the node's param ports, an
/// unmodulated param leaves the feed clear (the unit reads its own control,
/// every chunk), and a modulated one is fed chunk by chunk — `base +
/// offset` at every frame, across the adapter's 64-frame chunks of a
/// 200-frame block.
///
/// Mutation (run): in `Legacy::process`, feed every chunk from frame 0 of
/// the block (`&v[..len]`) → the second chunk repeats the first's values →
/// fails. Never clear the feed (drop the `Base` arm's `clear`) → after the
/// disconnect the unit keeps reading the stale feed → fails. Declare no
/// params in `Legacy::probe` → the connect is refused at compile → fails.
#[test]
fn a_legacy_units_param_feed_carries_the_modulation() {
    use tutti_graph::{ParamFrom, ParamIn, ParamShaping, PARAM_DECLICK};
    let unit = FedGain::new(0.5);
    let (fed, based) = (Arc::clone(&unit.fed), Arc::clone(&unit.based));
    let node = Legacy::new(unit);
    assert_eq!(
        node.shape().params.as_slice(),
        &[tutti_types::UnitParam::Volume],
        "the feed's params are the node's param ports"
    );
    let (mut ed, mut exec) = Editor::new(prepare(200));
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(2);
    ed.insert(NodeKey(1), "fed", node);
    let t = &mut ed.spec_mut().topology;
    t.edges.insert(
        InPort {
            node: NodeKey(1),
            port: 0,
        },
        Edge::Direct(Source::Global(0)),
    );
    t.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    // A pass-through for global input 1, the modulator.
    ed.insert(NodeKey(2), "pass", Legacy::new(FedGain::new(1.0)));
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(2),
            port: 0,
        },
        Edge::Direct(Source::Global(1)),
    );
    ed.commit().expect("commits");

    let ones = vec![1.0f32; 200];
    let ramp: Vec<f32> = (0..200).map(|i| i as f32 / 1000.0).collect();
    let block = |exec: &mut tutti_graph::Executor| {
        let mut out = vec![0.0f32; 200];
        exec.process(
            200,
            &Transport::default(),
            &[&ones, &ramp],
            &mut [&mut out[..]],
        );
        out
    };
    let out = block(&mut exec);
    assert!(
        out.iter().all(|&x| x == 0.5),
        "unmodulated: the unit's own base"
    );
    assert_eq!(fed.load(Ordering::Relaxed), 0);
    assert!(based.load(Ordering::Relaxed) > 0);

    let at = ParamIn {
        node: NodeKey(1),
        param: tutti_types::UnitParam::Volume,
    };
    let from = ParamFrom::Audio(OutPort {
        node: NodeKey(2),
        port: 0,
    });
    ed.spec_mut()
        .connect_param(at, from, ParamShaping::Identity);
    ed.commit().expect("connects");
    // Past the connection's declick.
    for _ in 0..=PARAM_DECLICK.get() / 200 {
        block(&mut exec);
    }
    let out = block(&mut exec);
    for (i, &x) in out.iter().enumerate() {
        assert_eq!(x, 0.5 + ramp[i], "frame {i}: base + offset, per frame");
    }
    assert!(fed.load(Ordering::Relaxed) > 0, "the feed was read");

    ed.spec_mut().disconnect_param(at, from);
    ed.commit().expect("disconnects");
    for _ in 0..=PARAM_DECLICK.get() / 200 + 1 {
        block(&mut exec);
    }
    let before = based.load(Ordering::Relaxed);
    let out = block(&mut exec);
    assert!(out.iter().all(|&x| x == 0.5), "back on its own base");
    assert!(
        based.load(Ordering::Relaxed) > before,
        "and reading its own control again"
    );
}
