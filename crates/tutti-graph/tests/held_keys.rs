//! Keys held across an edge change: the sink that loses an edge is sent the
//! offs of what the edge's source holds. A sink that gains an edge is told
//! nothing; the later off of a note it never heard begin reaches it.
//!
//! Each scenario runs on the serial executor, on the parallel one and on the
//! reference interpreter, and the three must log the same events.

use tutti_graph::testing as common;

use std::sync::{Arc, Mutex};

use common::TestPool;
use tutti_fixtures::midi::{self, MID_VELOCITY, m1_cc, m1_note_off, m1_note_on};
use tutti_graph::NodeSpec;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, Fade, ForkByClone,
    ForkMode, ForkTarget, Io, Node, Offset, Prepare, Reference, Shape, Shapes, Status, Transport,
    Unforkable,
};
use tutti_graph::{NodeKey, Topology};
use tutti_types::{ChannelLayout, Samples};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1};

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
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut m = self.mail.lock().unwrap();
        for (_, w) in m.drain(..) {
            // A unit fading out has detached writers: its pushes are refused.
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, Ump(w)));
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
        Shape::source_unassigned(ChannelLayout::MONO)
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
        Shape::sink_unassigned(a).with_events(1, 0)
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

/// A MIDI 1.0 Note On, group 0, velocity 100.
fn m1_on(ch: u8, note: u8) -> Words {
    m1_note_on(MidiChannel::of(ch), NoteNumber::of(note), U7::of(100)).0
}

/// A MIDI 1.0 Note Off, group 0, velocity 0.
fn m1_off(ch: u8, note: u8) -> Words {
    m1_note_off(MidiChannel::of(ch), NoteNumber::of(note), U7::of(0)).0
}

/// A MIDI 1.0 Note On at velocity 0, which MIDI 1.0 reads as an off.
fn m1_on_vel0(ch: u8, note: u8) -> Words {
    m1_note_on(MidiChannel::of(ch), NoteNumber::of(note), U7::of(0)).0
}

/// A MIDI 2.0 Note On, group 0, the midpoint velocity.
fn m2_on(ch: u8, note: u8) -> Words {
    midi::note_on(MidiGroup::FIRST, MidiChannel::of(ch), note, MID_VELOCITY).0
}

/// A MIDI 2.0 Note Off, group 0, velocity 0.
fn m2_off(ch: u8, note: u8) -> Words {
    midi::note_off(MidiGroup::FIRST, MidiChannel::of(ch), note, 0).0
}

fn on60() -> Words {
    m1_on(0, 60)
}
fn off60() -> Words {
    m1_off(0, 60)
}
fn on61() -> Words {
    m1_on(0, 61)
}
fn off61() -> Words {
    m1_off(0, 61)
}

/// A MIDI 1.0 Control Change on channel 0, group 0.
fn m1_controller(controller: CCNumber, value: u8) -> Words {
    m1_cc(MidiChannel::FIRST, controller, U7::of(value)).0
}

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
    /// Every edge `wire` sets is a feedback edge of this delay.
    feedback: Option<Samples>,
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
            feedback: None,
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
                        tutti_graph::InPort {
                            node: NodeKey(s),
                            port: 0,
                        },
                        vec![tutti_graph::Edge::Direct(tutti_graph::Source::Node(
                            tutti_graph::OutPort {
                                node: NodeKey(LAG),
                                port: 0,
                            },
                        ))],
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

    fn edge(&self, from: u64) -> EventEdge {
        match self.feedback {
            Some(delay) => EventEdge::feedback(out(from), delay),
            None => EventEdge::Direct(out(from)),
        }
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
                        NodeSpec::new("n", sh.audio_in(), sh.audio_out())
                            .with_latency(sh.latency.samples())
                            .with_tail(sh.tail),
                    );
                    shapes.insert(NodeKey(k), sh);
                }
                if self.lag {
                    for &s in &self.sinks {
                        t.edges.insert(
                            tutti_graph::InPort {
                                node: NodeKey(s),
                                port: 0,
                            },
                            vec![tutti_graph::Edge::Direct(tutti_graph::Source::Node(
                                tutti_graph::OutPort {
                                    node: NodeKey(LAG),
                                    port: 0,
                                },
                            ))],
                        );
                    }
                }
                let mut g = tutti_graph::GraphSpec::new(t);
                for &(f, to) in edges {
                    g.connect_events(at(to), self.edge(f));
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
                let feedback = self.feedback;
                let edge = |f| match feedback {
                    Some(delay) => EventEdge::feedback(out(f), delay),
                    None => EventEdge::Direct(out(f)),
                };
                let ed = self.ed.as_mut().unwrap();
                let spec = ed.spec_mut();
                spec.events.clear();
                for &(f, to) in edges {
                    spec.connect_events(at(to), edge(f));
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
            r.send(on60());
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(off60());
            r.block();
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![on60(), off60()],
        "A: note, then its close"
    );
    assert_eq!(
        sink_words(&logs[0], 3),
        vec![off60()],
        "B hears the orphan off, which ends nothing"
    );
}

