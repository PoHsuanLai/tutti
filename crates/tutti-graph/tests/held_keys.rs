//! Keys held across an edge change: the sink that loses an edge is sent the
//! offs of what the edge's source holds. A sink that gains an edge is told
//! nothing; the later off of a note it never heard begin reaches it.
//!
//! Each scenario runs on the serial executor, on the parallel one and on the
//! reference interpreter, and the three must log the same events.

mod common;

use std::sync::{Arc, Mutex};

use common::TestPool;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, Fade, Io, Node, Offset,
    Prepare, Reference, Shape, Shapes, Status, Transport, Unforkable,
};
use tutti_types::graph::NodeSpec;
use tutti_types::{ChannelLayout, NodeKey, Samples, Topology};
use tutti_ump::Ump;

type Words = [u32; 4];
type Mail = Arc<Mutex<Vec<(u32, Words)>>>;
/// `(sink, frame, words)`.
type Log = Arc<Mutex<Vec<(u64, u64, Words)>>>;

const MAX: usize = 64;

struct Src {
    mail: Mail,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut m = self.mail.lock().unwrap();
        for (_, w) in m.drain(..) {
            // A unit fading out has detached writers: its pushes are refused.
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, w));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

struct Sink {
    id: u64,
    log: Log,
    audio: bool,
}

const LAG: u64 = 9;
const LAG_FRAMES: usize = 100;

struct Lag;

impl Node for Lag {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_latency(tutti_types::Latency::new(Samples(LAG_FRAMES)))
            .with_tail(tutti_types::Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Silent
    }
    fn reset(&mut self) {}
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        let a = if self.audio {
            ChannelLayout::MONO
        } else {
            ChannelLayout::EMPTY
        };
        Shape::audio(a, ChannelLayout::EMPTY).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut log = self.log.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                log.push((self.id, cx.env.frame_at(e.offset).get(), w));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn out(n: u64) -> EventOut {
    EventOut {
        node: NodeKey(n),
        port: 0,
    }
}
fn at(n: u64) -> EventIn {
    EventIn {
        node: NodeKey(n),
        port: 0,
    }
}

const ON60: Words = [0x2090_3c64, 0, 0, 0];
const OFF60: Words = [0x2080_3c00, 0, 0, 0];
const ON61: Words = [0x2090_3d64, 0, 0, 0];
const OFF61: Words = [0x2080_3d00, 0, 0, 0];

/// Which engine a scenario runs on.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Serial,
    Parallel,
    Reference,
}

const MODES: [Mode; 3] = [Mode::Serial, Mode::Parallel, Mode::Reference];

/// The engine under a scenario: a source `1` and sinks `2..`, edges set with
/// `wire`, blocks with `block`.
struct Rig {
    ed: Option<Editor>,
    exec: Option<Executor>,
    reference: Option<Reference>,
    mail: Mail,
    log: Log,
    edges: Vec<(u64, u64)>,
    sinks: Vec<u64>,
    src_gen: u32,
    frame: u64,
    mode: Mode,
    /// Sinks take an audio input from a latent node, so every event edge
    /// into them runs through a PDC delay.
    lag: bool,
}

impl Rig {
    fn new(mode: Mode, sinks: &[u64]) -> Self {
        Self::with_lag(mode, sinks, false)
    }

    fn with_lag(mode: Mode, sinks: &[u64], lag: bool) -> Self {
        let prep = common::prepare(MAX);
        let mail: Mail = Arc::default();
        let log: Log = Arc::default();
        let (ed, exec, reference) = match mode {
            Mode::Reference => (None, None, Some(Reference::new(prep))),
            _ => {
                let (ed, mut exec) = Editor::with_event_capacity(prep, 64);
                if matches!(mode, Mode::Parallel) {
                    exec.set_pool(Some(TestPool::new(2)));
                }
                (Some(ed), Some(exec), None)
            }
        };
        let mut r = Self {
            ed,
            exec,
            reference,
            mail,
            log,
            edges: vec![],
            sinks: sinks.to_vec(),
            src_gen: 0,
            frame: 0,
            mode,
            lag,
        };
        if let Some(ed) = &mut r.ed {
            ed.insert(
                NodeKey(1),
                "src",
                Unforkable(Src {
                    mail: r.mail.clone(),
                }),
            );
            if lag {
                ed.insert(NodeKey(LAG), "lag", Unforkable(Lag));
            }
            for &s in sinks {
                ed.insert(
                    NodeKey(s),
                    "sink",
                    Unforkable(Sink {
                        id: s,
                        log: r.log.clone(),
                        audio: lag,
                    }),
                );
                if lag {
                    ed.spec_mut().topology.edges.insert(
                        tutti_types::graph::InPort {
                            node: NodeKey(s),
                            port: 0,
                        },
                        tutti_types::graph::Edge::Direct(tutti_types::graph::Source::Node(
                            tutti_types::graph::OutPort {
                                node: NodeKey(LAG),
                                port: 0,
                            },
                        )),
                    );
                }
            }
        }
        r
    }

