//! **A voice's gain is addressable through the native graph's settings ring.**
//!
//! `voice_gain_survives_commit.rs` pins the `Net::set` path: `VoiceNode::set`
//! writes `slot.voice.play.gain`, a plain `Copy` field, so the setting has to
//! reach the copy that renders. The native graph has no `Net`; a `Legacy`
//! node built with `Legacy::controlled` carries settings instead, through a
//! ring it drains into `AudioUnit::set` at the start of each block (doc 013,
//! Phase 3 PR 1). This pins that the voice's plain-field write is honoured on
//! that path: a gain change lands on the next block, as it did with `Net`.
//!
//! As in the `Net` file, the assertion is on rendered audio — the only
//! vantage point from which the rendering copy and any other copy differ —
//! and the shadow is checked separately, since it is the copy a fork clones.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::unit_param::setting;
use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples, Timeline, UnitParam};
use tutti_graph::{Delivery, Editor, Executor, Legacy, LegacyControls, Prepare, Transport};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};

/// Always rolling at beat 0: a `MemorySource` with no clock renders silence,
/// and every gain comparison would hold against 0.0 (see the `Net` file).
struct RollingTransport {
    playing: AtomicBool,
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl Timeline for RollingTransport {
    fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    fn beat(&self) -> Beat {
        Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }
    fn segment_generation(&self) -> u64 {
        0
    }
}

/// One playing voice over a flat 1.0 wave, so a rendered sample *is* the
/// gain, as a `Legacy::controlled` node at key 1 feeding output 0.
fn graph_with_voice() -> (Editor, Executor, LegacyControls<VoiceNode>) {
    let wave = Arc::new(Wave::from_samples(48_000.0, &vec![1.0f32; 48_000]));
    let mut source = MemorySource::new(wave);
    source.replace_transport(Arc::new(RollingTransport {
        playing: AtomicBool::new(true),
        beat: AtomicU64::new(0.0f64.to_bits()),
        tempo: AtomicU64::new(120.0f64.to_bits()),
    }));
    source.play();
    let voice = Voice {
        source: VoiceSource::Memory(source),
        play: Playback::default(),
        channel_index: None,
    };
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    let (node, controls) = Legacy::controlled(
        &mut ed,
        VoiceNode::with_channels(voice, tutti_core::ChannelLayout::MONO),
    );
    let key = NodeKey(1);
    ed.insert(key, "voice", node);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, controls)
}

/// Every sample of one 256-frame block.
fn render(exec: &mut Executor) -> Vec<f32> {
    let mut out = vec![0.0f32; 256];
    exec.process(256, &Transport::default(), &[], &mut [&mut out[..]]);
    out
}

/// **A Volume setting lands on the next block, whole.** Unity first, so the
/// probe demonstrably sees the wave; then every sample of the next block is
/// at the new gain, not only its peak.
///
/// Mutation: delete `VoiceNode::set`'s `play.gain` write → the block stays at
/// 1.0 → fails. Mutation: drop `Legacy::process`'s settings drain → fails.
/// Mutation: skip the shadow's `set` in `LegacyControls::set` → the shadow's
/// `play.gain` stays at unity → fails.
#[test]
fn a_gain_setting_reaches_the_voice_through_the_ring() {
    let (_ed, mut exec, mut controls) = graph_with_voice();
    let before = render(&mut exec);
    assert!(
        before.iter().all(|&x| (x - 1.0).abs() < 1e-4),
        "the fixture must render its flat wave at unity before any gain \
         assertion means anything; got {:?}",
        &before[..4]
    );

    assert_eq!(
        controls.set(setting(UnitParam::Volume, 0.25)),
        Delivery::Queued
    );
    assert!(
        (controls.shadow().voice().play.gain.get() - 0.25).abs() < 1e-6,
        "the shadow holds the by-value gain a fork would clone"
    );

    let after = render(&mut exec);
    assert!(
        after.iter().all(|&x| (x - 0.25).abs() < 1e-4),
        "every sample of the next block at the new gain; got {:?}",
        &after[..4]
    );
}
