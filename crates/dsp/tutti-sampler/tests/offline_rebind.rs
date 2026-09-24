//! An offline render must not hear the live playhead — nor steal from it.
//!
//! Rebinding is a per-node duty (`AudioUnit::rebind_offline`), which is why the
//! guards live here beside the implementations rather than in the exporter.
//!
//! The shape matters as much as the assertions. The alternative — one free
//! function walking the net and downcasting to each type it knows — silently
//! skips any node the ladder forgot, and `MemorySource` and `DiskVoice` are
//! exactly that shape: `AudioUnit` graph nodes holding a transport, easy to miss
//! because the obvious two are `VoicePool` and `VoiceNode`. The third test pins
//! that gap closed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{AudioUnit, Beat, Bpm, SampleRate, Timeline, Wave};
use tutti_nodes::testing::Const;
use tutti_sampler::{
    Direction, LoopSetting, MemorySource, Playback, SlotId, Voice, VoiceCommand, VoiceNode,
    VoicePool, VoiceSource,
};

struct MockTransport {
    playing: AtomicBool,
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl MockTransport {
    fn new(playing: bool) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(playing),
            beat: AtomicU64::new(0.0f64.to_bits()),
            tempo: AtomicU64::new(120.0f64.to_bits()),
        })
    }
}

impl Timeline for MockTransport {
    fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    fn beat(&self) -> Beat {
        Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }
}

fn ramp_wave() -> Arc<Wave> {
    Arc::new(Wave::from_samples(
        44100.0,
        &(0..64).map(|i| (i as f32 + 1.0) / 64.0).collect::<Vec<_>>(),
    ))
}

/// `isolate()` must leave the render's pool born empty and channel-less, and
/// must not consume commands the LIVE pool needs — each command is delivered to
/// exactly one consumer, so a shared channel means the worker steals playback.
#[test]
fn an_isolated_pool_steals_no_commands_from_the_live_one() {
    let live_transport = MockTransport::new(true);
    let (pool, handle) = VoicePool::with_transport(live_transport.clone(), None);

    let mut net = tutti_core::dsp::Net::new(0, 2);
    let id = net.push(Box::new(pool));
    net.pipe_output(id);

    // Clone + isolate + rebind, exactly as a render does.
    let offline = MockTransport::new(true) as Arc<dyn Timeline>;
    let mut clone = net.clone();
    for nid in clone.ids().copied().collect::<Vec<_>>() {
        let node = clone.node_mut(nid);
        node.isolate();
        node.rebind_offline(&offline.clone());
    }

    let cloned = clone
        .node_mut(id)
        .as_any_mut()
        .downcast_mut::<VoicePool>()
        .expect("still a voice pool after rebind");
    assert_eq!(
        cloned.voice_count(),
        0,
        "the render's pool must be born empty"
    );

    // The live handle still feeds the ORIGINAL pool.
    let sampler =
        MemorySource::with_transport(ramp_wave(), live_transport.clone(), Beat::new(0.0), None);
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    net.set_sample_rate(SampleRate(44100.0));
    net.allocate();
    let live = net
        .node_mut(id)
        .as_any_mut()
        .downcast_mut::<VoicePool>()
        .unwrap();
    let mut out = [0.0f32; 2];
    live.tick(&[], &mut out); // drains the live channel
    assert_eq!(
        live.voice_count(),
        1,
        "the live pool must still receive its own commands"
    );

    let cloned = clone
        .node_mut(id)
        .as_any_mut()
        .downcast_mut::<VoicePool>()
        .unwrap();
    let mut out_clone = [0.0f32; 2];
    cloned.tick(&[], &mut out_clone);
    assert_eq!(
        cloned.voice_count(),
        0,
        "the render's pool must never receive live commands"
    );
}

/// A bare `VoiceNode` must be rebound too.
///
/// Stated behaviourally rather than structurally: the live transport rolls and
/// the offline one is stopped, so a correctly rebound node renders exact
/// silence while the un-rebound original renders audio. Asserting "a clock
/// lives in this field and was swapped" would pass vacuously the day the
/// position derives from the playhead instead.
#[test]
fn a_rebound_voice_node_reads_the_offline_clock_not_the_live_one() {
    let live_transport = MockTransport::new(true);
    let sampler =
        MemorySource::with_transport(ramp_wave(), live_transport.clone(), Beat::new(0.0), None);
    assert!(
        sampler.window_position().is_some(),
        "sanity: the source reads a live position before rebind"
    );

    let voice = Voice {
        source: VoiceSource::Memory(sampler),
        play: Playback {
            loop_: LoopSetting::Off,
            direction: Direction::Forward,
            ..Playback::default()
        },
        channel_index: None,
    };

    let mut net = tutti_core::dsp::Net::new(0, 2);
    let id = net.push(Box::new(VoiceNode::from(voice)));
    net.pipe_output(id);

    // The offline transport is STOPPED, so a rebound node must go silent.
    let offline = MockTransport::new(false) as Arc<dyn Timeline>;
    let mut clone = net.clone();
    for nid in clone.ids().copied().collect::<Vec<_>>() {
        let node = clone.node_mut(nid);
        node.isolate();
        node.rebind_offline(&offline.clone());
    }

    let peak = |net: &mut tutti_core::dsp::Net| {
        net.reset();
        net.set_sample_rate(SampleRate(44_100.0));
        let mut worst = 0.0f32;
        let mut frame = [0.0f32; 2];
        for _ in 0..64 {
            net.tick(&[], &mut frame);
            worst = worst.max(frame[0].abs()).max(frame[1].abs());
        }
        worst
    };

    let live_peak = peak(&mut net.clone());
    let rebound_peak = peak(&mut clone);
    assert!(
        live_peak > 1e-6,
        "sanity: the un-rebound net must render audio from the rolling live \
         clock, else this comparison proves nothing (peak {live_peak})"
    );
    assert_eq!(
        rebound_peak, 0.0,
        "the rebound net must render silence against the stopped offline clock; \
         it rendered {rebound_peak} (live peak {live_peak})"
    );
}