    fn units(&self, keys: &[u64]) -> std::collections::BTreeMap<NodeKey, Box<dyn Node>> {
        let mut m: std::collections::BTreeMap<NodeKey, Box<dyn Node>> = Default::default();
        for &k in keys {
            m.insert(
                NodeKey(k),
                if k == 1 {
                    Box::new(Src {
                        mail: self.mail.clone(),
                    })
                } else if k == LAG {
                    Box::new(Lag)
                } else {
                    Box::new(Sink {
                        id: k,
                        log: self.log.clone(),
                        audio: self.lag,
                    })
                },
            );
        }
        m
    }

    /// Set the edge list and commit.
    fn wire(&mut self, edges: &[(u64, u64)]) {
        self.edges = edges.to_vec();
        match self.mode {
            Mode::Reference => {
                let mut t = Topology::default();
                let mut shapes = Shapes::new();
                let all: Vec<u64> = std::iter::once(1u64)
                    .chain(self.lag.then_some(LAG))
                    .chain(self.sinks.iter().copied())
                    .collect();
                for k in all {
                    let sh = if k == 1 {
                        Src {
                            mail: self.mail.clone(),
                        }
                        .shape()
                    } else if k == LAG {
                        Lag.shape()
                    } else {
                        Sink {
                            id: k,
                            log: self.log.clone(),
                            audio: self.lag,
                        }
                        .shape()
                    };
                    t.nodes.insert(
                        NodeKey(k),
                        NodeSpec::new("n", sh.audio_in, sh.audio_out)
                            .with_latency(sh.latency.samples())
                            .with_tail(sh.tail),
                    );
                    shapes.insert(NodeKey(k), sh);
                }
                if self.lag {
                    for &s in &self.sinks {
                        t.edges.insert(
                            tutti_types::graph::InPort {
                                node: NodeKey(s),
                                port: 0,
                            },
                            tutti_types::graph::Edge::Direct(tutti_types::graph::Source::Node(
                                tutti_types::graph::OutPort {
                                    node: NodeKey(LAG),
                                    port: 0,
                                },
                            )),
                        );
                    }
                }
                let mut g = tutti_graph::GraphSpec::new(t);
                for &(f, to) in edges {
                    g.connect_events(at(to), EventEdge::Direct(out(f)));
                }
                g.generations.insert(NodeKey(1), self.src_gen);
                let valid = g.validate().expect("valid");
                let keys: Vec<u64> = std::iter::once(1)
                    .chain(self.lag.then_some(LAG))
                    .chain(self.sinks.iter().copied())
                    .collect();
                let units = self.units(&keys);
                self.reference.as_mut().unwrap().set_graph(&valid, units);
                let _ = &shapes;
            }
            _ => {
                let ed = self.ed.as_mut().unwrap();
                let spec = ed.spec_mut();
                spec.events.clear();
                for &(f, to) in edges {
                    spec.connect_events(at(to), EventEdge::Direct(out(f)));
                }
                ed.commit().expect("commits");
                let exec = self.exec.as_mut().unwrap();
                exec.apply_pending();
                ed.collect();
                if matches!(self.mode, Mode::Parallel) && self.sinks.len() > 1 && !edges.is_empty()
                {
                    assert!(exec.is_parallel(), "the parallel mode runs parallel");
                }
            }
        }
    }

