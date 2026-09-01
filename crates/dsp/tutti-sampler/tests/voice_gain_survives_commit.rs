//! **A voice's gain is addressable by `Net::set`, and survives a commit.**
//!
//! `tutti-nodes`' `live_value_survives_commit` pins the *rule* — a live control
//! value lives in shared storage or the next commit discards the write. This
//! pins the sampler's compliance with it, through the door a host actually uses.
//!
//! # What is actually being asserted
//!
//! 1. **`VoiceNode` implements `set` at all.** `AudioUnit::set` has an empty
//!    default body, so a unit that does not implement it swallows every setting
//!    in silence. Before this, `Net::set` could not address a voice, and the
//!    sampler grew a parallel setter vocabulary (`apply_gain` and its siblings,
//!    all `pub(crate)`) that a host outside the crate could not reach at all.
//!
//! 2. **The setting reaches the copy that renders**, across a `commit`.
//!
//! # What is NOT being asserted, though an earlier draft claimed it was
//!
//! That the *shared gain cell* is what makes this work. It is not, on this path,
//! and the correction is worth recording because the claim is plausible and
//! wrong:
//!
//! Reverting `MemorySource::gain` to an unshared clone leaves **every test in
//! this file passing** — measured, not assumed. A `VoiceNode` renders through
//! `slot.voice.play.gain` (see `PlaybackSlot::tick_frame_into`), a plain `Copy`
//! field on the `Playback` record, and never consults the source's own cell
//! here. And `Net::set` with a backend attached *enqueues* to the audio thread
//! rather than mutating the frontend, so the frontend-clone hazard the
//! live-value rule is about does not arise on this path at all.
//!
//! The shared cell still matters — for `node_as_mut` writes, for a `VoicePool`
//! slot, and for the offline render — it is simply not what these tests
//! discriminate. `tutti-nodes`' `live_value_survives_commit` and this crate's
//! `a_gain_change_reaches_a_cloned_source` are where that property is pinned.
//!
//! Sabotages that DO fail this file: deleting the `UnitParam::Volume` arm, and
//! writing only the source without `play.gain`. Both were run.
//!
//! # Why the assertion is on rendered audio
//!
//! Reading back through `node_as` would report every write as landed — it reads
//! `self.vertex`, the **frontend**, which is the copy being written. The
//! frontend and backend differ only in what they render. `live_value_survives_commit`
//! records that this exact mistake produced a passing test that measured
//! nothing.
//!
//! A voice generates its own signal (0 inputs), so the sibling trap in that file
//! — a fixture with no input rendering silence — does not apply here. The
//! `a_voice_at_unity_renders_its_wave` guard below is what proves the probe can
//! see anything at all, so a regression to silence fails loudly rather than
//! passing every comparison.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::{Amplitude, Beat, Bpm, Timeline, UnitParam, Wave};
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};

/// A transport that is always rolling at beat 0.
///
/// A `MemorySource` reads through `window_position`, which answers `None`
/// without a timeline — so a voice with no clock renders **silence**, and every
/// gain comparison in this file would compare 0.0 against 0.0. That is not
/// hypothetical: the first version of this fixture had no timeline and all four
/// tests failed on the guard below, which is exactly what it is for.
struct RollingTransport {
    playing: AtomicBool,
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl RollingTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(true),
            beat: AtomicU64::new(0.0f64.to_bits()),
            tempo: AtomicU64::new(120.0f64.to_bits()),
        })
    }
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
}

/// A wave whose every frame is 1.0, so a rendered sample *is* the gain.
///
/// Deliberately flat rather than a ramp: the test compares a rendered peak
/// against an expected gain directly, and a ramp would make that comparison
/// depend on which frame the read landed on.
fn flat_wave(len: usize) -> Arc<Wave> {
    let samples: Vec<f32> = vec![1.0; len];
    Arc::new(Wave::from_samples(48_000.0, &samples))
}

/// A net holding one playing voice node, with a **live backend**.
///
/// `Net::new` alone has no backend and applies settings in place, which would
/// make every assertion here pass for the wrong reason — the hazard is entirely
/// about what happens to a *frontend* clone.
fn net_with_voice() -> (Net, Box<dyn AudioUnit>, tutti_core::NodeId) {
    let mut source = MemorySource::new(flat_wave(4_096));
    // The clock is mandatory, not decoration — see `RollingTransport`. The
    // default window starts at beat 0 and runs to the end of the source, so a
    // rolling transport at beat 0 is inside it.
    source.replace_transport(RollingTransport::new());
    source.play();

    let voice = Voice {
        source: VoiceSource::Memory(source),
        play: Playback::default(),
        channel_index: None,
    };
    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(VoiceNode::with_channels(
        voice,
        tutti_core::ChannelLayout::MONO,
    )));
    net.pipe_output(node);
    net.check();

    // **Hold** the backend. `Net::with_backend` drops it, and a dropped backend
    // never renders — so `migrate` would never run and the whole point of the
    // test would be lost.
    let backend = Box::new(net.backend()) as Box<dyn AudioUnit>;
    (net, backend, node)
}

/// Peak output over a short block — the only vantage point from which the
/// frontend's copy and the rendering copy differ.
fn render_peak(backend: &mut Box<dyn AudioUnit>) -> f32 {
    let mut peak = 0.0f32;
    let mut out = [0.0f32; 1];
    for _ in 0..64 {
        backend.tick(&[], &mut out);
        peak = peak.max(out[0].abs());
    }
    peak
}

