//! A plugin's per-block status in the plugin node (doc 023 step 2): CLAP's
//! *sleep* is the node's `report_tail(Tail::None)` and `Status::Idle`, and
//! nothing else narrows or parks.
//!
//! The plugin is `tutti_plugin_types::fake`'s: a voice that sounds `ring`
//! frames after each trigger (a non-silent input frame, a MIDI event at its
//! frame) and says what CLAP's process status would, hosted through
//! `client_over` as a served plugin is. No format and no server is involved.

use std::sync::Arc;

use tutti_fixtures::main_thread;

use tutti_fixtures::midi;
use tutti_graph::contract::{self, assert_honest_tail, drive_input};
use tutti_graph::{
    Editor, Env, EventCapacity, EventEdge, EventIn, EventInletNode, EventOut, Executor, Node,
    NodeKey, OutPort, PlayClock, Prepare as GraphPrepare, Source, Status, TransportChanges,
    TransportPosition, Unforkable,
};
use tutti_plugin_host::{PluginNode, client_over};
use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
use tutti_plugin_types::{Control, Loaded, Prepare};
use tutti_tempo::TempoMap;
use tutti_transport::TempoReader;
use tutti_types::{Bpm, Frame, MaxBlock, SampleRate, Samples, Tail};
use tutti_ump::{MidiChannel, U7};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

/// The fake's node: a voice of `ring` frames declaring `tail` as its ceiling.
fn node(ring: usize, tail: usize) -> (PluginNode<FakeLoaded>, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    fake.ring_out(ring);
    fake.set_tail(Samples(tail));
    let main = main_thread();
    let wiring = loaded.wiring(&main);
    let descriptor = loaded.info().clone();
    let prepare = Prepare::new(SR, MaxBlock::new(Samples(BLOCK)).expect("a block"));
    let Ok((processor, controller)) = loaded.activate(&main, prepare) else {
        panic!("the fake activates");
    };
    let client = client_over::<FakeLoaded>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        [],
        0,
        None,
    );
    let map = TempoMap::constant(Bpm::of(120.0)).expect("a tempo");
    (client.bind(TempoReader::fixed(Arc::new(map))), fake)
}

fn env(frame: u64) -> Env {
    Env {
        frame: Frame::new(frame),
        sample_rate: SR,
        block_len: Samples(BLOCK),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    }
}

/// An impulse on both input channels at the block's first frame, or silence.
fn input(impulse: bool) -> Vec<f32> {
    let mut v = vec![0.0f32; BLOCK];
    v[0] = f32::from(u8::from(impulse));
    v
}

fn drive(node: &mut dyn Node, frame: u64, impulse: bool) -> contract::Driven {
    let x = input(impulse);
    drive_input(node, &env(frame), &[&x, &x], &[], &[])
}

fn processed(fake: &FakePlugin) -> usize {
    fake.calls()
        .iter()
        .filter(|c| matches!(c, Call::Process(_)))
        .count()
}

/// **A plugin that sleeps reports no tail and idles; one that sounds says
/// nothing.** An impulse starts a 100-frame voice: blocks 0 and 1 sound and
/// leave the ceiling standing (`Modified`, nothing reported), block 2 is the
/// plugin's *sleep* (`Idle`, `Tail::None`).
#[test]
fn a_sleeping_plugin_reports_no_tail_and_idles() {
    let (node, _fake) = node(100, 128);
    let mut node = contract::prepared(node, SR, BLOCK);
    for (block, impulse) in [(0u64, true), (1, false)] {
        let d = drive(&mut node, block * BLOCK as u64, impulse);
        assert_eq!(d.status, Some(Status::Modified), "block {block} sounds");
        assert_eq!(d.reported_tail, None, "block {block}: the ceiling stands");
        assert!(d.audio[0].iter().any(|&x| x != 0.0), "block {block}");
    }
    let d = drive(&mut node, 2 * BLOCK as u64, false);
    assert_eq!(d.status, Some(Status::Idle), "asleep");
    assert_eq!(d.reported_tail, Some(Tail::None));
    assert!(d.audio.iter().flatten().all(|&x| x == 0.0));
}

/// **The honest-tail contract holds for the node over a voice of 100 frames
/// declaring a 128-frame ceiling**: after the impulse the node is silent past
/// what it reports (its ceiling, since the sounding blocks say nothing).
#[test]
fn the_plugin_node_passes_the_honest_tail_contract() {
    let (node, _fake) = node(100, 128);
    let mut node = contract::prepared(node, SR, BLOCK);
    assert_honest_tail(&mut node, SR);
}