    /// Replace the source with a new unit (a new generation).
    fn replace_source(&mut self) {
        self.src_gen += 1;
        match self.mode {
            Mode::Reference => {
                let edges = self.edges.clone();
                self.wire(&edges);
            }
            _ => {
                let ed = self.ed.as_mut().unwrap();
                ed.replace(
                    NodeKey(1),
                    Unforkable(Src {
                        mail: self.mail.clone(),
                    }),
                    Fade::new(Samples(0), Default::default()),
                )
                .ok();
                ed.commit().ok();
                self.exec.as_mut().unwrap().apply_pending();
                ed.collect();
            }
        }
    }

    fn remove_source(&mut self) {
        match self.mode {
            Mode::Reference => panic!("not modelled in this harness"),
            _ => {
                let ed = self.ed.as_mut().unwrap();
                ed.remove(NodeKey(1));
                ed.commit().expect("commits");
                self.exec.as_mut().unwrap().apply_pending();
                ed.collect();
            }
        }
    }

    fn send(&self, w: Words) {
        self.mail.lock().unwrap().push((0, w));
    }

    /// Render one block of `MAX` frames.
    fn block(&mut self) {
        let t = Transport::default();
        match self.mode {
            Mode::Reference => self
                .reference
                .as_mut()
                .unwrap()
                .process(MAX, &t, &[], &mut []),
            _ => self.exec.as_mut().unwrap().process(MAX, &t, &[], &mut []),
        }
        self.frame += MAX as u64;
    }

    /// What `sink` has received, `(frame, words)`.
    fn got(&self, sink: u64) -> Vec<(u64, Words)> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.0 == sink)
            .map(|e| (e.1, e.2))
            .collect()
    }
    fn words(&self, sink: u64) -> Vec<Words> {
        self.got(sink).into_iter().map(|e| e.1).collect()
    }
}

/// Run `scenario` on every mode and return each mode's log for the caller to
/// compare.
fn on_all(scenario: impl Fn(&mut Rig), sinks: &[u64]) -> Vec<Vec<(u64, u64, Words)>> {
    MODES
        .iter()
        .map(|&m| {
            let mut r = Rig::new(m, sinks);
            scenario(&mut r);
            r.log.lock().unwrap().clone()
        })
        .collect()
}

/// Per sink, in order: which sink logs first within a block is up to the
/// threads.
fn by_sink(log: &[(u64, u64, Words)]) -> Vec<(u64, u64, Words)> {
    let mut v = log.to_vec();
    v.sort_by_key(|e| e.0);
    v
}

fn same(logs: &[Vec<(u64, u64, Words)>]) {
    for l in &logs[1..] {
        assert_eq!(
            by_sink(&logs[0]),
            by_sink(l),
            "serial, parallel and reference agree"
        );
    }
}

fn sink_words(log: &[(u64, u64, Words)], sink: u64) -> Vec<Words> {
    log.iter().filter(|e| e.0 == sink).map(|e| e.2).collect()
}

/// A held key and a retarget: the old sink gets the off, and the new sink
/// hears the later off of a note it never saw begin.
#[test]
fn retarget_closes_the_old_sink_and_passes_the_orphan_off() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(OFF60);
            r.block();
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![ON60, OFF60],
        "A: note, then its close"
    );
    assert_eq!(
        sink_words(&logs[0], 3),
        vec![OFF60],
        "B hears the orphan off, which ends nothing"
    );
}

/// A cut with no new edge (`unroute`).
#[test]
fn unroute_closes_the_sink() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.send(ON61);
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, ON61, OFF60, OFF61]);
}

/// A fan-out: only the sink that loses the edge is closed; the other keeps
/// its note and hears the real off.
#[test]
fn fan_out_closes_only_the_cut_sink() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2), (1, 3)]);
            r.send(ON60);
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(OFF60);
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60]);
    assert_eq!(sink_words(&logs[0], 3), vec![ON60, OFF60]);
    let a = logs[0].iter().find(|e| e.0 == 2 && e.2 == OFF60).unwrap();
    let b = logs[0].iter().find(|e| e.0 == 3 && e.2 == OFF60).unwrap();
    assert!(
        a.1 < b.1,
        "A's close is at the commit, B's off a block later"
    );
}