/// A cut with no new edge (`unroute`).
#[test]
fn unroute_closes_the_sink() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(on60());
            r.send(on61());
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![on60(), on61(), off60(), off61()]
    );
}

/// A fan-out: only the sink that loses the edge is closed; the other keeps
/// its note and hears the real off.
#[test]
fn fan_out_closes_only_the_cut_sink() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2), (1, 3)]);
            r.send(on60());
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(off60());
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), off60()]);
    assert_eq!(sink_words(&logs[0], 3), vec![on60(), off60()]);
    let a = logs[0].iter().find(|e| e.0 == 2 && e.2 == off60()).unwrap();
    let b = logs[0].iter().find(|e| e.0 == 3 && e.2 == off60()).unwrap();
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
            r.send(on60());
            r.block();
            r.wire(&[(1, 2), (1, 3)]);
            r.send(off60());
            r.send(on60());
            r.block();
            r.send(off60());
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![on60(), off60(), on60(), off60()]
    );
    assert_eq!(
        sink_words(&logs[0], 3),
        vec![off60(), on60(), off60()],
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
            r.send(on60());
            r.block();
            r.send(off60());
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(
        sink_words(&logs[0], 2),
        vec![on60(), off60()],
        "nothing held at the cut: nothing sent"
    );
}

/// MIDI 1.0 spells an off as a Note On at velocity 0; MIDI 2.0 does not.
#[test]
fn velocity_zero_is_an_off_in_midi1_and_an_on_in_midi2() {
    let (m1_on, m1_on0) = (m1_on(1, 0x40), m1_on_vel0(1, 0x40));
    let m2_on0 = midi::note_on(MidiGroup::FIRST, MidiChannel::of(2), 0x41, 0).0;
    let m2_on = m2_on(1, 0x40);
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
        &[m2_off(1, 0x40), m2_off(2, 0x41)],
        "M1 on then velocity 0 holds nothing; the two M2 notes are closed as M2 offs"
    );
}

/// Group and channel are part of the key.
#[test]
fn group_and_channel_are_part_of_the_key() {
    // Group 1, channel 2: the group is the only thing the fixtures' MIDI 1.0
    // builders fix, so this one is built as the message.
    let address = Address {
        group: MidiGroup::of(1),
        channel: MidiChannel::of(2),
    };
    let voice = |v| Ump::from(Message::Midi1(address, v)).0;
    let key = NoteNumber::of(60);
    let g1c2 = voice(Voice1::NoteOn {
        note: key,
        velocity: U7::of(100),
    });
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(on60());
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
        &[
            off60(),
            voice(Voice1::NoteOff {
                note: key,
                velocity: U7::of(0)
            })
        ]
    );
}

/// A note released under a down pedal sounds on at the sink: the cut sends
/// the pedal up.
#[test]
fn a_held_sustain_pedal_is_released() {
    let down = m1_controller(CCNumber::SUSTAIN, 127);
    let up = m1_controller(CCNumber::SUSTAIN, 0);
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(on60());
            r.send(down);
            r.send(off60());
            r.block();
            r.wire(&[]);
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), down, off60(), up]);
}

/// The source replaced by a new unit: the sink is closed for the old one's
/// notes, and the new unit's later off is not the old note's.
#[test]
fn a_replaced_source_closes_its_sinks() {
    let logs = on_all(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(on60());
            r.block();
            r.replace_source();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), off60()]);
}

/// A removed source closes the sinks it fed (executor only).
#[test]
fn a_removed_source_closes_its_sinks() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.send(on60());
    r.block();
    r.remove_source();
    r.block();
    assert_eq!(r.words(2), vec![on60(), off60()]);
}

/// A sink that goes away with the edge: nothing to send, and nothing breaks.
#[test]
fn a_removed_sink_is_not_sent_anything() {
    let mut r = Rig::new(Mode::Serial, &[2, 3]);
    r.wire(&[(1, 2), (1, 3)]);
    r.send(on60());
    r.block();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.remove(NodeKey(2));
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    r.send(off60());
    r.block();
    assert_eq!(r.words(2), vec![on60()]);
    assert_eq!(r.words(3), vec![on60(), off60()]);
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
            r.send(on60());
            r.block();
            r.wire(&[]);
            r.block();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), off60()]);
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
            r.send(on60());
            r.send(off60());
            r.block();
            r.wire(&[]);
            r.block();
            r.block();
        },
        &[2],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), off60()]);
}