/// **The probe can see the wave at all.**
///
/// The guard against the whole file passing for the wrong reason. If a fixture
/// change ever leaves the voice gated, seeking or otherwise silent, every gain
/// comparison below would hold trivially — `0.0` is within tolerance of nothing
/// in particular, but a test asserting "the gain is 0.25" against silence would
/// still fail, whereas one asserting "it changed" would not. This makes the
/// baseline explicit.
#[test]
fn a_voice_at_unity_renders_its_wave() {
    let (_net, mut backend, _node) = net_with_voice();
    let peak = render_peak(&mut backend);
    assert!(
        (peak - 1.0).abs() < 1e-4,
        "the fixture must render its flat 1.0 wave at unity before any gain \
         assertion means anything; got {peak}. Silence here means the voice is \
         gated or not playing, and every other test in this file is vacuous."
    );
}

/// **`Net::set` reaches a live voice's gain, and the value survives the commit.**
///
/// The claim this file exists to make, through the production path: a host
/// addresses a node by `NodeId` + `UnitParam`, exactly as it does a filter or a
/// mixer strip, with no knowledge that the node is a sampler voice.
#[test]
fn a_setting_reaches_a_live_voices_gain() {
    let (mut net, mut backend, node) = net_with_voice();

    // Establish the baseline through the *backend*, so a failure below cannot
    // be blamed on the voice never having sounded.
    assert!(
        (render_peak(&mut backend) - 1.0).abs() < 1e-4,
        "unity before the edit"
    );

    net.set(tutti_core::unit_param::node_setting(
        node,
        UnitParam::Volume,
        0.25,
    ));
    net.commit();

    // A few blocks: `commit` only *sends* the new net; `migrate` runs when the
    // backend next processes, and the setting queue drains there too.
    let peak = render_peak(&mut backend);

    assert!(
        (peak - 0.25).abs() < 1e-3,
        "a Volume setting must reach the voice that renders; expected ~0.25, \
         got {peak}. A value near 1.0 means either `VoiceNode` has no \
         `UnitParam::Volume` arm (the setting was swallowed by `AudioUnit::set`'s \
         empty default) or the gain is stored by value and the write landed on a \
         discarded clone."
    );
}

/// **The `Playback` record moves with the source.**
///
/// Not redundant with the render assertion, and the reason is a bug this
/// codebase already fixed once. `Playback` is the control-*intent* record that
/// the offline render and the pool read back; the source holds what the DSP
/// reads. Writing only the source leaves two copies disagreeing, and a rebind
/// then restores the stale one — which is exactly the hazard
/// `Playback.placement` was **deleted** for, recorded in `voice/types.rs`:
///
/// > *Two clocks kept in sync by hand, one of them never consulted, is a rebind
/// > that can silently reach the wrong one.*
///
/// Gain cannot be deleted the same way (the record is what a spawn reads), so
/// the two are written together instead.
///
/// # Why this one uses a net with NO backend
///
/// Every other test here needs a backend, because the hazard they pin is what
/// happens to a frontend *clone*. This one needs the opposite, and the reason is
/// worth stating because it looks like an inconsistency:
///
/// **`Net::set` never touches the frontend when a backend is attached.** It
/// enqueues the setting to the audio thread (`net.rs`'s `if let Some((sender,
/// _)) = &mut self.front`), so the write is applied on the backend's copy and
/// the frontend's `Playback` stays at its old value forever. Reading the
/// frontend after a backend-attached `set` therefore observes nothing — which
/// is what the first version of this test did, and it failed against working
/// code.
///
/// Without a backend, `set` takes the `else` branch and applies in place, so
/// both halves of the write are visible on the one copy that exists. That is the
/// only vantage point from which "did `set` write *both*?" is answerable at all.
#[test]
fn a_setting_updates_the_playback_record_too() {
    let mut source = MemorySource::new(flat_wave(4_096));
    source.replace_transport(RollingTransport::new());
    source.play();
    let voice = Voice {
        source: VoiceSource::Memory(source),
        play: Playback::default(),
        channel_index: None,
    };

    // No backend: `set` applies in place — see the doc above.
    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(VoiceNode::with_channels(
        voice,
        tutti_core::ChannelLayout::MONO,
    )));
    net.pipe_output(node);
    net.check();

    net.set(tutti_core::unit_param::node_setting(
        node,
        UnitParam::Volume,
        0.25,
    ));

    let gain = net
        .node_as::<VoiceNode>(node)
        .expect("still a VoiceNode")
        .voice()
        .play
        .gain;
    assert_eq!(
        gain,
        Amplitude(0.25),
        "the Playback record must follow the source, or a rebind restores the \
         old gain"
    );
}

/// **A param the voice does not own is ignored, not misapplied.**
///
/// The convention that lets a host push a setting without dispatching on node
/// type. Worth pinning here because the failure mode is silent in the other
/// direction too: an arm added for a control that is still stored *by value*
/// would look like this test passing while changing nothing on a live node.
#[test]
fn an_unowned_param_is_ignored() {
    let (mut net, mut backend, node) = net_with_voice();

    net.set(tutti_core::unit_param::node_setting(
        node,
        UnitParam::Pan,
        0.0,
    ));
    net.commit();

    let peak = render_peak(&mut backend);
    assert!(
        (peak - 1.0).abs() < 1e-4,
        "a Pan setting must leave a voice's gain alone; got {peak}"
    );
}