/// An edge added to a source that holds a key: the sink hears the stream
/// from then on, orphan off included.
#[test]
fn a_new_edge_hears_the_stream_as_it_comes() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.wire(&[(1, 2), (1, 3)]);
            r.send(OFF60);
            r.send(ON60);
            r.block();
            r.send(OFF60);
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60, ON60, OFF60]);
    assert_eq!(
        sink_words(&logs[0], 3),
        vec![OFF60, ON60, OFF60],
        "B: the orphan off, then the new note whole"
    );
}

/// A sink whose edge is cut while it sits quiet is woken for the close, and
/// a block with no cut and no notes changes nothing.
#[test]
fn no_cut_no_change() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.send(OFF60);
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![ON60, OFF60],
        "nothing held at the cut: nothing sent"
    );
}

/// MIDI 1.0 spells an off as a Note On at velocity 0; MIDI 2.0 does not.
#[test]
fn velocity_zero_is_an_off_in_midi1_and_an_on_in_midi2() {
    let m1_on = [0x2091_4064, 0, 0, 0];
    let m1_on0 = [0x2091_4000, 0, 0, 0];
    let m2_on = [0x4091_4000, 0x8000_0000, 0, 0];
    let m2_on0 = [0x4092_4100, 0, 0, 0];
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(m1_on);
            r.send(m1_on0);
            r.send(m2_on);
            r.send(m2_on0);
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    let got = sink_words(&logs[0], 2);
    assert_eq!(
        &got[4..],
        &[[0x4081_4000, 0, 0, 0], [0x4082_4100, 0, 0, 0]],
        "M1 on then velocity 0 holds nothing; the two M2 notes are closed as M2 offs"
    );
}

/// Group and channel are part of the key.
#[test]
fn group_and_channel_are_part_of_the_key() {
    let g1c2 = [0x2192_3c64, 0, 0, 0];
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.send(g1c2);
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(
        &sink_words(&logs[0], 2)[2..],
        &[OFF60, [0x2182_3c00, 0, 0, 0]]
    );
}

/// A note released under a down pedal sounds on at the sink: the cut sends
/// the pedal up.
#[test]
fn a_held_sustain_pedal_is_released() {
    let down = [0x20b0_407f, 0, 0, 0];
    let up = [0x20b0_4000, 0, 0, 0];
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.send(down);
            r.send(OFF60);
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, down, OFF60, up]);
}

/// The source replaced by a new unit: the sink is closed for the old one's
/// notes, and the new unit's later off is not the old note's.
#[test]
fn a_replaced_source_closes_its_sinks() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.replace_source();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60]);
}

/// A removed source closes the sinks it fed (executor only).
#[test]
fn a_removed_source_closes_its_sinks() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.send(ON60);
    r.block();
    r.remove_source();
    r.block();
    assert_eq!(r.words(2), vec![ON60, OFF60]);
}

/// A sink that goes away with the edge: nothing to send, and nothing breaks.
#[test]
fn a_removed_sink_is_not_sent_anything() {
    let mut r = Rig::new(Mode::Serial, &[2, 3]);
    r.wire(&[(1, 2), (1, 3)]);
    r.send(ON60);
    r.block();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.remove(NodeKey(2));
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    r.send(OFF60);
    r.block();
    assert_eq!(r.words(2), vec![ON60]);
    assert_eq!(r.words(3), vec![ON60, OFF60]);
}

fn on_all_lagged(scenario: impl Fn(&mut Rig), sinks: &[u64]) -> Vec<Vec<(u64, u64, Words)>> {
    MODES
        .iter()
        .map(|&m| {
            let mut r = Rig::with_lag(m, sinks, true);
            scenario(&mut r);
            r.log.lock().unwrap().clone()
        })
        .collect()
}

/// An event in a PDC delay when the edge goes: the flush delivers it, and the
/// close follows it, so the sink sees the on, then the off.
#[test]
fn a_close_follows_the_events_in_flight_on_a_delayed_edge() {
    let logs = on_all_lagged(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.wire(&[]);
            r.block();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60]);
    let f: Vec<u64> = logs[0].iter().map(|e| e.1).collect();
    assert!(f[0] <= f[1], "in order: {f:?}");
}