/// A new edge behind a delay passes the orphan off through it.
#[test]
fn a_new_delayed_edge_passes_the_orphan_off() {
    let logs = on_all_lagged(
        |r| {
            r.wire(&[(1, 2)]);
            r.send(on60());
            r.block();
            r.wire(&[(1, 3)]);
            r.block();
            r.send(off60());
            r.block();
            r.block();
            r.block();
        },
        &[2, 3],
    );
    same(&logs);
    assert_eq!(sink_words(&logs[0], 3), vec![off60()]);
    assert_eq!(sink_words(&logs[0], 2), vec![on60(), off60()]);
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
    r.send(on60());
    mail2.lock().unwrap().push((0, on61()));
    r.block();
    both(&mut r, &[(5, 2)]);
    r.block();
    assert_eq!(
        r.words(2),
        vec![on60(), on61(), off60()],
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
    r.send(on60());
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
    mail2.lock().unwrap().push((0, on61()));
    r.block();
    r.block();
    r.block();
    assert_eq!(r.words(2), vec![on60(), off60(), on61()]);
}

/// Two sources hold the same key into one sink and one is cut: the sink
/// cannot tell whose note a key is, so the close ends the other's note too.
/// The limit is documented in `held.rs`; this pins it.
#[test]
fn fan_in_on_one_key_is_closed_by_either_source() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    let mail2: Mail = Arc::default();
    r.ed.as_mut().unwrap().insert(
        NodeKey(5),
        "src2",
        Unforkable(Src {
            mail: mail2.clone(),
        }),
    );
    let wire = |r: &mut Rig, edges: &[(u64, u64)]| {
        let ed = r.ed.as_mut().unwrap();
        ed.spec_mut().events.clear();
        for &(f, to) in edges {
            ed.spec_mut()
                .connect_events(at(to), EventEdge::Direct(out(f)));
        }
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    };
    wire(&mut r, &[(1, 2), (5, 2)]);
    r.send(on60());
    mail2.lock().unwrap().push((0, on60()));
    r.block();
    wire(&mut r, &[(5, 2)]);
    r.block();
    assert_eq!(r.words(2), vec![on60(), on60(), off60()]);
}

/// A re-prepare checks the units out and back; the tables go with them, so a
/// key held across it is still closed.
#[test]
fn a_key_held_across_a_reprepare_is_closed() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.send(on60());
    r.block();
    let (ed, exec) = (r.ed.as_mut().unwrap(), r.exec.as_mut().unwrap());
    ed.reprepare(common::prepare(MAX * 2)).expect("reprepares");
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    r.block();
    r.wire(&[]);
    r.block();
    assert_eq!(r.words(2), vec![on60(), off60()]);
}

/// A feedback edge that is cut: the event in flight is delivered, and the
/// close follows it.
#[test]
fn a_cut_feedback_edge_delivers_what_is_in_flight_then_closes() {
    let logs: Vec<_> = MODES
        .iter()
        .map(|&m| {
            let mut r = Rig::new(m, &[2]);
            r.feedback = Some(Samples(MAX * 4));
            r.wire(&[(1, 2)]);
            r.send(on60());
            r.block();
            assert!(r.words(2).is_empty(), "the note is still in the FIFO");
            r.wire(&[]);
            r.block();
            let words = r.words(2);
            assert_eq!(words, vec![on60(), off60()], "{m:?}");
            r.log.lock().unwrap().clone()
        })
        .collect();
    same(&logs);
}

/// A replace with a fade while another fade runs: the first replacement
/// keeps writing until the second takes over, and its keys are closed then,
/// at once for none of them. The reference interpreter does not model a
/// crossfade, so this runs on the serial and the parallel executor.
#[test]
fn a_second_fade_closes_the_first_replacements_keys_when_it_takes_over() {
    for mode in [Mode::Serial, Mode::Parallel] {
        let mut r = Rig::new(mode, &[2, 3]);
        r.wire(&[(1, 2), (1, 3)]);
        r.block();
        let fade = Fade::new(Samples(MAX * 3), Default::default());
        let swap = |r: &mut Rig, mail: &Mail| {
            let ed = r.ed.as_mut().unwrap();
            ed.replace(NodeKey(1), Unforkable(Src { mail: mail.clone() }), fade)
                .expect("replaces");
            ed.commit().unwrap();
            r.exec.as_mut().unwrap().apply_pending();
            ed.collect();
        };
        let (m2, m3): (Mail, Mail) = Default::default();
        swap(&mut r, &m2);
        m2.lock().unwrap().push((0, on60()));
        r.block();
        swap(&mut r, &m3);
        m2.lock().unwrap().push((0, on61()));
        r.block();
        for sink in [2, 3] {
            assert_eq!(
                r.words(sink),
                vec![on60(), on61()],
                "{mode:?}: still sounding"
            );
        }
        for _ in 0..8 {
            r.block();
        }
        for sink in [2, 3] {
            assert_eq!(
                r.words(sink),
                vec![on60(), on61(), off60(), off61()],
                "{mode:?}: closed once the second fade took over"
            );
        }
        // The new unit's own notes are its own.
        m3.lock().unwrap().push((0, m1_on(0, 0x3e)));
        r.block();
        r.wire(&[]);
        r.block();
        assert_eq!(
            r.words(2).last(),
            Some(&m1_off(0, 0x3e)),
            "{mode:?}: and closed at its edge"
        );
    }
}