/// **A plugin that says sleep over sound is not believed**: its audio is
/// heard, the node reports nothing and is not idle.
#[test]
fn a_plugin_that_sleeps_over_sound_is_not_idled() {
    let (node, fake) = node(100, 128);
    fake.sleep_over_sound(true);
    let mut node = contract::prepared(node, SR, BLOCK);
    let d = drive(&mut node, 0, true);
    assert_eq!(d.status, Some(Status::Modified));
    assert_eq!(d.reported_tail, None);
    assert!(d.audio[0].contains(&0.5), "the sound is heard");
}

/// A voice of 100 frames behind the executor: an event inlet into the node's
/// event input, the node's two channels to the outputs.
fn graph(fake_ring: usize) -> (Editor, Executor, tutti_graph::EventInletWriter, FakePlugin) {
    let (node, fake) = node(fake_ring, 128);
    let (mut ed, mut exec) = Editor::new(GraphPrepare::new(SR, MaxBlock::of(Samples(BLOCK))));
    let (inlet, writer) = EventInletNode::pair(EventCapacity::of(64));
    let (inlet_key, plugin) = (NodeKey(1), NodeKey(2));
    ed.insert(inlet_key, "inlet", inlet);
    ed.insert(plugin, "fake", Unforkable(node));
    ed.spec_mut().connect_events(
        EventIn {
            node: plugin,
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: inlet_key,
            port: 0,
        }),
    );
    ed.spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: plugin, port })));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, writer, fake)
}

fn render(exec: &mut Executor) -> [Vec<f32>; 2] {
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[],
        &mut [&mut l, &mut r],
    );
    [l, r]
}

fn note() -> tutti_ump::Ump {
    midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100))
}

/// **A parked plugin is woken by a note on its frame**, at a block's first
/// frame and inside a block. The plugin sleeps, so the executor parks the node
/// (its `process` stops being called); a note then lands at the first frame of
/// block 8 and, after the voice has rung out and the node parked again, at
/// frame 17 of block 20. Each is heard from exactly its frame, 100 frames of
/// 0.5, and silent before.
#[test]
fn a_parked_plugin_is_woken_by_a_note_on_its_frame() {
    let (_ed, mut exec, mut tx, fake) = graph(100);
    let mut heard = Vec::new();
    let mut calls = Vec::new();
    let (first, second) = (8 * BLOCK as u64, 20 * BLOCK as u64 + 17);
    tx.push(Frame::new(first), note()).expect("room");
    tx.push(Frame::new(second), note()).expect("room");
    for _ in 0..40 {
        let [l, r] = render(&mut exec);
        assert_eq!(l, r);
        heard.extend(l);
        calls.push(processed(&fake));
    }
    let sounding: Vec<usize> = (0..heard.len()).filter(|&i| heard[i] != 0.0).collect();
    let want: Vec<usize> = (first as usize..first as usize + 100)
        .chain(second as usize..second as usize + 100)
        .collect();
    assert_eq!(
        sounding, want,
        "each note exactly on its frame, 100 frames long"
    );
    assert!(heard.iter().all(|&x| x == 0.0 || x == 0.5));
    // Parked between: the plugin was not called for blocks 5..8 (asleep since
    // block 0, parked from block 1) nor across the quiet blocks after the
    // first note rang out (blocks 12..19).
    assert_eq!(calls[2], calls[7], "blocks 3..7 were skipped");
    assert_eq!(calls[14], calls[19], "blocks 15..19 were skipped");
    assert!(calls[8] > calls[7], "block 8 woke it");
}

/// The tail the executor parks on is the declared ceiling for a plugin that
/// never reports: a plugin node with a ceiling is not parked while its voice
/// rings, because it says *continue* then, and a sleeping one is parked at
/// once even though the ceiling is 128 frames.
#[test]
fn a_sleep_parks_before_the_ceiling_has_elapsed() {
    let (_ed, mut exec, _tx, fake) = graph(100);
    for _ in 0..10 {
        render(&mut exec);
    }
    // Called for the first block(s) only: asleep from the start, so parked
    // long before the 128-frame ceiling's two blocks could matter.
    assert!(processed(&fake) <= 2, "{} calls", processed(&fake));
}