/// The off in flight too: the on and its off both sit in the delay when the
/// edge goes; the flush delivers both and nothing more is sent.
#[test]
fn a_released_note_in_flight_is_not_closed_twice() {
    let logs = on_all_lagged(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.send(OFF60);
            r.block();
            r.wire(&[]);
            r.block();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60]);
}

/// A new edge behind a delay passes the orphan off through it.
#[test]
fn a_new_delayed_edge_passes_the_orphan_off() {
    let logs = on_all_lagged(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(ON60);
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(OFF60);
            r.block();
            r.block();
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 3), vec![OFF60]);
    assert_eq!(sink_words(&logs[0], 2), vec![ON60, OFF60]);
}

/// Two sources into one sink: cutting one closes only its keys.
#[test]
fn fan_in_closes_the_cut_sources_keys_only() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    let mail2: Mail = Arc::default();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.insert(
            NodeKey(5),
            "src2",
            Unforkable(Src {
                mail: mail2.clone(),
            }),
        );
    }
    let both = |r: &mut Rig, edges: &[(u64, u64)]| {
        let ed = r.ed.as_mut().unwrap();
        let spec = ed.spec_mut();
        spec.events.clear();
        for &(f, to) in edges {
            spec.connect_events(at(to), EventEdge::Direct(out(f)));
        }
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    };
    both(&mut r, &[(1, 2), (5, 2)]);
    r.send(ON60);
    mail2.lock().unwrap().push((0, ON61));
    r.block();
    both(&mut r, &[(5, 2)]);
    r.block();
    assert_eq!(
        r.words(2),
        vec![ON60, ON61, OFF60],
        "only source 1's key closed"
    );
}

/// A crossfade over a source (`Editor::replace` with a fade): the sink is
/// closed for the old unit's notes at the commit.
#[test]
fn a_crossfaded_source_closes_its_sinks() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    let mail2: Mail = Arc::default();
    r.wire(&[(1, 2)]);
    r.send(ON60);
    r.block();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.replace(
            NodeKey(1),
            Unforkable(Src {
                mail: mail2.clone(),
            }),
            Fade::new(Samples(MAX * 3), Default::default()),
        )
        .expect("replace");
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    r.block();
    mail2.lock().unwrap().push((0, ON61));
    r.block();
    r.block();
    r.block();
    assert_eq!(r.words(2), vec![ON60, OFF60, ON61]);
}

mod random {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Step {
        Block,
        Toggle(u8),
        Wire(u8),
        Replace,
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        proptest::collection::vec(
            prop_oneof![
                4 => Just(Step::Block),
                4 => (0u8..6).prop_map(Step::Toggle),
                3 => (0u8..8).prop_map(Step::Wire),
                1 => Just(Step::Replace),
            ],
            1..40,
        )
    }

    fn key_words(k: u8, on: bool) -> Words {
        // Keys vary in note, channel and protocol.
        let note = 60 + u32::from(k % 3);
        let ch = u32::from(k / 3);
        let m2 = k % 2 == 1;
        match (m2, on) {
            (false, true) => [0x2090_0000 | (ch << 16) | (note << 8) | 100, 0, 0, 0],
            (false, false) => [0x2080_0000 | (ch << 16) | (note << 8), 0, 0, 0],
            (true, true) => [0x4090_0000 | (ch << 16) | (note << 8), 0x8000_0000, 0, 0],
            (true, false) => [0x4080_0000 | (ch << 16) | (note << 8), 0, 0, 0],
        }
    }

    /// What the sink holds at the end of its stream. An off for a key it does
    /// not hold ends nothing.
    fn held_at_end(words: &[Words]) -> std::collections::BTreeSet<(u32, u32, u32)> {
        let mut held = std::collections::BTreeSet::new();
        for w in words {
            let t = w[0] >> 28;
            let status = (w[0] >> 20) & 0xf;
            let key = (t, (w[0] >> 16) & 0xf, (w[0] >> 8) & 0x7f);
            match status {
                0x9 => {
                    held.insert(key);
                }
                0x8 => {
                    held.remove(&key);
                }
                _ => {}
            }
        }
        held
    }