/// A third replace while one waits: the waiting unit never runs, and the
/// running one's keys are closed when the last takes over.
#[test]
fn a_third_fade_supersedes_the_waiting_one() {
    for mode in [Mode::Serial, Mode::Parallel] {
        let mut r = Rig::new(mode, &[2, 3]);
        r.wire(&[(1, 2), (1, 3)]);
        r.block();
        let fade = Fade::new(Samples(MAX * 3), Default::default());
        let swap = |r: &mut Rig, mail: &Mail| {
            let ed = r.ed.as_mut().unwrap();
            ed.replace(NodeKey(1), Unforkable(Src { mail: mail.clone() }), fade)
                .expect("replaces");
            ed.commit().unwrap();
            r.exec.as_mut().unwrap().apply_pending();
            ed.collect();
        };
        let (m2, m3, m4): (Mail, Mail, Mail) = Default::default();
        swap(&mut r, &m2);
        m2.lock().unwrap().push((0, on60()));
        r.block();
        swap(&mut r, &m3);
        r.block();
        swap(&mut r, &m4);
        m2.lock().unwrap().push((0, on61()));
        m3.lock().unwrap().push((0, m1_on(0, 0x40)));
        for _ in 0..10 {
            r.block();
        }
        for sink in [2, 3] {
            assert_eq!(
                r.words(sink),
                vec![on60(), on61(), off60(), off61()],
                "{mode:?}: the waiting unit never ran"
            );
        }
        assert_eq!(m3.lock().unwrap().len(), 1, "{mode:?}: and was not called");
    }
}

fn swap_with_fade(r: &mut Rig, mail: &Mail, frames: usize) {
    let ed = r.ed.as_mut().unwrap();
    ed.replace(
        NodeKey(1),
        Unforkable(Src { mail: mail.clone() }),
        Fade::new(Samples(frames), Default::default()),
    )
    .expect("replaces");
    ed.commit().unwrap();
    r.exec.as_mut().unwrap().apply_pending();
    ed.collect();
}

/// A re-prepare with a fade queued: the running replacement is cut, the
/// queued unit takes over, and the keys of the one that stopped are still
/// closed, in the first block after the resume.
#[test]
fn a_reprepare_with_a_fade_queued_closes_the_stopped_units_keys() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.block();
    let (m2, m3): (Mail, Mail) = Default::default();
    swap_with_fade(&mut r, &m2, MAX * 8);
    m2.lock().unwrap().push((0, on60()));
    r.block();
    swap_with_fade(&mut r, &m3, MAX * 8);
    r.block();
    assert_eq!(r.words(2), vec![on60()], "still sounding");
    let (ed, exec) = (r.ed.as_mut().unwrap(), r.exec.as_mut().unwrap());
    ed.reprepare(common::prepare(MAX * 2)).expect("reprepares");
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    for _ in 0..6 {
        r.block();
    }
    assert_eq!(r.words(2), vec![on60(), off60()], "closed after the resume");
    r.wire(&[]);
    r.block();
    assert_eq!(
        r.words(2),
        vec![on60(), off60()],
        "and nothing more is owed"
    );
}

/// More keys to close than a block's slot takes: the rest close in the
/// following blocks and none is lost. (The sink's own merge sheds controllers
/// first when the block is dense.)
#[test]
fn closes_that_do_not_fit_a_block_follow_in_the_next() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.block();
    let (m2, m3): (Mail, Mail) = Default::default();
    swap_with_fade(&mut r, &m2, MAX * 8);
    // 200 keys held by the running replacement, 50 a block.
    for block in 0..4u32 {
        for note in 0..50u32 {
            let w = m1_on(block as u8, note as u8);
            m2.lock().unwrap().push((0, w));
        }
        r.block();
        if block == 0 {
            swap_with_fade(&mut r, &m3, MAX * 8);
        }
    }
    // The queued unit takes over; it writes a dense block of controllers
    // every block, leaving the slot little room for closes.
    let dense = m1_controller(CCNumber::VOLUME, 100);
    for _ in 0..20 {
        for _ in 0..64 {
            m3.lock().unwrap().push((0, dense));
        }
        r.block();
    }
    let offs = r
        .words(2)
        .iter()
        .filter(|w| (w[0] >> 20) & 0xf == 0x8)
        .count();
    assert_eq!(offs, 200, "every held key is closed");
}