/// The gap the predecessor had: a **bare** `MemorySource` sitting directly in
/// the graph, wrapped in neither a pool nor a voice node.
///
/// `rebind_net_transport` matched `VoicePool` and `VoiceNode` only, so this node
/// kept the live transport and rendered against a playhead the offline driver
/// never advanced — silently, with no error and no compile failure. Declaring
/// the rebind on the node itself is what closes it.
#[test]
fn a_bare_memory_source_node_is_rebound_too() {
    let live_transport = MockTransport::new(true);
    let source =
        MemorySource::with_transport(ramp_wave(), live_transport.clone(), Beat::new(0.0), None);
    assert!(
        source.window_position().is_some(),
        "sanity: bound to the rolling live clock before rebind"
    );

    let mut net = tutti_core::dsp::Net::new(0, 1);
    let id = net.push(Box::new(source));
    net.pipe_output(id);

    let offline = MockTransport::new(false) as Arc<dyn Timeline>;
    let node = net.node_mut(id);
    node.isolate();
    node.rebind_offline(&offline);

    let rebound = net
        .node_mut(id)
        .as_any_mut()
        .downcast_mut::<MemorySource>()
        .expect("still a memory source");
    assert!(
        rebound.window_position().is_none(),
        "a bare MemorySource must be re-seated on the STOPPED offline clock — \
         the predecessor's type ladder skipped this node entirely"
    );
}

/// A node that reads no transport must be left alone, and an unrecognised
/// context must be ignored rather than panicking: `rebind_offline` takes
/// `&dyn Any`, so a wrong-typed context is a runtime possibility the default
/// and every impl must tolerate.
#[test]
fn pure_dsp_and_foreign_contexts_are_no_ops() {
    let mut net = tutti_core::dsp::Net::new(0, 1);
    let id = net.push(Box::new(Const::mono(0.5)));
    net.pipe_output(id);

    let offline = MockTransport::new(false) as Arc<dyn Timeline>;
    net.node_mut(id).rebind_offline(&offline);
    // A foreign context must not panic anywhere.
    net.node_mut(id).rebind_offline(&42u32);

    net.set_sample_rate(SampleRate(44_100.0));
    net.allocate();
    let mut frame = [0.0f32; 1];
    net.tick(&[], &mut frame);
    assert!(
        (frame[0] - 0.5).abs() < 1e-6,
        "a pure-DSP node must be unaffected by rebinding, got {}",
        frame[0]
    );
}

/// **The rebind must reach into nested networks.**
///
/// `Net` implements `AudioUnit`, so a sub-graph can be pushed as a single node.
/// Before `Net` forwarded `isolate`/`rebind_offline` to its vertices, such a
/// node inherited the do-nothing defaults and everything inside it kept the live
/// transport: an export of a bus whose contents are a sub-net rendered against a
/// playhead nothing advances.
///
/// The failure is silent — no value to compare, no error — which is exactly what
/// the per-node design was meant to eliminate. It only eliminates it if the walk
/// is deep.
#[test]
fn a_voice_nested_inside_a_sub_net_is_rebound_too() {
    let live_transport = MockTransport::new(true);
    let sampler =
        MemorySource::with_transport(ramp_wave(), live_transport.clone(), Beat::new(0.0), None);

    let voice = Voice {
        source: VoiceSource::Memory(sampler),
        play: Playback {
            loop_: LoopSetting::Off,
            direction: Direction::Forward,
            ..Playback::default()
        },
        channel_index: None,
    };

    // The voice lives one level down, inside a Net used as a node.
    let mut inner = tutti_core::dsp::Net::new(0, 2);
    let vid = inner.push(Box::new(VoiceNode::from(voice)));
    inner.pipe_output(vid);

    let mut outer = tutti_core::dsp::Net::new(0, 2);
    let nested = outer.push(Box::new(inner));
    outer.pipe_output(nested);

    // Offline transport is STOPPED: a rebound voice must fall silent.
    let offline = MockTransport::new(false) as Arc<dyn Timeline>;
    let mut clone = outer.clone();
    for nid in clone.ids().copied().collect::<Vec<_>>() {
        let node = clone.node_mut(nid);
        node.isolate();
        node.rebind_offline(&offline.clone());
    }

    let peak = |net: &mut tutti_core::dsp::Net| {
        net.reset();
        net.set_sample_rate(SampleRate(44_100.0));
        let mut worst = 0.0f32;
        let mut frame = [0.0f32; 2];
        for _ in 0..64 {
            net.tick(&[], &mut frame);
            worst = worst.max(frame[0].abs()).max(frame[1].abs());
        }
        worst
    };

    let live_peak = peak(&mut outer.clone());
    let rebound_peak = peak(&mut clone);

    assert!(
        live_peak > 0.0,
        "sanity: the nested voice must sound on a rolling live clock"
    );
    assert_eq!(
        rebound_peak, 0.0,
        "a voice one level down must follow the offline clock; it read the live \
         one instead (live {live_peak}, rebound {rebound_peak})"
    );
}