    fn run(mode: Mode, lag: bool, steps: &[Step]) -> Vec<(u64, u64, Words)> {
        let sinks = [2u64, 3, 4];
        let mut r = Rig::with_lag(mode, &sinks, lag);
        let mut source_holds = [false; 6];
        for s in steps {
            match s {
                Step::Block => r.block(),
                Step::Toggle(k) => {
                    let on = !source_holds[*k as usize];
                    source_holds[*k as usize] = on;
                    r.send(key_words(*k, on));
                }
                Step::Wire(mask) => {
                    let edges: Vec<(u64, u64)> = sinks
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask >> i & 1 == 1)
                        .map(|(_, &s)| (1, s))
                        .collect();
                    r.wire(&edges);
                }
                Step::Replace => {
                    // A new unit starts with nothing held; its mail is empty
                    // too (the old unit's pending sends die with it).
                    r.mail.lock().unwrap().clear();
                    r.replace_source();
                    source_holds = [false; 6];
                }
            }
        }
        // Whatever is still wired is cut at the end.
        r.wire(&[]);
        r.block();
        r.block();
        r.log.lock().unwrap().clone()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

        /// Every sink ends with nothing held, and serial, parallel and
        /// reference agree, with and without a PDC delay.
        #[test]
        fn sinks_end_holding_nothing(steps in steps(), lag in any::<bool>()) {
            let serial = run(Mode::Serial, lag, &steps);
            let parallel = run(Mode::Parallel, lag, &steps);
            let reference = run(Mode::Reference, lag, &steps);
            prop_assert_eq!(by_sink(&serial), by_sink(&parallel));
            prop_assert_eq!(by_sink(&serial), by_sink(&reference));
            for sink in [2u64, 3, 4] {
                let held = held_at_end(&sink_words(&serial, sink));
                prop_assert!(held.is_empty(), "sink {} holds {:?}", sink, held);
            }
        }
    }
}

mod no_alloc {
    use super::*;
    use assert_no_alloc::AllocDisabler;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[global_allocator]
    static A: AllocDisabler = AllocDisabler;

    struct Count(Arc<AtomicU64>);
    impl Node for Count {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
            self.0
                .fetch_add(io.events(0).len() as u64, Ordering::Relaxed);
            Status::Silent
        }
        fn reset(&mut self) {}
    }

    /// Dense note traffic through tracked edges allocates nothing, serially
    /// and on a pool whose every participant is inside the gate.
    ///
    /// Mutation: allocate a `Vec` in `Held::observe` -> the gate aborts.
    fn run(workers: usize) {
        let prep = common::prepare(MAX);
        let (mut ed, mut exec) = Editor::with_event_capacity(prep, 64);
        if workers > 1 {
            exec.set_pool(Some(TestPool::no_alloc(workers)));
        }
        let mail: Mail = Arc::new(Mutex::new(Vec::with_capacity(64)));
        let n = Arc::new(AtomicU64::new(0));
        ed.insert(NodeKey(1), "src", Unforkable(Src { mail: mail.clone() }));
        for s in [2u64, 3] {
            ed.insert(NodeKey(s), "c", Unforkable(Count(n.clone())));
            ed.spec_mut()
                .connect_events(at(s), EventEdge::Direct(out(1)));
        }
        ed.commit().unwrap();
        exec.apply_pending();
        ed.collect();
        assert_eq!(exec.is_parallel(), workers > 1);
        let t = Transport::default();
        let mut tog = false;
        for _ in 0..8 {
            exec.process(MAX, &t, &[], &mut []);
        }
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..3000u32 {
                {
                    let mut m = mail.lock().unwrap();
                    let key = i % 40;
                    tog = !tog;
                    let w = if tog { 0x2090_0000 } else { 0x2080_0000 } | ((key + 20) << 8) | 100;
                    m.push((0, [w, 0, 0, 0]));
                }
                exec.process(MAX, &t, &[], &mut []);
            }
        });
        assert!(n.load(Ordering::Relaxed) > 5000);
    }

    #[test]
    fn serial() {
        run(1);
    }
    #[test]
    fn parallel() {
        run(2);
    }
}