/// A third fade queues while closes are still owed: the owed keys stay
/// owed, the new unit's table starts empty, and every key is closed exactly
/// once.
#[test]
fn a_third_fade_leaves_the_owed_remainder_owed() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.block();
    let (m2, m3, m4): (Mail, Mail, Mail) = Default::default();
    swap_with_fade(&mut r, &m2, MAX * 8);
    for block in 0..4u32 {
        for note in 0..50u32 {
            let w = m1_on(block as u8, note as u8);
            m2.lock().unwrap().push((0, w));
        }
        r.block();
        if block == 0 {
            swap_with_fade(&mut r, &m3, MAX * 8);
        }
    }
    let dense = m1_controller(CCNumber::VOLUME, 100);
    let offs = |r: &Rig| {
        r.words(2)
            .into_iter()
            .filter(|w| (w[0] >> 20) & 0xf == 0x8)
            .collect::<Vec<_>>()
    };
    let mut blocks = 0;
    while offs(&r).is_empty() {
        for _ in 0..64 {
            m3.lock().unwrap().push((0, dense));
        }
        r.block();
        blocks += 1;
        assert!(blocks < 20, "the first fade ends");
    }
    // The remainder is still owed; the running unit holds a key of its own,
    // and a third fade queues behind it.
    assert!(offs(&r).len() < 200, "a remainder is pending");
    m3.lock().unwrap().push((0, m1_on(7, 0x63)));
    swap_with_fade(&mut r, &m4, MAX * 8);
    for _ in 0..40 {
        for _ in 0..64 {
            m3.lock().unwrap().push((0, dense));
        }
        r.block();
    }
    let closed = offs(&r);
    let mut distinct = closed.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(closed.len(), 201, "200 owed keys and the running unit's");
    assert_eq!(distinct.len(), 201, "each closed once");
}

/// A replace with a fade whose old unit holds a key, then a hard replace of
/// the same key: the running unit's keys are closed at the hard replace.
#[test]
fn a_hard_replace_during_a_fade_closes_the_running_units_keys() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.block();
    let (m2, m3): (Mail, Mail) = Default::default();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.replace(
            NodeKey(1),
            Unforkable(Src { mail: m2.clone() }),
            Fade::new(Samples(MAX * 6), Default::default()),
        )
        .unwrap();
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    m2.lock().unwrap().push((0, on60()));
    r.block();
    {
        let ed = r.ed.as_mut().unwrap();
        ed.replace(
            NodeKey(1),
            Unforkable(Src { mail: m3.clone() }),
            Fade::new(Samples(0), Default::default()),
        )
        .unwrap();
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    r.block();
    assert_eq!(r.words(2), vec![on60(), off60()]);
}

/// The first replacement holds `on60()` under a fade of `fade_blocks` blocks; a second waits
/// behind it. `then` runs `j` blocks after the second is queued (the first
/// fade ends, and the second takes over, in the course of these, so `j` up to
/// `fade_blocks + 4` crosses the promotion), with the
/// sinks `sinks` fed by the source.
fn around_promotion(
    mode: Mode,
    sinks: &[u64],
    fade_blocks: usize,
    j: usize,
    then: impl FnOnce(&mut Rig),
) -> Rig {
    let mut r = Rig::new(mode, sinks);
    let edges: Vec<(u64, u64)> = sinks.iter().map(|&s| (1, s)).collect();
    r.wire(&edges);
    r.block();
    let (m2, m3): (Mail, Mail) = Default::default();
    swap_with_fade(&mut r, &m2, MAX * fade_blocks);
    m2.lock().unwrap().push((0, on60()));
    r.block();
    swap_with_fade(&mut r, &m3, MAX * fade_blocks);
    for _ in 0..j {
        r.block();
    }
    then(&mut r);
    // Long enough for the promotion and the owed closes after any `j`.
    for _ in 0..fade_blocks + 4 {
        r.block();
    }
    r
}

/// Run `act` at every offset that crosses the promotion, for fades of 2, 3
/// and 5 blocks, on the serial and the parallel executor, and `check` each.
fn sweep(sinks: &[u64], act: impl Fn(&mut Rig), check: impl Fn(&Rig, &str)) {
    for mode in [Mode::Serial, Mode::Parallel] {
        for fade in [2, 3, 5] {
            for j in 0..fade + 4 {
                let r = around_promotion(mode, sinks, fade, j, &act);
                check(&r, &format!("{j} blocks on, {fade}-block fade, {mode:?}"));
            }
        }
    }
}

/// Removing the source at every block offset around the promotion: the
/// sink hears the note end exactly once. Before the promotion the running
/// unit's table closes at the commit; after it, what the unit owed does.
#[test]
fn removing_the_source_around_a_promotion_closes_once() {
    sweep(
        &[2],
        |r| r.remove_source(),
        |r, label| {
            assert_eq!(r.words(2), vec![on60(), off60()], "removed {label}");
        },
    );
}

/// A hard replace at every offset around the promotion.
#[test]
fn a_hard_replace_around_a_promotion_closes_once() {
    sweep(
        &[2],
        |r| {
            let ed = r.ed.as_mut().unwrap();
            ed.replace(
                NodeKey(1),
                Unforkable(Src {
                    mail: Arc::default(),
                }),
                Fade::new(Samples(0), Default::default()),
            )
            .unwrap();
            ed.commit().unwrap();
            r.exec.as_mut().unwrap().apply_pending();
            ed.collect();
        },
        |r, label| {
            assert_eq!(r.words(2), vec![on60(), off60()], "replaced {label}");
        },
    );
}

/// One of two sinks is cut at every offset around the promotion: it hears
/// the note end once, whether the cut or the owed close reaches it, and the
/// other, still wired, hears it once too.
#[test]
fn cutting_an_edge_around_a_promotion_closes_once() {
    sweep(
        &[2, 3],
        |r| r.wire(&[(1, 3)]),
        |r, label| {
            for sink in [2, 3] {
                assert_eq!(
                    r.words(sink),
                    vec![on60(), off60()],
                    "sink {sink}, cut {label}"
                );
            }
        },
    );
}

/// The resume commit of a re-prepare replaces the held source: the old
/// unit's tables are closed, not handed to the new one.
#[test]
fn a_source_replaced_by_the_resume_commit_is_closed() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.send(on60());
    r.block();
    let (ed, exec) = (r.ed.as_mut().unwrap(), r.exec.as_mut().unwrap());
    ed.replace(
        NodeKey(1),
        Unforkable(Src {
            mail: Arc::default(),
        }),
        Fade::new(Samples(0), Default::default()),
    )
    .unwrap();
    ed.reprepare(common::prepare(MAX * 2)).expect("reprepares");
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    r.block();
    assert_eq!(r.words(2), vec![on60(), off60()]);
}

/// The source and its sink are both replaced as the edge is cut: the new
/// sink never heard the note, so it is sent no close.
#[test]
fn a_replaced_sink_is_not_sent_a_close() {
    let mut r = Rig::new(Mode::Serial, &[2]);
    r.wire(&[(1, 2)]);
    r.send(on60());
    r.block();
    {
        let log = r.log.clone();
        let ed = r.ed.as_mut().unwrap();
        ed.replace(
            NodeKey(2),
            Unforkable(Sink {
                id: 22,
                log,
                audio: false,
            }),
            Fade::new(Samples(0), Default::default()),
        )
        .unwrap();
        ed.spec_mut().events.clear();
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    }
    r.block();
    assert_eq!(r.words(2), vec![on60()]);
    assert!(r.words(22).is_empty(), "the new sink hears nothing");
}

/// The closes owed on a delayed edge past what its FIFO holds are counted as
/// lost, not dropped silently.
#[test]
fn closes_the_delay_cannot_hold_are_counted() {
    let mut r = Rig::with_lag(Mode::Serial, &[2], true);
    r.wire(&[(1, 2)]);
    // Distinct held keys, a block's worth at a time, more than the FIFO of
    // the delayed edge can queue.
    for block in 0..8u32 {
        for k in 0..64u32 {
            let (note, ch) = (k, block);
            r.send(m1_on(ch as u8, note as u8));
        }
        r.block();
    }
    let before = r.exec.as_ref().unwrap().lost_event_count();
    assert_eq!(before, 0, "no event was lost before the replace");
    r.replace_source();
    r.block();
    r.block();
    r.block();
    let lost = r.exec.as_ref().unwrap().lost_event_count();
    assert!(lost > 0, "the closes the FIFO could not hold are counted");
}

/// A source that declares no note events keeps no table and closes nothing.
#[test]
fn a_source_without_note_events_closes_nothing() {
    struct Clock;
    impl Node for Clock {
        fn shape(&self) -> Shape {
            Shape::EMPTY.with_events(0, 1).with_no_note_events()
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            let _ = io.event_out(0).push(Event::midi(
                Offset::ZERO,
                Ump(m1_controller(CCNumber::VOLUME, 100)),
            ));
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let mut r = Rig::new(Mode::Serial, &[2]);
    let edit = |r: &mut Rig, wired: bool| {
        let ed = r.ed.as_mut().unwrap();
        if wired {
            ed.insert(NodeKey(6), "clock", Unforkable(Clock));
            ed.spec_mut()
                .connect_events(at(2), EventEdge::Direct(out(6)));
        } else {
            ed.spec_mut().events.clear();
        }
        ed.commit().unwrap();
        r.exec.as_mut().unwrap().apply_pending();
        ed.collect();
    };
    edit(&mut r, true);
    r.block();
    edit(&mut r, false);
    r.block();
    assert_eq!(r.words(2), vec![m1_controller(CCNumber::VOLUME, 100)]);
}

/// A node that declared no note events and writes one breaks its declaration:
/// a debug build says so, instead of leaving the note unclosed.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "declared no note events")]
fn a_note_from_a_source_that_declared_none_is_caught() {
    struct Liar;
    impl Node for Liar {
        fn shape(&self) -> Shape {
            Shape::EMPTY.with_events(0, 1).with_no_note_events()
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, Ump(on60())));
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX));
    ed.insert(NodeKey(1), "liar", Unforkable(Liar));
    ed.commit().unwrap();
    exec.apply_pending();
    exec.process(MAX, &Transport::default(), &[], &mut []);
}

/// A shape claims note events unless it says otherwise, so a node that
/// forgets the declaration is tracked, not left unclosed.
#[test]
fn a_shape_carries_note_events_by_default() {
    let shape = Shape::EMPTY.with_events(0, 1);
    assert!(shape.note_events);
    assert!(!shape.with_no_note_events().note_events);
}

/// Fork fixtures: a source that sends `on60()` once, and a sink that logs. A
/// clone of the source starts as the given flag says: unsent, or already
/// mid-note (its table in the fork is still empty).
struct Once(bool, bool);
impl Clone for Once {
    fn clone(&self) -> Self {
        Once(self.1, self.1)
    }
}
impl Node for Once {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if !std::mem::replace(&mut self.0, true) {
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, Ump(on60())));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
#[derive(Clone)]
struct Hear(Log, u64);
impl Node for Hear {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((self.1, cx.env.frame_at(e.offset).get(), w));
            }
        }
        io.output(0).fill(0.0);
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A live graph of `Once` into `Hear`, forked after `blocks` live blocks; the
/// fork's source is mid-note if `mid_note`.
fn forked(log: &Log, blocks: usize, mid_note: bool) -> (Editor, Executor) {
    let prep = common::prepare(MAX);
    let (mut ed, mut exec) = Editor::new(prep);
    ed.insert(NodeKey(1), "once", ForkByClone(Once(false, mid_note)));
    ed.insert(NodeKey(2), "hear", ForkByClone(Hear(log.clone(), 2)));
    ed.spec_mut()
        .connect_events(at(2), EventEdge::Direct(out(1)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![tutti_graph::Source::Node(tutti_graph::OutPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().unwrap();
    exec.apply_pending();
    ed.collect();
    let mut out_buf = [0.0f32; MAX];
    for _ in 0..blocks {
        exec.process(MAX, &Transport::default(), &[], &mut [&mut out_buf[..]]);
    }
    ed.fork(ForkTarget::Master, ForkMode::Offline, prep)
        .expect("forks")
}

/// A fork closes what it has seen itself: the live graph has not run, so the
/// fork's source sends the note and its edge cut closes it.
#[test]
fn a_fork_closes_what_it_has_seen() {
    let log: Log = Arc::default();
    let (mut fed, mut fexec) = forked(&log, 0, false);
    let mut out_buf = [0.0f32; MAX];
    let t = Transport::default();
    fexec.process(MAX, &t, &[], &mut [&mut out_buf[..]]);
    assert_eq!(sink_words(&log.lock().unwrap(), 2), vec![on60()]);
    fed.spec_mut().events.clear();
    fed.commit().unwrap();
    fexec.apply_pending();
    fed.collect();
    fexec.process(MAX, &t, &[], &mut [&mut out_buf[..]]);
    assert_eq!(sink_words(&log.lock().unwrap(), 2), vec![on60(), off60()]);
}

/// A key held when the graph is forked: the fork starts with empty tables, so
/// cutting the edge in the fork sends its sink nothing for it. The documented
/// limit: a fork closes only what it has seen itself.
///
/// It pins an absence. A fork that copied the live
/// tables would log an `off60()` and fail it, which is the change to make if
/// the limit is ever lifted.
#[test]
fn a_fork_does_not_close_a_key_held_at_fork_time() {
    let log: Log = Arc::default();
    let (mut fed, mut fexec) = forked(&log, 1, true);
    assert_eq!(
        sink_words(&log.lock().unwrap(), 2),
        vec![on60()],
        "live heard it"
    );
    let mut out_buf = [0.0f32; MAX];
    let t = Transport::default();
    fexec.process(MAX, &t, &[], &mut [&mut out_buf[..]]);
    fed.spec_mut().events.clear();
    fed.commit().unwrap();
    fexec.apply_pending();
    fed.collect();
    fexec.process(MAX, &t, &[], &mut [&mut out_buf[..]]);
    assert_eq!(
        sink_words(&log.lock().unwrap(), 2),
        vec![on60()],
        "the fork sent nothing for the key it never saw start"
    );
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
            (false, true) => m1_on(ch as u8, note as u8),
            (false, false) => m1_off(ch as u8, note as u8),
            (true, true) => m2_on(ch as u8, note as u8),
            (true, false) => m2_off(ch as u8, note as u8),
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

    type Key = (u32, u32, u32);

    fn key_of(w: &Words) -> Key {
        (w[0] >> 28, (w[0] >> 16) & 0xf, (w[0] >> 8) & 0x7f)
    }

    fn is_on(w: &Words) -> bool {
        (w[0] >> 20) & 0xf == 0x9
    }

    /// What the source wrote, by key.
    #[derive(Default)]
    struct Sent {
        ons: std::collections::BTreeMap<Key, u32>,
        offs: std::collections::BTreeMap<Key, u32>,
    }

    impl Sent {
        fn count(&mut self, w: &Words, by: i32) {
            let map = if is_on(w) {
                &mut self.ons
            } else {
                &mut self.offs
            };
            let n = map.entry(key_of(w)).or_default();
            *n = n.checked_add_signed(by).expect("counts what was sent");
        }
    }

    /// A sink's stream against what the source wrote: it hears no on the
    /// source did not write, and an off for a key it does not hold (an orphan
    /// off, or the close of a note that began before its edge) is for a key
    /// the source wrote something for.
    fn audit(words: &[Words], sent: &Sent) -> Result<(), String> {
        let mut held = std::collections::BTreeSet::new();
        let mut ons: std::collections::BTreeMap<Key, u32> = Default::default();
        let mut orphans: std::collections::BTreeMap<Key, u32> = Default::default();
        for w in words {
            let key = key_of(w);
            if is_on(w) {
                held.insert(key);
                *ons.entry(key).or_default() += 1;
            } else if !held.remove(&key) {
                *orphans.entry(key).or_default() += 1;
            }
        }
        for (k, n) in &ons {
            let wrote = sent.ons.get(k).copied().unwrap_or(0);
            if *n > wrote {
                return Err(format!("{k:?}: heard {n} ons, the source wrote {wrote}"));
            }
        }
        for k in orphans.keys() {
            if !sent.ons.contains_key(k) && !sent.offs.contains_key(k) {
                return Err(format!("{k:?}: an off for a key the source never wrote"));
            }
        }
        Ok(())
    }

    fn run(mode: Mode, lag: bool, steps: &[Step]) -> (Vec<(u64, u64, Words)>, Sent) {
        let sinks = [2u64, 3, 4];
        let mut r = Rig::with_lag(mode, &sinks, lag);
        let mut source_holds = [false; 6];
        let mut sent = Sent::default();
        for s in steps {
            match s {
                Step::Block => r.block(),
                Step::Toggle(k) => {
                    let on = !source_holds[*k as usize];
                    source_holds[*k as usize] = on;
                    let w = key_words(*k, on);
                    sent.count(&w, 1);
                    r.send(w);
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
                    for (_, w) in r.mail.lock().unwrap().drain(..) {
                        sent.count(&w, -1);
                    }
                    r.replace_source();
                    source_holds = [false; 6];
                }
            }
        }
        // Whatever is still wired is cut at the end.
        r.wire(&[]);
        r.block();
        r.block();
        let log = r.log.lock().unwrap().clone();
        (log, sent)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

        /// Every sink ends with nothing held, hears no on the source did not
        /// write, and no off for a key the source never wrote. Serial,
        /// parallel and reference agree, with and without a PDC delay. (The
        /// reference does not model a crossfade, so no fade is in this walk.)
        #[test]
        fn sinks_end_holding_nothing(steps in steps(), lag in any::<bool>()) {
            let (serial, sent) = run(Mode::Serial, lag, &steps);
            let (parallel, _) = run(Mode::Parallel, lag, &steps);
            let (reference, _) = run(Mode::Reference, lag, &steps);
            prop_assert_eq!(by_sink(&serial), by_sink(&parallel));
            prop_assert_eq!(by_sink(&serial), by_sink(&reference));
            for sink in [2u64, 3, 4] {
                let held = held_at_end(&sink_words(&serial, sink));
                prop_assert!(held.is_empty(), "sink {} holds {:?}", sink, held);
                let audited = audit(&sink_words(&serial, sink), &sent);
                prop_assert!(audited.is_ok(), "sink {}: {:?}", sink, audited);
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
            Shape::EMPTY.with_events(1, 0)
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
    fn run(workers: usize) {
        let prep = common::prepare(MAX);
        let (mut ed, mut exec) = Editor::with_event_capacity(prep, 64);
        if workers > 1 {
            exec.set_pool(Some(TestPool::no_alloc(workers)));
        }
        let mail: Mail = Arc::new(Mutex::new(Vec::with_capacity(64)));
        // A std mutex can box its lock on first use (it does on macOS): take it
        // once here, since the pool gates the warm-up blocks too.
        drop(mail.lock().unwrap());
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
                    let note = (key + 20) as u8;
                    // An off that carries velocity 100 as well, as the on does.
                    let vel = U7::of(100);
                    let (ch, key) = (MidiChannel::FIRST, NoteNumber::of(note));
                    m.push((
                        0,
                        if tog {
                            m1_note_on(ch, key, vel)
                        } else {
                            m1_note_off(ch, key, vel)
                        }
                        .0,
                    ));
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
